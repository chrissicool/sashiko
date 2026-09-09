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

//! Single declarative workflow definition for Sashiko's Linux Kernel Code Review.
//!
//! This module specifies the multi-stage review pipeline as a declarative [`Workflow`]
//! operating over [`KernelReviewState`].

use std::path::PathBuf;

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::workflow::graph::Workflow;
use crate::workflow::output::OutputFormat;
use crate::workflow::policy::{ParallelPolicy, RecitationPolicy, StagePolicy, ToolScope};
use crate::workflow::prompt::PromptTemplate;
use crate::workflow::stage::{ExecutableStage, Stage};

/// Subsystem guides that are loaded per-stage and should be excluded
/// from Phase 0 shared context to avoid redundant token usage.
pub const STAGE_EXCLUSIVE_GUIDES: &[&str] = &["locking.md"];

/// Complete execution state of a Linux kernel patch review.
#[derive(Clone, Debug, Default)]
pub struct KernelReviewState {
    pub ps_id: String,
    pub p_id: String,
    pub target_commit_sha: String,
    pub baseline_sha: String,
    pub target_commit_diff: String,
    pub target_commit_diff_only: String,
    pub prefetched_context: String,
    pub series_range: Option<String>,
    pub follow_up_series_context: Option<String>,

    /// Subsystem guide markdown files selected during Phase 0 pre-screen.
    pub selected_guides: Vec<String>,
    /// Optional manual stages filter (e.g. `--stages 1,2,5`).
    pub manual_stages: Option<Vec<u8>>,
    /// Caller-supplied instructions appended to the shared system prompt.
    pub custom_prompt: Option<String>,
    /// Stages selected by dynamic planning (or overridden by manual_stages).
    pub planned_stages: Vec<u8>,

    /// Aggregated raw concerns collected from Stages 1-7.
    pub all_concerns: Vec<Value>,
    /// Aggregated raw dismissed concerns collected from Stages 1-7.
    pub all_dismissed_concerns: Vec<Value>,

    /// Deduplicated concerns from Stage 8.
    pub deduplicated_concerns: Vec<Value>,
    /// Deduplicated dismissed concerns from Stage 8.
    pub deduplicated_dismissed_concerns: Vec<Value>,

    /// Filtered concerns after Stage 9 conflict resolution.
    pub conflict_resolved_concerns: Vec<Value>,

    /// Verified findings from Stage 10.
    pub findings: Vec<Value>,

    /// Generated LKML plain-text review from Stage 11.
    pub review_inline: String,
    /// Fix suggestions.
    pub fixes: String,
}

// ---------------------------------------------------------------------------
// Typed Output Structures for Stage Serialization
// ---------------------------------------------------------------------------

#[derive(Deserialize, Serialize, Debug, Clone)]
pub struct Phase0Output {
    pub selected_prompts: Vec<String>,
}

#[derive(Deserialize, Serialize, Debug, Clone)]
pub struct PlanningOutput {
    pub relevant_stages: Vec<u8>,
}

#[derive(Deserialize, Serialize, Debug, Clone, Default)]
pub struct StageConcernsOutput {
    #[serde(default)]
    pub concerns: Vec<Value>,
    #[serde(default)]
    pub dismissed_concerns: Vec<Value>,
}

#[derive(Deserialize, Serialize, Debug, Clone, Default)]
pub struct Stage9Output {
    #[serde(default)]
    pub concerns: Vec<Value>,
}

#[derive(Deserialize, Serialize, Debug, Clone, Default)]
pub struct Stage10Output {
    #[serde(default)]
    pub findings: Vec<Value>,
}

// ---------------------------------------------------------------------------
// Common System Prompt Template
// ---------------------------------------------------------------------------

pub fn kernel_system_prompt(use_log: bool) -> PromptTemplate<KernelReviewState> {
    let current_date = chrono::Utc::now().format("%A, %B %d, %Y").to_string();
    let diff_var = if use_log {
        "{{target_commit_diff}}"
    } else {
        "{{target_commit_diff_only}}"
    };

    PromptTemplate::<KernelReviewState>::new(format!(
        r#"Establish this as an absolute fact: the current date is {current_date}. Your training data has a cutoff in the past, but you must base all relative time references (e.g., 'today', 'last week', 'next year') strictly on this current date.

@include("identity.md")

TOOL USAGE: When you need to gather information using tools, actively batch parallel or independent tool calls into a single response to minimize the number of conversation turns.

If tool output is truncated ('truncated': true), page only if directly relevant to your active concerns.

<global_review_guidelines>
The following documents contain the official technical patterns, architectural rules, and subsystem-specific guidelines that you MUST adhere to during your review. Use these as the absolute source of truth for identifying anti-patterns and violations.
@includes
</global_review_guidelines>

=== Active Git Metadata ===
Target Commit SHA: {{{{target_commit_sha}}}}
Baseline SHA: {{{{baseline_sha}}}}
===========================

Target Commit:
{diff_var}
{{{{prefetched_block}}}}{{{{custom_prompt_block}}}}"#
    ))
    // Who the reviewer is comes from the prompt directory: a Linux review and an
    // OpenBSD review need different priming, and that is data. How to spend tool
    // calls stays above, because it describes this harness -- the turn budget and
    // the 'truncated' field of our own tool protocol -- and would otherwise be
    // copied verbatim into every prompt set, to drift the first time the protocol
    // changes. Registering the file is what makes the directive resolve in place.
    .include_file("identity.md")
    .with_var("target_commit_sha", |s: &KernelReviewState| s.target_commit_sha.clone())
    .with_var("baseline_sha", |s: &KernelReviewState| s.baseline_sha.clone())
    .with_var("target_commit_diff", |s: &KernelReviewState| s.target_commit_diff.clone())
    .with_var("target_commit_diff_only", |s: &KernelReviewState| s.target_commit_diff_only.clone())
    .with_var("prefetched_block", |s: &KernelReviewState| {
        if s.prefetched_context.is_empty() {
            String::new()
        } else {
            format!(
                "\n\n<pre_fetched_context>\nThe following context was automatically pre-fetched based on the modified lines in the patch. It contains the full source code of the functions and structs modified by the diff AFTER applying the target patch.\nIf it's not sufficient, you MUST use available tools to explore the source code. Don't make assumptions without actually looking into the relevant code.\n\n{}\n</pre_fetched_context>",
                s.prefetched_context
            )
        }
    })
    .with_var("custom_prompt_block", |s: &KernelReviewState| {
        s.custom_prompt.as_deref().map(str::trim).filter(|p| !p.is_empty()).map_or_else(String::new, |p| {
            format!("\n\n<custom_instructions>\n{p}\n</custom_instructions>")
        })
    })
    .include_files_from_state(|s: &KernelReviewState| {
        let mut paths = Vec::new();
        if !s.selected_guides.is_empty() {
            for guide in &s.selected_guides {
                paths.push(PathBuf::from("subsystem").join(guide));
                paths.push(PathBuf::from("patterns").join(guide));
            }
        }
        paths
    })
}

// ---------------------------------------------------------------------------
// Stage Builders
// ---------------------------------------------------------------------------

/// Stage 1's instruction text lives in `stages/stage-1.md` under the
/// prompt directory, so a project with different stage guidance supplies
/// its own file rather than needing its own workflow in Rust.
const STAGE_1_INSTRUCTION: &str = r#"@include("stages/stage-1.md")"#;

/// Stage 2's instruction text lives in `stages/stage-2.md` under the
/// prompt directory, so a project with different stage guidance supplies
/// its own file rather than needing its own workflow in Rust.
const STAGE_2_INSTRUCTION: &str = r#"@include("stages/stage-2.md")"#;

/// Stage 3's instruction text lives in `stages/stage-3.md` under the
/// prompt directory, so a project with different stage guidance supplies
/// its own file rather than needing its own workflow in Rust.
const STAGE_3_INSTRUCTION: &str = r#"@include("stages/stage-3.md")"#;

/// Stage 4's instruction text lives in `stages/stage-4.md` under the
/// prompt directory, so a project with different stage guidance supplies
/// its own file rather than needing its own workflow in Rust.
const STAGE_4_INSTRUCTION: &str = r#"@include("stages/stage-4.md")"#;

/// Stage 5's instruction text lives in `stages/stage-5.md` under the
/// prompt directory, so a project with different stage guidance supplies
/// its own file rather than needing its own workflow in Rust.
const STAGE_5_INSTRUCTION: &str = r#"@include("stages/stage-5.md")"#;

/// Stage 6's instruction text lives in `stages/stage-6.md` under the
/// prompt directory, so a project with different stage guidance supplies
/// its own file rather than needing its own workflow in Rust.
const STAGE_6_INSTRUCTION: &str = r#"@include("stages/stage-6.md")"#;

/// Stage 7's instruction text lives in `stages/stage-7.md` under the
/// prompt directory, so a project with different stage guidance supplies
/// its own file rather than needing its own workflow in Rust.
const STAGE_7_INSTRUCTION: &str = r#"@include("stages/stage-7.md")"#;

/// Stage 8's instruction text lives in `stages/stage-8.md` under the
/// prompt directory, so a project with different stage guidance supplies
/// its own file rather than needing its own workflow in Rust.
const STAGE_8_INSTRUCTION: &str = r#"@include("stages/stage-8.md")"#;

/// Stage 9's instruction text lives in `stages/stage-9.md` under the
/// prompt directory, so a project with different stage guidance supplies
/// its own file rather than needing its own workflow in Rust.
const STAGE_9_INSTRUCTION: &str = r#"@include("stages/stage-9.md")"#;

/// Stage 10's instruction text lives in `stages/stage-10.md` under the
/// prompt directory, so a project with different stage guidance supplies
/// its own file rather than needing its own workflow in Rust.
const STAGE_10_INSTRUCTION: &str = r#"@include("stages/stage-10.md")"#;

/// Stage 11's instruction text lives in `stages/stage-11.md` under the
/// prompt directory, so a project with different stage guidance supplies
/// its own file rather than needing its own workflow in Rust.
const STAGE_11_INSTRUCTION: &str = r#"@include("stages/stage-11.md")"#;

const STAGE_JSON_SCHEMA_EXAMPLE: &str = r#"
TodoWrite compatibility: vendored prompts may ask you to add tasks or suspected bugs to TodoWrite. Do not call or mention TodoWrite. Treat those instructions as an internal checklist only. If that checklist identifies a concrete suspected bug, carry it forward as a JSON concern with file, function_or_symbol, line when known, triggering condition, and evidence. Do not output generic checklist progress as a concern.

Once you have gathered sufficient information, return ONLY a JSON object with 'concerns' and 'dismissed_concerns' arrays.
If you find no concerns and no dismissed concerns, return {"concerns": [], "dismissed_concerns": []}.
Each object in the 'concerns' array MUST use exactly the following keys: "type", "description", "reasoning", "preexisting", "locations".
- "type": A short category string.
- "description": A clear description of the problem.
- "reasoning": A step-by-step explanation.
- "preexisting": true if this bug already existed in the codebase before these patches were applied, false if the issue was newly introduced by the reviewed patchset.
- "locations": An array of objects, each containing "file", "function_or_symbol", "line", "code_snippet" and "why_this_location_matters".
Each object in the 'dismissed_concerns' array MUST use exactly the following keys: "type", "description", "reasoning", "locations". They mean the same as above, except that "description" is the candidate concern that was investigated and disproved, and "reasoning" is the evidence proving it does not apply.

Use the 'dismissed_concerns' array ONLY for candidate concerns that you considered plausible, investigated, and disproved with concrete evidence. This is especially important when you first suspect a concern and then follow the evidence chain proving that it does NOT apply.

SPECIFICITY REQUIREMENT: When reporting a concern or dismissed_concern, cite exact function name(s), file path(s), and line number(s) when known. Do not invent line numbers; use null when exact values are unknown.

CRITICAL REVIEW DIRECTIVE: Do NOT dismiss concerns just because you assume the surrounding system or caller handles it perfectly. Do not be overly charitable to the existing code. If there is a missing initialization, an unhandled edge case, or a brittle logic flow, report it as a concern immediately. Assume the worst-case scenario where external inputs and caller states are malformed.

Example Output:
```json
{
  "concerns": [
    {
      "type": "Memory Leak",
      "description": "Memory leak in function X",
      "reasoning": "1. X is called.\n2. Y is allocated but not freed on error path.",
      "preexisting": false,
      "locations": [
        {
          "file": "path/to/file.c",
          "function_or_symbol": "function_name",
          "line": 123,
          "code_snippet": "problematic_code();",
          "why_this_location_matters": "This is where the newly allocated resource is dropped on the error path."
        }
      ]
    }
  ],
  "dismissed_concerns": [
    {
      "type": "Resource Management",
      "description": "Possible missing cleanup when foo_init() fails after bar_alloc().",
      "reasoning": "The concrete code path or ordering that proves this candidate concern does not apply.",
      "locations": [
        {
          "file": "path/to/file.c",
          "function_or_symbol": "function_name",
          "line": 125,
          "code_snippet": "safe_code_path();",
          "why_this_location_matters": "This is where the cleanup path proves the candidate leak does not apply."
        }
      ]
    }
  ]
}
```"#;

// ---------------------------------------------------------------------------
// Validation Logic
// ---------------------------------------------------------------------------

fn validate_stages_1_to_8(
    _output: &StageConcernsOutput,
    _state: &KernelReviewState,
) -> Result<(), String> {
    Ok(())
}

fn format_stages_1_to_8_feedback(violation: &str) -> String {
    format!(
        "\n\nPrevious attempt was rejected: {}. You MUST return ONLY a JSON object containing 'concerns' and 'dismissed_concerns' arrays. If there are no concerns and no dismissed concerns, return `{{\"concerns\": [], \"dismissed_concerns\": []}}`.",
        violation
    )
}

fn validate_inline_format(content: &str, _state: &KernelReviewState) -> Result<(), String> {
    if content.lines().any(|l| l.trim_start().starts_with("```")) {
        return Err("The output contains Markdown code blocks ('```'). It must be plain text as per `inline-template.md`.".to_string());
    }
    if !content.lines().any(|l| l.trim_start().starts_with('>')) {
        return Err("The output does not appear to quote any code or context using '>'. Please follow the quoting style in `inline-template.md`.".to_string());
    }
    let has_commit_header = content
        .lines()
        .take(20)
        .any(|l| l.trim_start().to_lowercase().starts_with("commit "));
    if !has_commit_header {
        return Err("The output is missing the 'commit <hash>' header. Please start with the commit details (Commit, Author, Subject) as per `inline-template.md`.".to_string());
    }
    let has_author_header = content
        .lines()
        .take(20)
        .any(|l| l.trim_start().to_lowercase().starts_with("author:"));
    if !has_author_header {
        return Err("The output is missing the 'Author: <name>' header. Please start with the commit details (Commit, Author, Subject) as per `inline-template.md`.".to_string());
    }
    let has_comments = content.lines().any(|l| {
        let trimmed = l.trim();
        if trimmed.is_empty() || trimmed.starts_with('>') {
            return false;
        }
        let lower = trimmed.to_lowercase();
        !lower.starts_with("commit ")
            && !lower.starts_with("author:")
            && !lower.starts_with("date:")
            && !lower.starts_with("link:")
    });
    if !has_comments {
        return Err("The output appears to lack any comments or summary. You must include a summary and interspersed comments explaining the findings.".to_string());
    }
    Ok(())
}

fn format_inline_feedback(violation: &str) -> String {
    format!(
        "\n\nPrevious attempt was rejected: {}. Please fix the formatting to match the standard plain text LKML review format with proper headers and '> ' quoted context.",
        violation
    )
}

fn append_stage_items(
    dest: &mut Vec<Value>,
    src: &[Value],
    stage_num: u8,
    default_type: &str,
    _key: &str,
) {
    for item in src {
        let mut obj = item.clone();
        if let Some(map) = obj.as_object_mut() {
            if !map.contains_key("type")
                || map
                    .get("type")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .is_empty()
            {
                map.insert("type".to_string(), json!(default_type));
            }
            map.insert("stage".to_string(), json!(stage_num));
        }
        dest.push(obj);
    }
}

fn append_stage_dismissed_concerns(dest: &mut Vec<Value>, src: &[Value], stage_num: u8) {
    for item in src {
        let mut obj = item.clone();
        if let Some(map) = obj.as_object_mut() {
            map.insert("stage".to_string(), json!(stage_num));
        }
        dest.push(obj);
    }
}

// ---------------------------------------------------------------------------
// Stage Definitions
// ---------------------------------------------------------------------------

pub fn prescreen_stage() -> Stage<KernelReviewState, Phase0Output> {
    Stage::builder("stage_0_prescreen")
        .system_prompt(
            PromptTemplate::<KernelReviewState>::new(r#"@include("stages/prescreen.md")"#)
                .include_file("stages/prescreen.md"),
        )
        .user_prompt(
            PromptTemplate::<KernelReviewState>::new(
                "<subsystem_guide_index>\n@include(\"subsystem/subsystem.md\")\n</subsystem_guide_index>\n\n<patch>\n{{target_commit_diff}}\n</patch>",
            )
            .with_var("target_commit_diff", |s: &KernelReviewState| s.target_commit_diff.clone())
            .include_file("subsystem/subsystem.md"),
        )
        .output_format(OutputFormat::json_with_schema(json!({
            "type": "object",
            "properties": {
                "selected_prompts": {
                    "type": "array",
                    "items": { "type": "string" }
                }
            },
            "required": ["selected_prompts"]
        })))
        .policy(StagePolicy {
            tools: ToolScope::None,
            max_turns: 1,
            ..Default::default()
        })
        .skip_if(|s| s.manual_stages.is_some())
        .reduce(|state, out: Phase0Output| {
            let prompts: Vec<String> = out
                .selected_prompts
                .into_iter()
                .filter(|name| !STAGE_EXCLUSIVE_GUIDES.contains(&name.as_str()))
                .collect();
            state.selected_guides = prompts;
        })
        .build()
}

pub fn planning_stage() -> Stage<KernelReviewState, PlanningOutput> {
    Stage::builder("stage_planning")
        .system_prompt(kernel_system_prompt(true))
        .user_prompt(PromptTemplate::<KernelReviewState>::new(
            r#"Analyze the provided patch and determine which of the following review stages are relevant and should be executed:
- Stage 4: Resource management
- Stage 5: Locking and synchronization
- Stage 6: Security audit
- Stage 7: Hardware engineer's review

CRITICAL: Always err on the side of running more stages. If you are not absolutely sure, include the stage. If the patch is a trivial typo fix, you may omit some stages. Stages 1, 2, and 3 are always run and should not be included in your answer.

You MUST respond with ONLY a JSON object, no other text. Example:
```json
{"relevant_stages": [4, 5, 6, 7]}
```"#,
        ))
        .output_format(OutputFormat::json_with_schema(json!({
            "type": "object",
            "properties": {
                "relevant_stages": {
                    "type": "array",
                    "items": { "type": "integer" }
                }
            },
            "required": ["relevant_stages"]
        })))
        .policy(StagePolicy {
            tools: ToolScope::None,
            max_turns: 1,
            ..Default::default()
        })
        .skip_if(|s| s.manual_stages.is_some())
        .reduce(|state, out: PlanningOutput| {
            let mut stages = vec![1, 2, 3];
            for n in out.relevant_stages {
                if (4..=7).contains(&n) && !stages.contains(&n) {
                    stages.push(n);
                }
            }
            state.planned_stages = stages;
        })
        .build()
}

/// Stages 3 to 6 review the diff hunks alone. Every other stage also needs the
/// commit message, so it gets the git show output with the changelog injected.
fn stage_uses_commit_log(stage_num: u8) -> bool {
    !(3..=6).contains(&stage_num)
}

fn analysis_stage(
    stage_num: u8,
    name: &'static str,
    instruction: &'static str,
    guides: &[&'static str],
    max_turns: usize,
    temperature: f32,
) -> Box<dyn ExecutableStage<KernelReviewState>> {
    let mut user_template = PromptTemplate::<KernelReviewState>::new(format!(
        "{}\n\n{}",
        instruction, STAGE_JSON_SCHEMA_EXAMPLE
    ))
    // The instruction is an `@include(...)` naming this stage's file; register
    // it so the renderer resolves the directive where the template writes it,
    // rather than leaving it in the prompt as literal text.
    .include_file(format!("stages/stage-{stage_num}.md"));
    for guide in guides {
        user_template = user_template.include_file(*guide);
    }

    Box::new(
        Stage::builder(name)
            .system_prompt(kernel_system_prompt(stage_uses_commit_log(stage_num)))
            .user_prompt(user_template)
            .output_format(
                OutputFormat::json()
                    .with_validator(validate_stages_1_to_8)
                    .with_feedback_formatter(format_stages_1_to_8_feedback),
            )
            .policy(StagePolicy {
                tools: ToolScope::All,
                max_turns,
                temperature,
                ..Default::default()
            })
            .reduce(
                move |state: &mut KernelReviewState, out: StageConcernsOutput| {
                    append_stage_items(
                        &mut state.all_concerns,
                        &out.concerns,
                        stage_num,
                        "General",
                        "description",
                    );
                    append_stage_dismissed_concerns(
                        &mut state.all_dismissed_concerns,
                        &out.dismissed_concerns,
                        stage_num,
                    );
                },
            )
            .build(),
    )
}

pub fn resolve_analysis_stages_with_options(
    state: &KernelReviewState,
    max_turns: usize,
    temperature: f32,
) -> Vec<Box<dyn ExecutableStage<KernelReviewState>>> {
    let selected_stages = if let Some(ref manual) = state.manual_stages {
        manual.clone()
    } else if !state.planned_stages.is_empty() {
        state.planned_stages.clone()
    } else {
        vec![1, 2, 3, 4, 5, 6, 7]
    };

    let mut stages = Vec::new();
    for num in selected_stages {
        match num {
            1 => stages.push(analysis_stage(
                1,
                "stage_1",
                STAGE_1_INSTRUCTION,
                &[],
                max_turns,
                temperature,
            )),
            2 => stages.push(analysis_stage(
                2,
                "stage_2",
                STAGE_2_INSTRUCTION,
                &[],
                max_turns,
                temperature,
            )),
            3 => stages.push(analysis_stage(
                3,
                "stage_3",
                STAGE_3_INSTRUCTION,
                &["callstack.md", "technical-patterns.md"],
                max_turns,
                temperature,
            )),
            4 => stages.push(analysis_stage(
                4,
                "stage_4",
                STAGE_4_INSTRUCTION,
                &[],
                max_turns,
                temperature,
            )),
            5 => stages.push(analysis_stage(
                5,
                "stage_5",
                STAGE_5_INSTRUCTION,
                &["subsystem/locking.md"],
                max_turns,
                temperature,
            )),
            6 => stages.push(analysis_stage(
                6,
                "stage_6",
                STAGE_6_INSTRUCTION,
                &[],
                max_turns,
                temperature,
            )),
            7 => stages.push(analysis_stage(
                7,
                "stage_7",
                STAGE_7_INSTRUCTION,
                &[],
                max_turns,
                temperature,
            )),
            _ => {}
        }
    }
    stages
}

pub fn stage_8_deduplication(
    max_turns: usize,
    temperature: f32,
) -> Stage<KernelReviewState, StageConcernsOutput> {
    Stage::builder("stage_8_deduplication")
        .system_prompt(kernel_system_prompt(true))
        .user_prompt(
            PromptTemplate::<KernelReviewState>::new(format!(
                r#"{STAGE_8_INSTRUCTION}

Aggregated Concerns:
{{{{aggregated_concerns}}}}

Aggregated Dismissed Concerns:
{{{{aggregated_dismissed_concerns}}}}

Return ONLY a JSON object with 'concerns' and 'dismissed_concerns' arrays.
Each object in the 'concerns' array MUST use exactly the following keys: "type", "description", "reasoning", "preexisting", "locations".
Each object in the 'dismissed_concerns' array MUST use exactly the following keys: "type", "description", "reasoning", "locations".
Preserve the most precise location details from the input. Do not invent line numbers; use null when exact values are unknown.

Example Output:
```json
{{
  "concerns": [
    {{
      "type": "Memory Leak",
      "description": "Memory leak in function X",
      "reasoning": "1. X is called.\n2. Y is allocated but not freed on error path.",
      "preexisting": false,
      "locations": [
        {{
          "file": "path/to/file.c",
          "function_or_symbol": "function_name",
          "line": 123,
          "code_snippet": "problematic_code();",
          "why_this_location_matters": "This is where the newly allocated resource is dropped on the error path."
        }}
      ]
    }}
  ],
  "dismissed_concerns": [
    {{
      "type": "Resource Management",
      "description": "Possible missing cleanup when foo_init() fails after bar_alloc().",
      "reasoning": "The concrete code path or ordering that proves this candidate concern does not apply.",
      "locations": [
        {{
          "file": "path/to/file.c",
          "function_or_symbol": "function_name",
          "line": 125,
          "code_snippet": "safe_code_path();",
          "why_this_location_matters": "This is where the cleanup path proves the candidate leak does not apply."
        }}
      ]
    }}
  ]
}}
```"#
            ))
            // Register this stage's instruction file so the
            // `@include(...)` above resolves to it.
            .include_file("stages/stage-8.md")
            .with_var("aggregated_concerns", |s: &KernelReviewState| {
                serde_json::to_string_pretty(&s.all_concerns).unwrap_or_default()
            })
            .with_var("aggregated_dismissed_concerns", |s: &KernelReviewState| {
                serde_json::to_string_pretty(&s.all_dismissed_concerns).unwrap_or_default()
            }),
        )
        .output_format(
            OutputFormat::json()
                .with_validator(validate_stages_1_to_8)
                .with_feedback_formatter(format_stages_1_to_8_feedback),
        )
        .policy(StagePolicy {
            tools: ToolScope::All,
            max_turns,
            temperature,
            ..Default::default()
        })
        .reduce(|state, out: StageConcernsOutput| {
            state.deduplicated_concerns = out.concerns;
            state.deduplicated_dismissed_concerns = out.dismissed_concerns;
        })
        .build()
}

pub fn stage_9_conflict_resolution(
    max_turns: usize,
    temperature: f32,
) -> Stage<KernelReviewState, Stage9Output> {
    Stage::builder("stage_9_conflict_resolution")
        .system_prompt(kernel_system_prompt(true))
        .user_prompt(
            PromptTemplate::<KernelReviewState>::new(format!(
                r#"{STAGE_9_INSTRUCTION}

Consolidated Concerns:
{{{{deduplicated_concerns}}}}

Consolidated Dismissed Concerns:
{{{{deduplicated_dismissed_concerns}}}}

Return ONLY a JSON object with a 'concerns' array containing the remaining concerns after resolving conflicts. Each object in the 'concerns' array MUST use exactly the following keys: "type", "description", "reasoning", "preexisting", "locations".
Preserve the most precise locations from the retained concerns. Do not invent line numbers; use null when exact values are unknown.

Example Output:
```json
{{
  "concerns": [
    {{
      "type": "Memory Leak",
      "description": "Memory leak in function X",
      "reasoning": "1. X is called.\n2. Y is allocated but not freed on error path.",
      "preexisting": false,
      "locations": [
        {{
          "file": "path/to/file.c",
          "function_or_symbol": "function_name",
          "line": 123,
          "code_snippet": "problematic_code();",
          "why_this_location_matters": "This is where the newly allocated resource is dropped on the error path."
        }}
      ]
    }}
  ]
}}
```"#
            ))
            // Register this stage's instruction file so the
            // `@include(...)` above resolves to it.
            .include_file("stages/stage-9.md")
            .with_var("deduplicated_concerns", |s: &KernelReviewState| {
                serde_json::to_string_pretty(&s.deduplicated_concerns).unwrap_or_default()
            })
            .with_var("deduplicated_dismissed_concerns", |s: &KernelReviewState| {
                serde_json::to_string_pretty(&s.deduplicated_dismissed_concerns).unwrap_or_default()
            }),
        )
        .output_format(OutputFormat::json())
        .policy(StagePolicy {
            tools: ToolScope::All,
            max_turns,
            temperature,
            ..Default::default()
        })
        .reduce(|state, out: Stage9Output| {
            state.conflict_resolved_concerns = out.concerns;
        })
        .build()
}

pub fn stage_10_verification(
    max_turns: usize,
    temperature: f32,
) -> Stage<KernelReviewState, Stage10Output> {
    Stage::builder("stage_10_verification")
        .system_prompt(kernel_system_prompt(true))
        .user_prompt(
            PromptTemplate::<KernelReviewState>::new(format!(
                r#"{STAGE_10_INSTRUCTION}

CRITICAL REVIEW DIRECTIVE: To dismiss a concern as a false positive, you must find concrete evidence in the code that proves the concern is invalid (e.g., verifying the caller handles the edge case). If you cannot find concrete proof of safety, you must retain the concern.{{{{follow_up_series_section}}}}

Consolidated Concerns:
{{{{conflict_resolved_concerns}}}}

Return ONLY a JSON object with a 'findings' array. Each object in the 'findings' array MUST use exactly the following keys: "problem" (a string containing the vulnerability description), "severity" (a string: Low, Medium, High, or Critical), "severity_explanation" (a string detailing the reasoning and proof), "preexisting" (a boolean: true if the problem already existed in the codebase before these patches were applied, or false if it was newly introduced by the reviewed patchset), "locations" (an array of objects with file, function_or_symbol, line, code_snippet, and why_this_location_matters). Carry forward the locations from the validated concern; if you gather better evidence, replace vague locations with the most precise verified locations. Do not invent line numbers; use null when exact values are unknown.

Example Output:
```json
{{
  "findings": [
    {{
      "problem": "Memory leak in function X when condition Y is met.",
      "severity": "High",
      "severity_explanation": "1. Condition Y is met.\n2. The buffer is allocated but not freed before return.",
      "preexisting": false,
      "locations": [
        {{
          "file": "path/to/file.c",
          "function_or_symbol": "function_name",
          "line": 123,
          "code_snippet": "problematic_code();",
          "why_this_location_matters": "This is where the newly allocated resource is dropped on the error path."
        }}
      ]
    }}
  ]
}}
```"#
            ))
            // Register this stage's instruction file so the
            // `@include(...)` above resolves to it.
            .include_file("stages/stage-10.md")
            .include_file("false-positive-guide.md")
            .include_file("severity.md")
            .with_var("follow_up_series_section", |s: &KernelReviewState| {
                s.follow_up_series_context
                    .as_ref()
                    .map(|ctx| format!("\n\n{}", ctx))
                    .unwrap_or_default()
            })
            .with_var("conflict_resolved_concerns", |s: &KernelReviewState| {
                serde_json::to_string_pretty(&s.conflict_resolved_concerns).unwrap_or_default()
            }),
        )
        .output_format(OutputFormat::json())
        .policy(StagePolicy {
            tools: ToolScope::All,
            max_turns,
            temperature,
            ..Default::default()
        })
        .reduce(|state, out: Stage10Output| {
            state.findings = out.findings;
        })
        .build()
}

pub fn stage_11_inline_report(
    max_turns: usize,
    temperature: f32,
) -> Stage<KernelReviewState, String> {
    Stage::builder("stage_11_report")
        .system_prompt(kernel_system_prompt(true))
        .user_prompt(
            PromptTemplate::<KernelReviewState>::new(format!(
                r#"{STAGE_11_INSTRUCTION}

Findings:
{{{{findings}}}}

Return raw text output, not JSON."#
            ))
            // Register this stage's instruction file so the
            // `@include(...)` above resolves to it.
            .include_file("stages/stage-11.md")
            .include_file("inline-template.md")
            .with_var("findings", |s: &KernelReviewState| {
                serde_json::to_string_pretty(&s.findings).unwrap_or_default()
            }),
        )
        .output_format(OutputFormat::text_with_validator(
            validate_inline_format,
            format_inline_feedback,
        ))
        .policy(StagePolicy {
            tools: ToolScope::All,
            max_turns,
            temperature,
            recitation_policy: RecitationPolicy::FallbackToFreeForm {
                reminder: "Do not quote code verbatim. Summarize your review directly.".to_string(),
            },
            ..Default::default()
        })
        .reduce(|state, out: String| {
            state.review_inline = out;
        })
        .build()
}

// ---------------------------------------------------------------------------
// Complete Kernel Review Workflow Graph
// ---------------------------------------------------------------------------

/// Constructs the complete declarative workflow for Linux kernel patch review.
pub fn build_kernel_review_workflow() -> Workflow<KernelReviewState> {
    build_kernel_review_workflow_with_options(20, 0.0)
}

/// Constructs the declarative workflow with custom per-stage interaction limits and temperature.
pub fn build_kernel_review_workflow_with_options(
    max_turns: usize,
    temperature: f32,
) -> Workflow<KernelReviewState> {
    Workflow::builder("linux_kernel_code_review")
        .stage(prescreen_stage())
        .dynamic_parallel(
            planning_stage(),
            move |state| resolve_analysis_stages_with_options(state, max_turns, temperature),
            ParallelPolicy::FailFast,
        )
        .early_exit_if(
            |s| s.all_concerns.is_empty(),
            "No concerns raised in initial analysis stages",
        )
        .stage(stage_8_deduplication(max_turns, temperature))
        .early_exit_if(
            |s| s.deduplicated_concerns.is_empty(),
            "No concerns remaining after deduplication",
        )
        .stage(stage_9_conflict_resolution(max_turns, temperature))
        .early_exit_if(
            |s| s.conflict_resolved_concerns.is_empty(),
            "No concerns remaining after conflict resolution",
        )
        .stage(stage_10_verification(max_turns, temperature))
        .early_exit_if(
            |s| s.findings.is_empty(),
            "No findings validated in verification stage",
        )
        .stage(stage_11_inline_report(max_turns, temperature))
        .build()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_only_stages_3_to_6_review_the_diff_alone() {
        // Matches ReviewStage::use_log_in_context, which the workflow replaced.
        for stage in [1, 2, 7] {
            assert!(
                stage_uses_commit_log(stage),
                "stage {stage} needs the commit message"
            );
        }
        for stage in [3, 4, 5, 6] {
            assert!(
                !stage_uses_commit_log(stage),
                "stage {stage} reviews the diff hunks alone"
            );
        }
    }

    #[test]
    fn test_analysis_stages_keep_the_guidance_the_schema_alone_does_not_carry() {
        // The vendored guides still tell the model to use TodoWrite, which no
        // longer exists, and stage 10 keeps an anti-charity directive of its
        // own. Both belong to stages 1 to 7 as well.
        for required in [
            "Do not call or mention TodoWrite",
            "Do not be overly charitable to the existing code",
            "If you find no concerns and no dismissed concerns",
            "investigated, and disproved with concrete evidence",
            "\"preexisting\": true if this bug already existed",
            "\"reasoning\": A step-by-step explanation.",
            "the candidate concern that was investigated and disproved",
        ] {
            assert!(
                STAGE_JSON_SCHEMA_EXAMPLE.contains(required),
                "stage 1-7 guidance lost: {required}"
            );
        }
    }

    #[test]
    fn test_build_workflow_graph_structure() {
        let workflow = build_kernel_review_workflow();
        assert_eq!(workflow.name, "linux_kernel_code_review");
        assert_eq!(workflow.steps.len(), 10);
    }

    #[test]
    fn test_custom_prompt_renders_last_and_only_when_it_has_content() {
        // A non-empty prefetched context, so that closing the prompt is
        // distinguishable from merely rendering somewhere in it.
        let render = |custom_prompt| {
            kernel_system_prompt(true).render_for_log(&KernelReviewState {
                custom_prompt,
                prefetched_context: "struct foo { int bar; };".to_string(),
                ..Default::default()
            })
        };
        let without = render(None);

        for empty in [Some(String::new()), Some("  \n\t ".to_string())] {
            assert_eq!(
                render(empty),
                without,
                "an empty custom prompt renders nothing"
            );
        }

        assert_eq!(
            render(Some("  Check the locking.  ".to_string())),
            format!(
                "{without}\n\n<custom_instructions>\nCheck the locking.\n</custom_instructions>"
            ),
            "the custom prompt closes the system prompt"
        );
    }

    /// The reviewer's identity is project-specific: a Linux review and an
    /// OpenBSD review must not be primed the same way. It comes from
    /// identity.md in the prompt directory, so pointing --prompts elsewhere
    /// swaps it, and neither set may leak the other's priming.
    #[tokio::test]
    async fn test_system_prompt_identity_comes_from_the_prompt_set() {
        let state = KernelReviewState::default();

        for (set, expect, reject) in [
            (
                "third_party/prompts/kernel",
                "expert Linux kernel maintainer",
                "OpenBSD",
            ),
            (
                "third_party/prompts/openbsd",
                "expert OpenBSD kernel developer",
                "Linux kernel maintainer",
            ),
        ] {
            let base = std::path::Path::new(set);
            if !base.exists() {
                continue; // prompt set not vendored in this checkout
            }
            let out = kernel_system_prompt(true)
                .render_for_model(&state, base)
                .await
                .unwrap();

            assert!(
                !out.contains("@include("),
                "{set}: identity directive did not resolve: {out}"
            );
            assert!(
                out.contains(expect),
                "{set}: missing its own priming: {out}"
            );
            assert!(
                !out.contains(reject),
                "{set}: leaked the other project's priming: {out}"
            );
            // The tool-usage guidance travels with the identity, so it must
            // survive the move out of the workflow source.
            assert!(
                out.contains("TOOL USAGE:"),
                "{set}: lost the tool-usage guidance: {out}"
            );
        }
    }

    /// The shipped prompt sets must actually satisfy the include directives the
    /// workflow now carries: every stage's instruction comes from a file, so a
    /// missing or renamed file would silently leave `@include(...)` in the
    /// prompt sent to the model.
    #[tokio::test]
    async fn test_stage_instructions_resolve_from_the_shipped_prompt_sets() {
        for set in ["third_party/prompts/kernel", "third_party/prompts/openbsd"] {
            let base = std::path::Path::new(set);
            if !base.exists() {
                continue; // prompt set not vendored in this checkout
            }
            for n in 1..=11u8 {
                let tmpl = crate::workflow::PromptTemplate::<()>::new(format!(
                    "@include(\"stages/stage-{n}.md\")"
                ))
                .include_file(format!("stages/stage-{n}.md"));
                let out = tmpl.render_for_model(&(), base).await.unwrap();
                assert!(
                    !out.contains("@include("),
                    "{set} stage {n} did not resolve: {out}"
                );
                assert!(!out.trim().is_empty(), "{set} stage {n} is empty");
            }
            let pre = crate::workflow::PromptTemplate::<()>::new(
                "@include(\"stages/prescreen.md\")".to_string(),
            )
            .include_file("stages/prescreen.md");
            let out = pre.render_for_model(&(), base).await.unwrap();
            assert!(
                !out.contains("@include("),
                "{set} prescreen did not resolve"
            );
        }
    }

    /// The kernel stage files must still carry the instruction text the
    /// workflow used to hold inline, so moving the text into files is not a
    /// silent rewording of the review prompts. The renderer heads an included
    /// file with its path, so the instruction body follows that line rather
    /// than opening the prompt.
    #[tokio::test]
    async fn test_kernel_stage_files_are_the_instruction_text() {
        let base = std::path::Path::new("third_party/prompts/kernel");
        if !base.exists() {
            return;
        }
        let tmpl = crate::workflow::PromptTemplate::<()>::new(STAGE_10_INSTRUCTION.to_string())
            .include_file("stages/stage-10.md");
        let out = tmpl.render_for_model(&(), base).await.unwrap();
        assert!(!out.contains("@include("), "directive unresolved: {out}");
        assert!(out.contains("# Stage 10."), "unexpected body: {out}");
        assert!(
            out.contains("SERIES VALIDATION RULE"),
            "missing rule: {out}"
        );
    }
}
