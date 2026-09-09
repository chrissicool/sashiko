// Copyright 2026 The Sashiko Authors
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
//     https://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

use crate::ai::{AiErrorClass, AiMessage, AiProvider, ClassifyAiError};
use crate::toolbox::ToolBox;
use anyhow::Result;

/// Typed errors that must not be silently retried.
#[derive(Debug, thiserror::Error)]
pub enum ReviewError {
    /// The AI exceeded its per-review turn limit.  Retrying with the same
    /// limit will just hit the cap again — fail fast.
    #[error("Max interactions exceeded")]
    LimitExceeded,
    /// A token budget was exceeded.  Retrying wastes tokens for no gain.
    #[error("Token budget exceeded: {0}")]
    BudgetExceeded(String),
    /// The AI produced output that failed format validation.  The retry
    /// should use an augmented prompt that reminds the model of the
    /// violated constraint rather than repeating the identical request.
    #[error("Format validation failed: {0}")]
    FormatRejection(String),
    /// The AI response was truncated by the provider (e.g., hit max tokens).
    #[error("AI response truncated by provider limit")]
    OutputTruncated,
}

impl ClassifyAiError for ReviewError {
    fn ai_error_class(&self) -> AiErrorClass {
        match self {
            ReviewError::LimitExceeded => AiErrorClass::Fatal,
            ReviewError::BudgetExceeded(_) => AiErrorClass::Fatal,
            ReviewError::FormatRejection(_) => AiErrorClass::Fatal,
            ReviewError::OutputTruncated => AiErrorClass::Fatal,
        }
    }
}

use crate::worker::kernel_workflow::{
    KernelReviewState, build_kernel_review_workflow_with_options, kernel_system_prompt,
};
use crate::workflow::{WorkflowEngine, WorkflowEnv, WorkflowEvent};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::path::{Path, PathBuf};
use std::sync::Arc;

/// System identity prompt - used across all AI interactions
pub const SYSTEM_IDENTITY: &str = "";

#[derive(Deserialize, Serialize, Debug, Clone, PartialEq)]
pub struct PatchInput {
    pub index: i64,
    pub diff: String,
    pub subject: Option<String>,
    pub author: Option<String>,
    pub date: Option<i64>,
    #[serde(default)]
    pub message_id: Option<String>,
    #[serde(default)]
    pub commit_id: Option<String>,
}

#[derive(Deserialize, Serialize, Debug)]
pub struct ReviewInput {
    pub id: i64,
    pub subject: String,
    pub patches: Vec<PatchInput>,
}

pub struct WorkerConfig {
    pub max_input_tokens: usize,
    pub max_interactions: usize,
    pub temperature: f32,
    pub custom_prompt: Option<String>,
    pub series_range: Option<String>,
    pub baseline_sha: Option<String>,
    pub stages: Option<Vec<u8>>,
}

#[derive(Debug, Clone)]
pub enum WorkerProgressEvent {
    PreScreenStarted,
    PlanningStarted,
    ReviewStarted {
        planned_stages: Vec<u8>,
    },
    StageStarted {
        stage: u8,
    },
    StageFinished {
        stage: u8,
    },
    StageTurn {
        stage: u8,
        turn: usize,
        max_turns: usize,
    },
}

pub struct WorkerResult {
    pub output: Option<Value>,
    pub error: Option<String>,
    pub input_context: String,
    pub history: Vec<AiMessage>,
    pub history_before_pruning: Vec<AiMessage>,
    pub history_after_pruning: Vec<AiMessage>,
    pub tokens_in: u32,
    pub tokens_out: u32,
    pub tokens_cached: u32,
}

pub struct PromptRegistry {
    pub base_dir: PathBuf,
}

impl PromptRegistry {
    pub fn new(base_dir: PathBuf) -> Self {
        Self { base_dir }
    }

    /// Validate that the prompt directory is usable before any review stage runs.
    /// The base directory and `identity.md` must exist, and the stage files
    /// `stages/stage-{1..11}.md` plus `stages/prescreen.md` must exist; every
    /// missing file is reported together in a single error. Paths in error
    /// messages are absolute.
    pub fn validate_prompt_directory(&self) -> Result<()> {
        let abs = |p: &Path| {
            std::path::absolute(p)
                .unwrap_or_else(|_| p.to_path_buf())
                .display()
                .to_string()
        };

        if !self.base_dir.exists() {
            anyhow::bail!("Prompt directory not found: {}", abs(&self.base_dir));
        }

        let identity = self.base_dir.join("identity.md");
        if !identity.exists() {
            anyhow::bail!("Required prompt file not found: {}", abs(&identity));
        }

        let mut missing = Vec::new();
        for n in 1..=11u8 {
            let stage = self.base_dir.join(format!("stages/stage-{n}.md"));
            if !stage.exists() {
                missing.push(abs(&stage));
            }
        }
        let prescreen = self.base_dir.join("stages/prescreen.md");
        if !prescreen.exists() {
            missing.push(abs(&prescreen));
        }
        if !missing.is_empty() {
            anyhow::bail!(
                "Missing required stage prompt files:\n{}",
                missing.join("\n")
            );
        }

        Ok(())
    }
}

pub struct Worker {
    provider: Arc<dyn AiProvider>,
    tools: Arc<ToolBox>,
    prompts: PromptRegistry,
    global_history: Vec<AiMessage>,
    max_interactions: usize,
    temperature: f32,
    series_range: Option<String>,
    baseline_sha: Option<String>,
    context_tag: Option<String>,
    stages: Option<Vec<u8>>,
    custom_prompt: Option<String>,
}

impl Worker {
    pub fn new(
        provider: Arc<dyn AiProvider>,
        tools: Arc<ToolBox>,
        prompts: PromptRegistry,
        config: WorkerConfig,
    ) -> Self {
        Self {
            provider,
            tools,
            prompts,
            global_history: Vec::new(),
            max_interactions: config.max_interactions,
            temperature: config.temperature,
            series_range: config.series_range,
            baseline_sha: config.baseline_sha,
            context_tag: None,
            stages: config.stages,
            custom_prompt: config.custom_prompt,
        }
    }

    pub async fn run(
        &mut self,
        patchset: Value,
        progress: Option<&(dyn Fn(WorkerProgressEvent) + Send + Sync)>,
    ) -> Result<WorkerResult> {
        let mut target_commit_diff = String::new();
        let mut target_commit_diff_only = String::new();

        let ps_id = patchset["id"]
            .as_i64()
            .map(|id| id.to_string())
            .unwrap_or_else(|| "unknown".to_string());
        let p_id = patchset["patch_index"]
            .as_i64()
            .map(|id| id.to_string())
            .unwrap_or_else(|| "multi".to_string());
        self.context_tag = Some(format!("[ps:{} p:{}] ", ps_id, p_id));

        let baseline_sha = self
            .baseline_sha
            .clone()
            .or_else(|| {
                patchset
                    .get("baseline")
                    .and_then(|b| b.as_str())
                    .map(|s| s.to_string())
            })
            .or_else(|| {
                self.series_range.as_ref().and_then(|range| {
                    let parts: Vec<&str> = range.split("..").collect();
                    if !parts.is_empty() && !parts[0].is_empty() {
                        Some(parts[0].to_string())
                    } else {
                        None
                    }
                })
            })
            .unwrap_or_else(|| "unknown".to_string());

        let mut target_commit_sha = "unknown".to_string();
        if let Some(patches) = patchset["patches"].as_array() {
            if let Some(idx) = patchset["patch_index"].as_i64()
                && let Some(p) = patches.iter().find(|p| p["index"].as_i64() == Some(idx))
                && let Some(sha) = p["commit_id"].as_str()
            {
                target_commit_sha = sha.to_string();
            }
            if target_commit_sha == "unknown"
                && !patches.is_empty()
                && let Some(sha) = patches[0]["commit_id"].as_str()
            {
                target_commit_sha = sha.to_string();
            }
        }

        if let Some(patches) = patchset["patches"].as_array() {
            let target_patches: Vec<&Value> = if let Some(idx) = patchset["patch_index"].as_i64() {
                let filtered: Vec<&Value> = patches
                    .iter()
                    .filter(|p| p["index"].as_i64() == Some(idx))
                    .collect();
                if filtered.is_empty() {
                    patches.iter().collect()
                } else {
                    filtered
                }
            } else {
                patches.iter().collect()
            };

            for p in target_patches {
                let diff_body = p["diff"].as_str().unwrap_or("");
                let changelog_opt = crate::patch::extract_changelog_from_body(diff_body);

                if let Some(show) = p["git_show"].as_str() {
                    if let Some(ref changelog) = changelog_opt {
                        let enriched_show =
                            crate::patch::inject_changelog_into_git_show(show, changelog);
                        target_commit_diff.push_str(&enriched_show);
                    } else {
                        target_commit_diff.push_str(show);
                    }
                    target_commit_diff.push('\n');
                } else {
                    target_commit_diff.push_str(diff_body);
                    target_commit_diff.push('\n');
                }

                if let Some(diff) = p["diff"].as_str() {
                    target_commit_diff_only.push_str(diff);
                    target_commit_diff_only.push('\n');
                }
            }
        }

        let worktree_path = self.tools.get_worktree_path();
        let prefetched_context =
            crate::worker::prefetch::prefetch_context(worktree_path, &target_commit_diff)
                .await
                .unwrap_or_default();

        let follow_up_series_context = build_follow_up_series_context(
            self.series_range.as_deref(),
            &patchset,
            &target_commit_sha,
        );

        let mut state = KernelReviewState {
            ps_id,
            p_id,
            target_commit_sha,
            baseline_sha,
            target_commit_diff,
            target_commit_diff_only,
            prefetched_context,
            series_range: self.series_range.clone(),
            follow_up_series_context,
            selected_guides: Vec::new(),
            manual_stages: self.stages.clone(),
            custom_prompt: self.custom_prompt.clone(),
            planned_stages: Vec::new(),
            all_concerns: Vec::new(),
            all_dismissed_concerns: Vec::new(),
            deduplicated_concerns: Vec::new(),
            deduplicated_dismissed_concerns: Vec::new(),
            conflict_resolved_concerns: Vec::new(),
            findings: Vec::new(),
            review_inline: String::new(),
            fixes: String::new(),
        };

        if self.global_history.is_empty() {
            let sys_template = kernel_system_prompt(true);
            let rendered_sys = sys_template.render_for_log(&state);
            self.global_history.push(AiMessage {
                role: crate::ai::AiRole::System,
                content: Some(rendered_sys),
                thought: None,
                thought_signature: None,
                tool_calls: None,
                tool_call_id: None,
            });
        }

        let workflow =
            build_kernel_review_workflow_with_options(self.max_interactions, self.temperature);
        let env = WorkflowEnv {
            provider: self.provider.clone(),
            tools: self.tools.clone(),
            base_dir: &self.prompts.base_dir,
            context_tag: self.context_tag.clone(),
        };

        let event_cb = move |event: WorkflowEvent| {
            if let Some(progress_cb) = progress {
                match event {
                    WorkflowEvent::StageStarted { stage_name } => {
                        if stage_name == "stage_0_prescreen" {
                            progress_cb(WorkerProgressEvent::PreScreenStarted);
                        } else if stage_name == "stage_planning" {
                            progress_cb(WorkerProgressEvent::PlanningStarted);
                        } else if let Some(num) = parse_stage_number(stage_name) {
                            progress_cb(WorkerProgressEvent::StageStarted { stage: num });
                        }
                    }
                    WorkflowEvent::ParallelResolved { stage_names } => {
                        progress_cb(WorkerProgressEvent::ReviewStarted {
                            planned_stages: planned_stages_from(&stage_names),
                        });
                    }
                    WorkflowEvent::StageFinished { stage_name, .. } => {
                        if let Some(num) = parse_stage_number(stage_name) {
                            progress_cb(WorkerProgressEvent::StageFinished { stage: num });
                        }
                    }
                    WorkflowEvent::StageTurn {
                        stage_name,
                        turn,
                        max_turns,
                    } => {
                        if let Some(num) = parse_stage_number(stage_name) {
                            progress_cb(WorkerProgressEvent::StageTurn {
                                stage: num,
                                turn,
                                max_turns,
                            });
                        }
                    }
                    _ => {}
                }
            }
        };

        let outcome = WorkflowEngine::execute(&workflow, &env, &mut state, Some(&event_cb)).await?;
        self.global_history.extend(outcome.history.clone());

        let concerns_count = state.all_concerns.len();
        let dismissed_concerns = if !state.deduplicated_dismissed_concerns.is_empty() {
            state.deduplicated_dismissed_concerns.clone()
        } else {
            state.all_dismissed_concerns.clone()
        };
        let dismissed_concerns_count = dismissed_concerns.len();

        let review_inline = if state.review_inline.is_empty() {
            "No issues found.".to_string()
        } else {
            state.review_inline
        };

        let final_output = json!({
            "findings": state.findings,
            "dismissed_concerns": dismissed_concerns,
            "review_inline": review_inline,
            "fixes": state.fixes,
            "concerns_count": concerns_count,
            "dismissed_concerns_count": dismissed_concerns_count,
        });

        Ok(WorkerResult {
            output: Some(final_output),
            error: None,
            input_context: "Multi-stage execution completed".to_string(),
            history: self.global_history.clone(),
            history_before_pruning: self.global_history.clone(),
            history_after_pruning: self.global_history.clone(),
            tokens_in: outcome.tokens_in,
            tokens_out: outcome.tokens_out,
            tokens_cached: outcome.tokens_cached,
        })
    }
}

/// The stages a review will run: the analysis stages the fan-out resolved, then
/// the four that always follow them. Nothing resolved means nothing planned,
/// not a bare tail.
fn planned_stages_from(stage_names: &[&'static str]) -> Vec<u8> {
    let mut planned: Vec<u8> = stage_names
        .iter()
        .filter_map(|n| parse_stage_number(n))
        .collect();
    if !planned.is_empty() {
        planned.extend([8, 9, 10, 11]);
    }
    planned
}

fn parse_stage_number(name: &str) -> Option<u8> {
    if let Some(rest) = name.strip_prefix("stage_")
        && let Some(num_str) = rest.split('_').next()
    {
        return num_str.parse().ok();
    }
    None
}

pub fn calculate_series_range(
    patches: &[PatchInput],
    patches_to_review: &[PatchInput],
    patch_shas: &std::collections::HashMap<i64, String>,
    baseline_sha: &str,
) -> Option<String> {
    if patches.is_empty() {
        return None;
    }

    let max_patch_index = patches.iter().map(|p| p.index).max().unwrap_or(0);
    let is_last_patch_review =
        patches_to_review.len() == 1 && patches_to_review[0].index == max_patch_index;

    if is_last_patch_review {
        None
    } else {
        patches
            .iter()
            .map(|p| p.index)
            .max()
            .and_then(|max_idx| {
                patches
                    .iter()
                    .find(|p| p.index == max_idx)
                    .and_then(|p| p.commit_id.clone())
                    .or_else(|| patch_shas.get(&max_idx).cloned())
            })
            .map(|end_sha| format!("{}..{}", baseline_sha, end_sha))
    }
}

pub fn build_follow_up_series_context(
    series_range: Option<&str>,
    patchset: &Value,
    target_commit_sha: &str,
) -> Option<String> {
    let range = series_range?;
    let end_sha = range.split("..").nth(1)?;
    if end_sha.is_empty() {
        return None;
    }

    let current_idx = patchset["patch_index"].as_i64().unwrap_or(1);
    let patches = patchset["patches"].as_array()?;
    let total_patches = patches.len();

    let current_subject = patches
        .iter()
        .find(|p| p["index"].as_i64() == Some(current_idx))
        .and_then(|p| p["subject"].as_str())
        .unwrap_or("unknown");

    let mut follow_ups = Vec::new();
    for p in patches {
        let idx = p["index"].as_i64().unwrap_or(0);
        if idx > current_idx {
            let subj = p["subject"].as_str().unwrap_or("");
            let commit_id = p["commit_id"].as_str();
            follow_ups.push((idx, commit_id, subj));
        }
    }

    if follow_ups.is_empty() {
        return None;
    }

    follow_ups.sort_by_key(|(idx, _, _)| *idx);

    let mut block = String::new();
    block.push_str("\n\n=== Follow-Up Patches in Series ===\n");
    block.push_str(&format!(
        "Current Patch Under Review: [Patch {} of {}] - {}\n",
        current_idx, total_patches, current_subject
    ));
    block.push_str(&format!("Series End Commit (Final State): {}\n\n", end_sha));
    block.push_str("Subsequent patches in this series:\n");

    for (idx, commit_id, subj) in follow_ups {
        if let Some(sha) = commit_id {
            block.push_str(&format!(
                "- [Patch {} of {}] (commit {}): {}\n",
                idx, total_patches, sha, subj
            ));
        } else {
            block.push_str(&format!(
                "- [Patch {} of {}]: {}\n",
                idx, total_patches, subj
            ));
        }
    }

    let diff_directive = if target_commit_sha != "unknown" && !target_commit_sha.is_empty() {
        format!(
            "Use tools (e.g., git_diff with base_revision=\"{}\", target_revision=\"{}\", or git_read_files with revision=\"{}\") to inspect the final code state at the end of the series.",
            target_commit_sha, end_sha, end_sha
        )
    } else {
        format!(
            "Use tools (e.g., git_diff with target_revision=\"{}\", or git_read_files with revision=\"{}\") to inspect the final code state at the end of the series.",
            end_sha, end_sha
        )
    };

    block.push_str("\nSERIES VERIFICATION DIRECTIVE:\n");
    block.push_str(&format!(
        "Verify if any candidate concern raised against this patch is fixed, refactored, or resolved in the subsequent patches listed above. {} If a concern is resolved by follow-up patches in this series, discard it as a false positive.\n",
        diff_directive
    ));
    block.push_str("===================================\n");

    Some(block)
}

#[cfg(test)]
fn append_stage_items(
    target: &mut Vec<Value>,
    items: &[Value],
    stage: u8,
    default_type: &str,
    default_text_key: &str,
) {
    for item in items {
        if let Some(item) = normalize_stage_item(item, stage, default_type, default_text_key) {
            target.push(item);
        }
    }
}

#[cfg(test)]
fn append_stage_dismissed_concerns(target: &mut Vec<Value>, items: &[Value], stage: u8) {
    append_stage_items(target, items, stage, "General", "description");
}

#[cfg(test)]
fn normalize_stage_item(
    item: &Value,
    stage: u8,
    default_type: &str,
    default_text_key: &str,
) -> Option<Value> {
    if let Some(obj) = item.as_object() {
        let mut with_stage = obj.clone();
        with_stage.insert("source_stage".to_string(), json!(stage));
        Some(Value::Object(with_stage))
    } else {
        item.as_str().map(|s| {
            let mut obj = serde_json::Map::new();
            obj.insert("source_stage".to_string(), json!(stage));
            obj.insert("type".to_string(), json!(default_type));
            obj.insert(default_text_key.to_string(), json!(s));
            Value::Object(obj)
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_planned_stages_follow_the_resolved_fan_out() {
        assert_eq!(
            planned_stages_from(&["stage_1", "stage_2", "stage_5"]),
            vec![1, 2, 5, 8, 9, 10, 11]
        );
        assert_eq!(planned_stages_from(&[]), Vec::<u8>::new());
        assert_eq!(planned_stages_from(&["stage_planning"]), Vec::<u8>::new());
    }

    /// Populate a temp prompt directory with the minimal stub files the worker
    /// reads while running stages: an identity file and stage-1.md..stage-11.md.
    /// Seed a prompt directory with the stage files the workflow includes.
    /// The instruction text is data now, so a test that exercises prompt
    /// content has to provide it; the `# Stage N.` headers match the real
    /// files so assertions on them stay meaningful.
    fn seed_stage_files(dir: &std::path::Path) {
        let stages = dir.join("stages");
        std::fs::create_dir_all(&stages).unwrap();
        for n in 1..=11 {
            std::fs::write(
                stages.join(format!("stage-{n}.md")),
                format!("# Stage {n}. Test instruction."),
            )
            .unwrap();
        }
        std::fs::write(stages.join("prescreen.md"), "Test prescreen instruction.").unwrap();
    }

    fn write_stub_prompt_dir(dir: &std::path::Path) {
        std::fs::write(dir.join("identity.md"), "Test identity.\n").unwrap();
        let stages = dir.join("stages");
        std::fs::create_dir_all(&stages).unwrap();
        for n in 1..=11 {
            std::fs::write(
                stages.join(format!("stage-{n}.md")),
                format!("# Stage {n}\nTest stub instruction."),
            )
            .unwrap();
        }
        std::fs::write(stages.join("prescreen.md"), "Test prescreen.\n").unwrap();
    }

    #[test]
    fn test_validate_prompt_directory_complete_ok() {
        let dir = tempfile::tempdir().unwrap();
        write_stub_prompt_dir(dir.path());
        let reg = PromptRegistry::new(dir.path().to_path_buf());
        reg.validate_prompt_directory()
            .expect("complete dir is valid");
    }

    #[test]
    fn test_validate_prompt_directory_missing_path() {
        let reg = PromptRegistry::new(PathBuf::from("/nonexistent/prompt/dir/xyz"));
        let err = reg
            .validate_prompt_directory()
            .expect_err("missing dir is fatal");
        assert!(
            err.to_string().contains("/nonexistent/prompt/dir/xyz"),
            "error: {err}"
        );
    }

    #[test]
    fn test_validate_prompt_directory_missing_identity() {
        let dir = tempfile::tempdir().unwrap();
        let stages = dir.path().join("stages");
        std::fs::create_dir_all(&stages).unwrap();
        for n in 1..=11 {
            std::fs::write(stages.join(format!("stage-{n}.md")), "x").unwrap();
        }
        let reg = PromptRegistry::new(dir.path().to_path_buf());
        let err = reg
            .validate_prompt_directory()
            .expect_err("missing identity is fatal");
        assert!(err.to_string().contains("identity.md"), "error: {err}");
    }

    #[test]
    fn test_validate_prompt_directory_reports_all_missing_stages() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("identity.md"), "x").unwrap();
        let stages = dir.path().join("stages");
        std::fs::create_dir_all(&stages).unwrap();
        // Only stage 1 present; 2..=11 missing.
        std::fs::write(stages.join("stage-1.md"), "x").unwrap();
        let reg = PromptRegistry::new(dir.path().to_path_buf());
        let err = reg
            .validate_prompt_directory()
            .expect_err("missing stages are fatal");
        let msg = err.to_string();
        assert!(msg.contains("stage-2.md"), "error: {msg}");
        assert!(msg.contains("stage-11.md"), "error: {msg}");
        assert!(
            !msg.contains("stage-1.md"),
            "stage-1 present, should not be listed: {msg}"
        );
    }

    #[test]
    fn test_append_stage_dismissed_concerns_preserves_category_type() {
        let mut items = Vec::new();
        let input = vec![json!({
            "type": "Resource Management",
            "description": "suspected cross-zone page leak does not apply",
            "reasoning": "hugetlb_free_cross_zone_pages() runs before HVO init"
        })];

        append_stage_dismissed_concerns(&mut items, &input, 1);

        assert_eq!(items.len(), 1);
        assert_eq!(items[0]["source_stage"], 1);
        assert_eq!(items[0]["type"], "Resource Management");
        assert_eq!(
            items[0]["reasoning"],
            "hugetlb_free_cross_zone_pages() runs before HVO init"
        );
    }

    #[test]
    fn test_append_stage_dismissed_concerns_normalizes_string_items() {
        let mut items = Vec::new();
        let input = vec![json!("suspected missing cleanup does not apply")];

        append_stage_dismissed_concerns(&mut items, &input, 2);

        assert_eq!(items.len(), 1);
        assert_eq!(items[0]["source_stage"], 2);
        assert_eq!(items[0]["type"], "General");
        assert_eq!(
            items[0]["description"],
            "suspected missing cleanup does not apply"
        );
    }

    #[test]
    fn test_append_stage_items_overwrites_existing_source_stage() {
        let mut items = Vec::new();
        let input = vec![json!({
            "source_stage": 3,
            "type": "Execution flow",
            "description": "already annotated"
        })];

        append_stage_items(&mut items, &input, 4, "General", "description");

        assert_eq!(items.len(), 1);
        assert_eq!(items[0]["source_stage"], 4);
    }

    #[test]
    fn test_append_stage_items_normalizes_string_items() {
        let mut items = Vec::new();
        let input = vec![json!("plain concern")];

        append_stage_items(&mut items, &input, 6, "General", "description");

        assert_eq!(items.len(), 1);
        assert_eq!(items[0]["source_stage"], 6);
        assert_eq!(items[0]["type"], "General");
        assert_eq!(items[0]["description"], "plain concern");
    }

    #[test]
    fn test_calculate_series_range_single_patch() {
        let p = PatchInput {
            index: 1,
            diff: "".to_string(),
            subject: None,
            author: None,
            date: None,
            message_id: None,
            commit_id: Some("sha1".to_string()),
        };
        let patches = vec![p.clone()];
        let patches_to_review = vec![p.clone()];
        let patch_shas = std::collections::HashMap::new();

        assert_eq!(
            calculate_series_range(&patches, &patches_to_review, &patch_shas, "base"),
            None
        );
    }

    #[test]
    fn test_calculate_series_range_multi_patch_last() {
        let p1 = PatchInput {
            index: 1,
            diff: "".to_string(),
            subject: None,
            author: None,
            date: None,
            message_id: None,
            commit_id: Some("sha1".to_string()),
        };
        let p2 = PatchInput {
            index: 2,
            diff: "".to_string(),
            subject: None,
            author: None,
            date: None,
            message_id: None,
            commit_id: Some("sha2".to_string()),
        };
        let patches = vec![p1.clone(), p2.clone()];
        let patches_to_review = vec![p2.clone()]; // Reviewing last
        let patch_shas = std::collections::HashMap::new();

        assert_eq!(
            calculate_series_range(&patches, &patches_to_review, &patch_shas, "base"),
            None
        );
    }

    #[test]
    fn test_calculate_series_range_multi_patch_middle() {
        let p1 = PatchInput {
            index: 1,
            diff: "".to_string(),
            subject: None,
            author: None,
            date: None,
            message_id: None,
            commit_id: Some("sha1".to_string()),
        };
        let p2 = PatchInput {
            index: 2,
            diff: "".to_string(),
            subject: None,
            author: None,
            date: None,
            message_id: None,
            commit_id: Some("sha2".to_string()),
        };
        let patches = vec![p1.clone(), p2.clone()];
        let patches_to_review = vec![p1.clone()]; // Reviewing first
        let patch_shas = std::collections::HashMap::new();

        assert_eq!(
            calculate_series_range(&patches, &patches_to_review, &patch_shas, "base"),
            Some("base..sha2".to_string())
        );
    }

    #[test]
    fn test_calculate_series_range_use_patch_shas_map() {
        let p1 = PatchInput {
            index: 1,
            diff: "".to_string(),
            subject: None,
            author: None,
            date: None,
            message_id: None,
            commit_id: None, // Missing in input
        };
        let p2 = PatchInput {
            index: 2,
            diff: "".to_string(),
            subject: None,
            author: None,
            date: None,
            message_id: None,
            commit_id: None, // Missing in input
        };
        let patches = vec![p1.clone(), p2.clone()];
        let patches_to_review = vec![p1.clone()];

        let mut patch_shas = std::collections::HashMap::new();
        patch_shas.insert(2, "sha2_resolved".to_string());

        assert_eq!(
            calculate_series_range(&patches, &patches_to_review, &patch_shas, "base"),
            Some("base..sha2_resolved".to_string())
        );
    }

    #[test]
    fn test_build_follow_up_series_context_none_when_no_range() {
        let patchset = serde_json::json!({
            "patch_index": 1,
            "patches": [{
                "index": 1,
                "subject": "Single patch",
                "commit_id": "sha1"
            }]
        });
        assert_eq!(
            build_follow_up_series_context(None, &patchset, "sha1"),
            None
        );
    }

    #[test]
    fn test_build_follow_up_series_context_none_when_last_patch() {
        let patchset = serde_json::json!({
            "patch_index": 2,
            "patches": [
                { "index": 1, "subject": "Patch 1", "commit_id": "sha1" },
                { "index": 2, "subject": "Patch 2", "commit_id": "sha2" }
            ]
        });
        assert_eq!(
            build_follow_up_series_context(Some("base..sha2"), &patchset, "sha2"),
            None
        );
    }

    #[test]
    fn test_build_follow_up_series_context_intermediate_patch() {
        let patchset = serde_json::json!({
            "patch_index": 1,
            "patches": [
                { "index": 1, "subject": "net: add foo API", "commit_id": "sha1" },
                { "index": 2, "subject": "net: add caller for foo", "commit_id": "sha2" },
                { "index": 3, "subject": "net: add docs for foo", "commit_id": "sha3" }
            ]
        });
        let ctx = build_follow_up_series_context(Some("base..sha3"), &patchset, "sha1");
        assert!(ctx.is_some());
        let content = ctx.unwrap();
        assert!(content.contains("Current Patch Under Review: [Patch 1 of 3] - net: add foo API"));
        assert!(content.contains("Series End Commit (Final State): sha3"));
        assert!(content.contains("- [Patch 2 of 3] (commit sha2): net: add caller for foo"));
        assert!(content.contains("- [Patch 3 of 3] (commit sha3): net: add docs for foo"));
        assert!(!content.contains("- [Patch 1 of 3]"));
        assert!(content.contains("SERIES VERIFICATION DIRECTIVE:"));
        assert!(content.contains("base_revision=\"sha1\""));
        assert!(content.contains("target_revision=\"sha3\""));
        assert!(content.contains("git_read_files with revision=\"sha3\""));
    }

    #[test]
    fn test_build_follow_up_series_context_unordered_patches() {
        let patchset = serde_json::json!({
            "patch_index": 1,
            "patches": [
                { "index": 3, "subject": "Patch 3", "commit_id": "sha3" },
                { "index": 1, "subject": "Patch 1", "commit_id": "sha1" },
                { "index": 2, "subject": "Patch 2", "commit_id": "sha2" }
            ]
        });
        let ctx = build_follow_up_series_context(Some("base..sha3"), &patchset, "sha1");
        assert!(ctx.is_some());
        let content = ctx.unwrap();
        let p2_pos = content.find("Patch 2 of 3").unwrap();
        let p3_pos = content.find("Patch 3 of 3").unwrap();
        assert!(
            p2_pos < p3_pos,
            "Patch 2 should appear before Patch 3 in follow-up list"
        );
    }

    #[test]
    fn test_build_follow_up_series_context_unknown_target_sha() {
        let patchset = serde_json::json!({
            "patch_index": 1,
            "patches": [
                { "index": 1, "subject": "Patch 1" },
                { "index": 2, "subject": "Patch 2", "commit_id": "sha2" }
            ]
        });
        let ctx = build_follow_up_series_context(Some("base..sha2"), &patchset, "unknown");
        assert!(ctx.is_some());
        let content = ctx.unwrap();
        assert!(!content.contains("base_revision=\"unknown\""));
        assert!(content.contains("target_revision=\"sha2\""));
    }

    struct MockProviderAlwaysFails;
    #[async_trait::async_trait]
    impl crate::ai::AiProvider for MockProviderAlwaysFails {
        async fn generate_content(
            &self,
            _request: crate::ai::AiRequest,
        ) -> anyhow::Result<crate::ai::AiResponse> {
            anyhow::bail!("mock: simulated AI failure")
        }
        fn estimate_tokens(&self, _request: &crate::ai::AiRequest) -> usize {
            0
        }
        fn get_capabilities(&self) -> crate::ai::ProviderCapabilities {
            crate::ai::ProviderCapabilities {
                model_name: "mock".to_string(),
                context_window_size: 1000,
            }
        }
    }

    #[tokio::test]
    async fn test_stage_failure_aborts_review() {
        let temp_dir = tempfile::tempdir().unwrap();
        let prompts_dir = temp_dir.path().join("prompts");
        std::fs::create_dir_all(&prompts_dir).unwrap();
        seed_stage_files(&prompts_dir);

        let provider = std::sync::Arc::new(MockProviderAlwaysFails);
        let tools = crate::toolbox::ToolBox::new(temp_dir.path().to_path_buf(), None);
        let prompts = PromptRegistry::new(prompts_dir);
        let config = WorkerConfig {
            max_input_tokens: 10000,
            max_interactions: 3,
            temperature: 0.0,
            series_range: None,
            baseline_sha: None,
            custom_prompt: None,
            stages: None,
        };
        let mut worker = Worker::new(provider, std::sync::Arc::new(tools), prompts, config);

        let patchset = serde_json::json!({
            "id": 1,
            "patch_index": 1,
            "patches": [{"diff": "diff --git a/foo.c b/foo.c\n+int x;"}]
        });

        match worker.run(patchset, None).await {
            Ok(_) => panic!("Expected stage failure error, got Ok"),
            Err(e) => assert!(
                e.to_string().contains("simulated AI failure"),
                "unexpected error: {e}"
            ),
        }
    }

    // ReviewError tests

    #[test]
    fn test_limit_exceeded_classifies_as_fatal() {
        let err = ReviewError::LimitExceeded;

        assert_eq!(err.ai_error_class(), AiErrorClass::Fatal);
    }

    #[test]
    fn test_budget_exceeded_classifies_as_fatal() {
        let err = ReviewError::BudgetExceeded("1000 tokens used (limit: 500)".to_string());

        assert_eq!(err.ai_error_class(), AiErrorClass::Fatal);
    }

    #[test]
    fn test_format_rejection_classifies_as_fatal() {
        let err = ReviewError::FormatRejection("contains markdown code blocks".to_string());

        assert_eq!(err.ai_error_class(), AiErrorClass::Fatal);
    }

    #[test]
    fn test_limit_exceeded_downcasts_as_review_error() {
        let err: anyhow::Error = ReviewError::LimitExceeded.into();
        assert!(
            err.downcast_ref::<ReviewError>().is_some(),
            "LimitExceeded must downcast to ReviewError so the retry loop can fail fast"
        );
    }

    #[test]
    fn test_budget_exceeded_downcasts_as_review_error() {
        let err: anyhow::Error =
            ReviewError::BudgetExceeded("1000 tokens used (limit: 500)".to_string()).into();
        assert!(
            err.downcast_ref::<ReviewError>().is_some(),
            "BudgetExceeded must downcast to ReviewError so the retry loop can fail fast"
        );
    }

    #[test]
    fn test_generic_error_does_not_downcast_as_review_error() {
        let err: anyhow::Error = anyhow::anyhow!("transient JSON parse failure");
        assert!(
            err.downcast_ref::<ReviewError>().is_none(),
            "Plain anyhow errors must NOT match ReviewError so they remain retryable"
        );
    }

    #[test]
    fn test_format_rejection_downcasts_as_review_error() {
        let err: anyhow::Error =
            ReviewError::FormatRejection("contains markdown code blocks".to_string()).into();
        assert!(
            err.downcast_ref::<ReviewError>().is_some(),
            "FormatRejection must downcast to ReviewError"
        );
    }

    use std::sync::atomic::{AtomicUsize, Ordering};

    struct MockBlockedProvider {
        attempts: AtomicUsize,
    }

    #[async_trait::async_trait]
    impl crate::ai::AiProvider for MockBlockedProvider {
        async fn generate_content(
            &self,
            request: crate::ai::AiRequest,
        ) -> anyhow::Result<crate::ai::AiResponse> {
            let attempt = self.attempts.fetch_add(1, Ordering::SeqCst);
            if attempt == 0 {
                anyhow::bail!(
                    "Remote AI Error: Gemini candidate blocked (finish reason: RECITATION)"
                )
            } else {
                let has_filter = request.messages.iter().any(|m| {
                    m.role == crate::ai::AiRole::User
                        && m.content
                            .as_ref()
                            .is_some_and(|c| c.contains("recitation filter"))
                });
                if has_filter {
                    return Ok(crate::ai::AiResponse {
                        content: Some(r#"{"concerns": [], "dismissed_concerns": []}"#.to_string()),
                        thought: None,
                        thought_signature: None,
                        tool_calls: None,
                        usage: None,
                        truncated: false,
                        served_from_cache: false,
                    });
                }
                anyhow::bail!(
                    "Remote AI Error: Gemini candidate blocked again (finish reason: RECITATION)"
                )
            }
        }

        fn estimate_tokens(&self, _request: &crate::ai::AiRequest) -> usize {
            0
        }

        fn get_capabilities(&self) -> crate::ai::ProviderCapabilities {
            crate::ai::ProviderCapabilities {
                model_name: "mock".to_string(),
                context_window_size: 1000,
            }
        }
    }

    #[tokio::test]
    async fn test_recitation_error_triggers_prompt_perturbation() {
        let temp_dir = tempfile::tempdir().unwrap();
        let prompts_dir = temp_dir.path().join("prompts");
        std::fs::create_dir_all(&prompts_dir).unwrap();
        seed_stage_files(&prompts_dir);

        let provider = std::sync::Arc::new(MockBlockedProvider {
            attempts: AtomicUsize::new(0),
        });
        let tools = crate::toolbox::ToolBox::new(temp_dir.path().to_path_buf(), None);
        let prompts = PromptRegistry::new(prompts_dir);
        let config = WorkerConfig {
            max_input_tokens: 10000,
            max_interactions: 3,
            temperature: 0.0,
            series_range: None,
            baseline_sha: None,
            custom_prompt: None,
            stages: Some(vec![1]),
        };
        let mut worker = Worker::new(provider, std::sync::Arc::new(tools), prompts, config);

        let patchset = serde_json::json!({
            "id": 1,
            "patch_index": 1,
            "patches": [{"diff": "diff --git a/foo.c b/foo.c\n+int x;"}]
        });

        let res = worker.run(patchset, None).await;
        if let Err(e) = &res {
            panic!("Expected run to succeed, got error: {:?}", e);
        }
    }

    #[tokio::test]
    async fn test_baseline_sha_in_worker_context_when_single_patch() {
        let temp_dir = tempfile::tempdir().unwrap();
        let prompts_dir = temp_dir.path().join("prompts");
        std::fs::create_dir_all(&prompts_dir).unwrap();
        seed_stage_files(&prompts_dir);

        let provider = std::sync::Arc::new(MockBlockedProvider {
            attempts: AtomicUsize::new(0),
        });
        let tools = crate::toolbox::ToolBox::new(temp_dir.path().to_path_buf(), None);
        let prompts = PromptRegistry::new(prompts_dir);
        let config = WorkerConfig {
            max_input_tokens: 10000,
            max_interactions: 3,
            temperature: 0.0,
            series_range: None,
            baseline_sha: Some("explicit_baseline_sha".to_string()),
            custom_prompt: None,
            stages: Some(vec![1]),
        };
        let mut worker = Worker::new(provider, std::sync::Arc::new(tools), prompts, config);

        let patchset = serde_json::json!({
            "id": 1,
            "patch_index": 1,
            "patches": [{"diff": "diff --git a/foo.c b/foo.c\n+int x;", "commit_id": "target_sha"}]
        });

        let res = worker.run(patchset, None).await;
        assert!(res.is_ok());
        let worker_res = res.unwrap();
        assert!(!worker_res.history.is_empty());
        // System prompt contains the Active Git Metadata:
        let sys_content = worker_res.history[0].content.as_deref().unwrap_or_default();
        assert!(sys_content.contains("Baseline SHA: explicit_baseline_sha"));
        assert!(sys_content.contains("Target Commit SHA: target_sha"));
    }

    #[tokio::test]
    async fn test_multi_patch_worker_context_only_contains_target_patch_diff() {
        let temp_dir = tempfile::tempdir().unwrap();
        let prompts_dir = temp_dir.path().join("prompts");
        std::fs::create_dir_all(&prompts_dir).unwrap();
        seed_stage_files(&prompts_dir);

        let provider = std::sync::Arc::new(MockBlockedProvider {
            attempts: AtomicUsize::new(0),
        });
        let tools = crate::toolbox::ToolBox::new(temp_dir.path().to_path_buf(), None);
        let prompts = PromptRegistry::new(prompts_dir);
        let config = WorkerConfig {
            max_input_tokens: 10000,
            max_interactions: 3,
            temperature: 0.0,
            series_range: Some("base_sha..sha2".to_string()),
            baseline_sha: Some("base_sha".to_string()),
            custom_prompt: None,
            stages: Some(vec![1]),
        };
        let mut worker = Worker::new(provider, std::sync::Arc::new(tools), prompts, config);

        let patchset = serde_json::json!({
            "id": 100,
            "patch_index": 1,
            "patches": [
                {
                    "index": 1,
                    "subject": "Patch 1 Subject",
                    "diff": "diff --git a/file1.c b/file1.c\n+int patch1_unique_symbol;",
                    "commit_id": "sha1"
                },
                {
                    "index": 2,
                    "subject": "Patch 2 Subject",
                    "diff": "diff --git a/file2.c b/file2.c\n+int patch2_unique_symbol;",
                    "commit_id": "sha2"
                }
            ]
        });

        let res = worker.run(patchset, None).await;
        assert!(res.is_ok());
        let worker_res = res.unwrap();
        assert!(!worker_res.history.is_empty());
        let sys_content = worker_res.history[0].content.as_deref().unwrap_or_default();
        assert!(sys_content.contains("Target Commit SHA: sha1"));
        assert!(sys_content.contains("patch1_unique_symbol"));
        assert!(!sys_content.contains("patch2_unique_symbol"));
    }

    struct MockMultiStageSeriesProvider;

    #[async_trait::async_trait]
    impl crate::ai::AiProvider for MockMultiStageSeriesProvider {
        async fn generate_content(
            &self,
            request: crate::ai::AiRequest,
        ) -> anyhow::Result<crate::ai::AiResponse> {
            let last_user = request
                .messages
                .iter()
                .rfind(|m| m.role == crate::ai::AiRole::User)
                .and_then(|m| m.content.as_deref())
                .unwrap_or_default();

            let content = if last_user.contains("# Stage 1.") || last_user.contains("# Stage 8.") {
                r#"{"concerns": [{"type": "Bug", "description": "some issue", "reasoning": "reason", "preexisting": false, "locations": []}], "dismissed_concerns": []}"#
            } else if last_user.contains("# Stage 9.") {
                r#"{"concerns": [{"type": "Bug", "description": "some issue", "reasoning": "reason", "preexisting": false, "locations": []}]}"#
            } else if last_user.contains("# Stage 10.") {
                r#"{"findings": []}"#
            } else {
                r#"{"concerns": [], "dismissed_concerns": []}"#
            };

            Ok(crate::ai::AiResponse {
                content: Some(content.to_string()),
                thought: None,
                thought_signature: None,
                tool_calls: None,
                usage: None,
                truncated: false,
                served_from_cache: false,
            })
        }

        fn estimate_tokens(&self, _request: &crate::ai::AiRequest) -> usize {
            0
        }

        fn get_capabilities(&self) -> crate::ai::ProviderCapabilities {
            crate::ai::ProviderCapabilities {
                model_name: "mock".to_string(),
                context_window_size: 1000,
            }
        }
    }

    #[tokio::test]
    async fn test_stage_10_log_history_contains_follow_up_series_context() {
        let temp_dir = tempfile::tempdir().unwrap();
        let prompts_dir = temp_dir.path().join("prompts");
        std::fs::create_dir_all(&prompts_dir).unwrap();
        seed_stage_files(&prompts_dir);

        let provider = std::sync::Arc::new(MockMultiStageSeriesProvider);
        let tools = crate::toolbox::ToolBox::new(temp_dir.path().to_path_buf(), None);
        let prompts = PromptRegistry::new(prompts_dir);
        let config = WorkerConfig {
            max_input_tokens: 10000,
            max_interactions: 3,
            temperature: 0.0,
            series_range: Some("base_sha..sha2".to_string()),
            baseline_sha: Some("base_sha".to_string()),
            custom_prompt: None,
            stages: Some(vec![1]),
        };
        let mut worker = Worker::new(provider, std::sync::Arc::new(tools), prompts, config);

        let patchset = serde_json::json!({
            "id": 200,
            "patch_index": 1,
            "patches": [
                {
                    "index": 1,
                    "subject": "Patch 1 Subject",
                    "diff": "diff --git a/file1.c b/file1.c\n+int patch1;",
                    "commit_id": "sha1"
                },
                {
                    "index": 2,
                    "subject": "Patch 2 Subject",
                    "diff": "diff --git a/file2.c b/file2.c\n+int patch2;",
                    "commit_id": "sha2"
                }
            ]
        });

        let res = worker.run(patchset, None).await;
        assert!(res.is_ok());
        let worker_res = res.unwrap();
        assert!(!worker_res.history.is_empty());

        let stage10_user_msg = worker_res
            .history
            .iter()
            .find(|m| {
                m.role == crate::ai::AiRole::User
                    && m.content
                        .as_deref()
                        .unwrap_or_default()
                        .contains("@stages/stage-10.md")
            })
            .expect("Stage 10 user message should be in history");

        let content = stage10_user_msg.content.as_deref().unwrap();
        assert!(content.contains("=== Follow-Up Patches in Series ==="));
        assert!(content.contains("Series End Commit (Final State): sha2"));
        assert!(content.contains("- [Patch 2 of 2] (commit sha2): Patch 2 Subject"));
        assert!(content.contains("SERIES VERIFICATION DIRECTIVE:"));
    }
}
