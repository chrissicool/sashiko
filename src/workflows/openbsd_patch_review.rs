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

//! Single declarative workflow definition for Sashiko's OpenBSD kernel code review.
//!
//! This module specifies the multi-stage review pipeline as a declarative [`Workflow`]
//! operating over [`OpenBsdPatchReviewState`].

use std::path::PathBuf;

use serde_json::{Value, json};

use crate::workflow::graph::Workflow;
use crate::workflow::output::OutputFormat;
use crate::workflow::policy::{ParallelPolicy, RecitationPolicy, StagePolicy, ToolScope};
use crate::workflow::prompt::PromptTemplate;
use crate::workflow::stage::{ExecutableStage, Stage};

use crate::workflows::linux_patch_review::{
    AnalysisStage, ConflictResolutionOutput, ConsolidationStage, LinuxPatchReviewState,
    PlanningOutput, PrescreenOutput, SERIES_CONTEXT_PLACEHOLDER, StageConcernsOutput,
    VerificationOutput,
};

/// State container for an OpenBSD patch review run.
///
/// The state a review carries -- the diff, the selected guides, the concerns
/// accumulated by the stages -- is the same shape whatever the project, so it
/// is shared rather than duplicated. Only the prompts and the stage table
/// differ, which is what this module supplies.
pub type OpenBsdPatchReviewState = LinuxPatchReviewState;

// ---------------------------------------------------------------------------
// Common System Prompt Template
// ---------------------------------------------------------------------------

pub fn openbsd_system_prompt(use_log: bool) -> PromptTemplate<OpenBsdPatchReviewState> {
    let current_date = chrono::Utc::now().format("%A, %B %d, %Y").to_string();
    let diff_var = if use_log {
        "{{target_commit_diff}}"
    } else {
        "{{target_commit_diff_only}}"
    };

    PromptTemplate::<OpenBsdPatchReviewState>::new(format!(
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
    // Who the reviewer is comes from the prompt set: a Linux review and an
    // OpenBSD review need different priming, and that is data. How to spend
    // tool calls stays above, because it describes this harness -- the turn
    // budget and the 'truncated' field of our own tool protocol -- and would
    // otherwise be copied into every prompt set, to drift the first time the
    // protocol changes. Registering the file is what makes the directive
    // resolve in place rather than survive into the prompt as literal text.
    .include_file("identity.md")
    .with_var("target_commit_sha", |s: &OpenBsdPatchReviewState| s.target_commit_sha.clone())
    .with_var("baseline_sha", |s: &OpenBsdPatchReviewState| s.baseline_sha.clone())
    .with_var("target_commit_diff", |s: &OpenBsdPatchReviewState| s.target_commit_diff.clone())
    .with_var("target_commit_diff_only", |s: &OpenBsdPatchReviewState| s.target_commit_diff_only.clone())
    .with_var("prefetched_block", |s: &OpenBsdPatchReviewState| {
        if s.prefetch_failed {
            format!(
                "\n\nAutomatic source prefetch failed for target commit {}. Before analyzing the code, use git_read_files and git_grep at that revision to gather the source context. Do not infer source contents from the physical checkout.\n",
                s.target_commit_sha
            )
        } else if s.prefetched_context.is_empty() {
            String::new()
        } else {
            format!(
                "\n\n<pre_fetched_context>\nThe following source excerpts were fetched from the target commit identified by Source revision below, based on the modified lines in the patch. They include modified definitions and selected dependencies. Parent and series-final revisions must be inspected separately with Git tools.\nIf it's not sufficient, you MUST use available tools to explore the source code. Don't make assumptions without actually looking into the relevant code.\n\n{}\n</pre_fetched_context>",
                s.prefetched_context
            )
        }
    })
    .with_var("custom_prompt_block", |s: &OpenBsdPatchReviewState| {
        s.custom_prompt.as_deref().map(str::trim).filter(|p| !p.is_empty()).map_or_else(String::new, |p| {
            format!("\n\n<custom_instructions>\n{p}\n</custom_instructions>")
        })
    })
    .include_files_from_state(|s: &OpenBsdPatchReviewState| {
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

const STAGE_GOAL_INSTRUCTION: &str = r#"@include("stages/goal.md")"#;

const STAGE_IMPLEMENTATION_INSTRUCTION: &str = r#"@include("stages/implementation.md")"#;

const STAGE_EXECUTION_FLOW_INSTRUCTION: &str = r#"@include("stages/execution-flow.md")"#;

const STAGE_RESOURCES_INSTRUCTION: &str = r#"@include("stages/resources.md")"#;

const STAGE_LOCKING_INSTRUCTION: &str = r#"@include("stages/locking.md")"#;

const STAGE_SECURITY_INSTRUCTION: &str = r#"@include("stages/security.md")"#;

const STAGE_HARDWARE_INSTRUCTION: &str = r#"@include("stages/hardware.md")"#;

const STAGE_DEDUPLICATION_INSTRUCTION: &str = r#"@include("stages/deduplication.md")"#;

const STAGE_CONFLICT_RESOLUTION_INSTRUCTION: &str = r#"@include("stages/conflict-resolution.md")"#;

const STAGE_VERIFICATION_INSTRUCTION: &str = r#"@include("stages/verification.md")"#;

pub const STAGE_REPORT_INSTRUCTION: &str = r#"@include("stages/report.md")"#;

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

fn validate_concerns_output(
    _output: &StageConcernsOutput,
    _state: &OpenBsdPatchReviewState,
) -> Result<(), String> {
    Ok(())
}

fn format_concerns_feedback(violation: &str) -> String {
    format!(
        "\n\nPrevious attempt was rejected: {}. You MUST return ONLY a JSON object containing 'concerns' and 'dismissed_concerns' arrays. If there are no concerns and no dismissed concerns, return `{{\"concerns\": [], \"dismissed_concerns\": []}}`.",
        violation
    )
}

fn validate_inline_format(content: &str, _state: &OpenBsdPatchReviewState) -> Result<(), String> {
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
        "\n\nPrevious attempt was rejected: {}. Please fix the formatting to match the plain text review format described in inline-template.md, with proper headers and '> ' quoted context.",
        violation
    )
}

fn append_stage_items(
    dest: &mut Vec<Value>,
    src: &[Value],
    stage: &str,
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
            map.insert("stage".to_string(), json!(stage));
        }
        dest.push(obj);
    }
}

fn append_stage_dismissed_concerns(dest: &mut Vec<Value>, src: &[Value], stage: &str) {
    for item in src {
        let mut obj = item.clone();
        if let Some(map) = obj.as_object_mut() {
            map.insert("stage".to_string(), json!(stage));
        }
        dest.push(obj);
    }
}

// ---------------------------------------------------------------------------
// Stage Definitions
// ---------------------------------------------------------------------------

pub fn prescreen_stage() -> Stage<OpenBsdPatchReviewState, PrescreenOutput> {
    Stage::builder("pre-screen")
        .system_prompt(
            PromptTemplate::<OpenBsdPatchReviewState>::new(r#"@include("stages/prescreen.md")"#)
                .include_file("stages/prescreen.md"),
        )
        .user_prompt(
            PromptTemplate::<OpenBsdPatchReviewState>::new(
                "<subsystem_guide_index>\n@include(\"subsystem/subsystem.md\")\n</subsystem_guide_index>\n\n<patch>\n{{target_commit_diff}}\n</patch>",
            )
            .with_var("target_commit_diff", |s: &OpenBsdPatchReviewState| s.target_commit_diff.clone())
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
        .reduce(|state, out: PrescreenOutput| {
            let prompts: Vec<String> = out
                .selected_prompts
                .into_iter()
                .filter(|name| !is_stage_exclusive_guide(name))
                .filter(|name| crate::workflows::guard::sanitize_guide_name(name))
                .collect();
            state.selected_guides = prompts;
        })

        .build()
}

pub fn planning_stage() -> Stage<OpenBsdPatchReviewState, PlanningOutput> {
    let optional_stages: Vec<&'static str> = ANALYSIS_STAGES
        .iter()
        .filter(|d| d.optional)
        .map(|d| d.name)
        .collect();

    Stage::builder("planning")
        .system_prompt(openbsd_system_prompt(true))
        .user_prompt(PromptTemplate::<OpenBsdPatchReviewState>::new(
            r#"Analyze the provided patch and determine which of the following review stages are relevant and should be executed:
- resources: Resource management
- locking: Locking and synchronization
- security: Security audit
- hardware: Hardware engineer's review

CRITICAL: Always err on the side of running more stages. If you are not absolutely sure, include the stage. If the patch is a trivial typo fix, you may omit some stages. Stages not listed above always run and should not be included in your answer.

You MUST respond with ONLY a JSON object, no other text. Use the names exactly as given above. Example:
```json
{"relevant_stages": ["resources", "locking", "security", "hardware"]}
```"#,
        ))
        .output_format(OutputFormat::json_with_schema(json!({
            "type": "object",
            "properties": {
                "relevant_stages": {
                    "type": "array",
                    "items": {
                        "type": "string",
                        "enum": optional_stages,
                    }
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
            // The stages the planner is not asked about run regardless. Its
            // answer is then admitted only where it names an optional stage,
            // which is what stops a hallucinated name reaching the resolver.
            let mut stages: Vec<String> = ANALYSIS_STAGES
                .iter()
                .filter(|d| !d.optional)
                .map(|d| d.name.to_string())
                .collect();
            for raw_name in out.relevant_stages {
                if let Some(def) = analysis_stage_by_name(&raw_name) {
                    if def.optional && !stages.iter().any(|s| s == def.name) {
                        stages.push(def.name.to_string());
                    }
                } else {
                    tracing::warn!("Ignoring unknown planned review stage {:?}", raw_name);
                }
            }
            state.planned_stages = stages;
        })
        .build()
}

pub static ANALYSIS_STAGES: &[AnalysisStage] = &[
    AnalysisStage {
        name: "goal",
        short: "Goal Analysis",
        instruction: STAGE_GOAL_INSTRUCTION,
        guides: &[],
        uses_commit_log: true,
        optional: false,
        wants_series_context: false,
    },
    AnalysisStage {
        name: "implementation",
        short: "Implementation",
        instruction: STAGE_IMPLEMENTATION_INSTRUCTION,
        guides: &[],
        uses_commit_log: true,
        optional: false,
        wants_series_context: false,
    },
    AnalysisStage {
        name: "execution-flow",
        short: "Execution Flow",
        instruction: STAGE_EXECUTION_FLOW_INSTRUCTION,
        guides: &["callstack.md", "technical-patterns.md"],
        uses_commit_log: false,
        optional: false,
        wants_series_context: false,
    },
    AnalysisStage {
        name: "resources",
        short: "Resource Mgmt",
        instruction: STAGE_RESOURCES_INSTRUCTION,
        guides: &[],
        uses_commit_log: false,
        optional: true,
        wants_series_context: false,
    },
    AnalysisStage {
        name: "locking",
        short: "Locking & Sync",
        instruction: STAGE_LOCKING_INSTRUCTION,
        guides: &["subsystem/locking.md"],
        uses_commit_log: false,
        optional: true,
        wants_series_context: false,
    },
    AnalysisStage {
        name: "security",
        short: "Security Audit",
        instruction: STAGE_SECURITY_INSTRUCTION,
        guides: &[],
        uses_commit_log: false,
        optional: true,
        wants_series_context: false,
    },
    AnalysisStage {
        name: "hardware",
        short: "Hardware Review",
        instruction: STAGE_HARDWARE_INSTRUCTION,
        guides: &[],
        uses_commit_log: true,
        optional: true,
        wants_series_context: false,
    },
];

pub static DEDUPLICATION: ConsolidationStage = ConsolidationStage {
    name: "deduplication",
    short: "Deduplication",
    wants_series_context: false,
};

pub static CONFLICT_RESOLUTION: ConsolidationStage = ConsolidationStage {
    name: "conflict-resolution",
    short: "Conflict Resolution",
    wants_series_context: false,
};

pub static VERIFICATION: ConsolidationStage = ConsolidationStage {
    name: "verification",
    short: "Severity Estimation",
    wants_series_context: true,
};

pub static REPORT: ConsolidationStage = ConsolidationStage {
    name: "report",
    short: "Report Generation",
    wants_series_context: false,
};

/// In the order the workflow runs them. Each builder refers to its own
/// definition above, so the name a stage registers under is the same string
/// this list recognises and labels.
pub static CONSOLIDATION_STAGES: &[&ConsolidationStage] =
    &[&DEDUPLICATION, &CONFLICT_RESOLUTION, &VERIFICATION, &REPORT];

/// Marks where a stage's prompt carries the list of patches that follow this
/// one in the series.
///
/// Two kinds of question need it. Where a test belongs in a series is only
/// answerable from what comes after it, and whether a concern still stands can
/// depend on a later patch reworking the code it is about. Both are declared in
/// the stage tables rather than wired up per builder, so the placeholder and
/// the variable that fills it cannot get separated.
fn series_context_placeholder(wants: bool) -> &'static str {
    if wants {
        SERIES_CONTEXT_PLACEHOLDER
    } else {
        ""
    }
}

fn with_series_context(
    template: PromptTemplate<OpenBsdPatchReviewState>,
    wants: bool,
) -> PromptTemplate<OpenBsdPatchReviewState> {
    if !wants {
        return template;
    }
    template.with_var("follow_up_series_section", |s: &OpenBsdPatchReviewState| {
        s.follow_up_series_context
            .as_ref()
            .map(|ctx| format!("\n\n{}", ctx))
            .unwrap_or_default()
    })
}

use crate::workflows::guard::normalize_stage_name;

pub fn consolidation_stage_by_name(name: &str) -> Option<&'static ConsolidationStage> {
    let normalized = normalize_stage_name(name);
    CONSOLIDATION_STAGES
        .iter()
        .copied()
        .find(|s| s.name == normalized)
}

/// Display label for any stage the pipeline runs.
pub fn stage_short_label(name: &str) -> Option<&'static str> {
    if let Some(def) = analysis_stage_by_name(name) {
        return Some(def.short);
    }
    consolidation_stage_by_name(name).map(|s| s.short)
}

/// Whether a guide belongs to one stage rather than to the shared context.
///
/// The pre-screen offers a guide to the whole review, but a guide some stage
/// loads for itself would then arrive twice: once in that stage's user prompt
/// and again in every stage's system prompt. Deriving the answer from the
/// stage table means a guide claimed in the table is excluded by that fact
/// alone, with no second list to keep in step.
pub fn is_stage_exclusive_guide(name: &str) -> bool {
    ANALYSIS_STAGES
        .iter()
        .flat_map(|def| def.guides)
        .any(|guide| guide.rsplit('/').next() == Some(name))
}

pub fn analysis_stage_by_name(name: &str) -> Option<&'static AnalysisStage> {
    let normalized = normalize_stage_name(name);
    ANALYSIS_STAGES.iter().find(|s| s.name == normalized)
}

/// Every stage name a review can produce, analysis and consolidation alike,
/// for validating what a caller or the planner asked for.
pub fn is_known_stage(name: &str) -> bool {
    let normalized = normalize_stage_name(name);
    analysis_stage_by_name(&normalized).is_some()
        || consolidation_stage_by_name(&normalized).is_some()
        || matches!(normalized.as_str(), "pre-screen" | "planning")
}

fn analysis_stage(
    def: &'static AnalysisStage,
    max_turns: usize,
    temperature: f32,
) -> Box<dyn ExecutableStage<OpenBsdPatchReviewState>> {
    let mut user_template = PromptTemplate::<OpenBsdPatchReviewState>::new(format!(
        "{}\n\n{}{}",
        def.instruction,
        STAGE_JSON_SCHEMA_EXAMPLE,
        series_context_placeholder(def.wants_series_context)
    ))
    // The instruction is an `@include(...)` naming this stage's file; register
    // it so the renderer resolves the directive where the template writes it
    // rather than leaving it in the prompt as literal text.
    .include_file(format!("stages/{}.md", def.name));
    for guide in def.guides {
        user_template = user_template.include_file(*guide);
    }
    let user_template = with_series_context(user_template, def.wants_series_context);

    Box::new(
        Stage::builder(def.name)
            .system_prompt(openbsd_system_prompt(def.uses_commit_log))
            .user_prompt(user_template)
            .output_format(
                OutputFormat::json()
                    .with_validator(validate_concerns_output)
                    .with_feedback_formatter(format_concerns_feedback),
            )
            .policy(StagePolicy {
                tools: ToolScope::All,
                max_turns,
                temperature,
                ..Default::default()
            })
            .reduce(
                move |state: &mut OpenBsdPatchReviewState, out: StageConcernsOutput| {
                    append_stage_items(
                        &mut state.all_concerns,
                        &out.concerns,
                        def.name,
                        "General",
                        "description",
                    );
                    append_stage_dismissed_concerns(
                        &mut state.all_dismissed_concerns,
                        &out.dismissed_concerns,
                        def.name,
                    );
                },
            )
            .build(),
    )
}

pub fn resolve_analysis_stages_with_options(
    state: &OpenBsdPatchReviewState,
    max_turns: usize,
    temperature: f32,
) -> Vec<Box<dyn ExecutableStage<OpenBsdPatchReviewState>>> {
    let selected_stages: Vec<String> = if let Some(ref manual) = state.manual_stages {
        manual.clone()
    } else if !state.planned_stages.is_empty() {
        state.planned_stages.clone()
    } else {
        ANALYSIS_STAGES.iter().map(|d| d.name.to_string()).collect()
    };

    let mut stages = Vec::new();
    for name in selected_stages {
        match analysis_stage_by_name(&name) {
            Some(def) => stages.push(analysis_stage(def, max_turns, temperature)),
            // Dropping an unrecognised entry in silence would make a mistyped
            // --stages look like it had worked.
            None => tracing::warn!("Ignoring unknown review stage {:?}", name),
        }
    }
    stages
}

pub fn deduplication_stage(
    max_turns: usize,
    temperature: f32,
) -> Stage<OpenBsdPatchReviewState, StageConcernsOutput> {
    Stage::builder(DEDUPLICATION.name)
        .system_prompt(openbsd_system_prompt(true))
        .user_prompt(
            PromptTemplate::<OpenBsdPatchReviewState>::new(format!(
                r#"{STAGE_DEDUPLICATION_INSTRUCTION}

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
            .include_file("stages/deduplication.md")
            .with_var("aggregated_concerns", |s: &OpenBsdPatchReviewState| {
                serde_json::to_string_pretty(&s.all_concerns).unwrap_or_default()
            })
            .with_var("aggregated_dismissed_concerns", |s: &OpenBsdPatchReviewState| {
                serde_json::to_string_pretty(&s.all_dismissed_concerns).unwrap_or_default()
            }),
        )
        .output_format(
            OutputFormat::json()
                .with_validator(validate_concerns_output)
                .with_feedback_formatter(format_concerns_feedback),
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

pub fn conflict_resolution_stage(
    max_turns: usize,
    temperature: f32,
) -> Stage<OpenBsdPatchReviewState, ConflictResolutionOutput> {
    Stage::builder(CONFLICT_RESOLUTION.name)
        .system_prompt(openbsd_system_prompt(true))
        .user_prompt(
            PromptTemplate::<OpenBsdPatchReviewState>::new(format!(
                r#"{STAGE_CONFLICT_RESOLUTION_INSTRUCTION}

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
            .include_file("stages/conflict-resolution.md")
            .with_var("deduplicated_concerns", |s: &OpenBsdPatchReviewState| {
                serde_json::to_string_pretty(&s.deduplicated_concerns).unwrap_or_default()
            })
            .with_var("deduplicated_dismissed_concerns", |s: &OpenBsdPatchReviewState| {
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
        .reduce(|state, out: ConflictResolutionOutput| {
            let mut new_concerns = Vec::new();
            let mut preexisting = Vec::new();
            for concern in out.concerns {
                let is_preexisting = concern
                    .get("preexisting")
                    .and_then(|v| v.as_bool())
                    .unwrap_or(false);
                if is_preexisting {
                    preexisting.push(concern);
                } else {
                    new_concerns.push(concern);
                }
            }
            state.patch_concerns = new_concerns;
            state.concerns = preexisting;
        })
        .build()
}

pub fn verification_stage(
    max_turns: usize,
    temperature: f32,
) -> Stage<OpenBsdPatchReviewState, VerificationOutput> {
    let series_context = series_context_placeholder(VERIFICATION.wants_series_context);
    Stage::builder(VERIFICATION.name)
        .system_prompt(openbsd_system_prompt(true))
        .user_prompt(with_series_context(
            PromptTemplate::<OpenBsdPatchReviewState>::new(format!(
                r#"{STAGE_VERIFICATION_INSTRUCTION}

CRITICAL REVIEW DIRECTIVE: To dismiss a concern as a false positive, you must find concrete evidence in the code that proves the concern is invalid (e.g., verifying the caller handles the edge case). If you cannot find concrete proof of safety, you must retain the concern.{series_context}

Consolidated Concerns:
{{{{patch_concerns}}}}

Return ONLY a JSON object with a 'findings' array. Each object in the 'findings' array MUST use exactly the following keys: "problem" (a short naming string containing the vulnerability description. BUG NAME RULES: 1) less than 80 characters, 2) preferably start with a short subsystem prefix like 'mm:' or 'bpf:', 3) NEVER use backquotes, 4) if referring to a function, use fn_name() format, 5) try to describe the root cause instead of the consequence of the problem), "severity" (a string: Low, Medium, High, or Critical), "severity_explanation" (a string detailing the reasoning and proof), "preexisting" (a boolean: true if the problem already existed in the codebase before these patches were applied, or false if it was newly introduced by the reviewed patchset), "locations" (an array of objects with file, function_or_symbol, line, code_snippet, and why_this_location_matters). Carry forward the locations from the validated concern; if you gather better evidence, replace vague locations with the most precise verified locations. Do not invent line numbers; use null when exact values are unknown.

Example Output:
```json
{{
  "findings": [
    {{
      "problem": "mm: memory leak in func_x() due to unmet condition Y",
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
            .include_file("stages/verification.md")
            .include_file("false-positive-guide.md")
            .include_file("severity.md")
            .with_var("patch_concerns", |s: &OpenBsdPatchReviewState| {
                serde_json::to_string_pretty(&s.patch_concerns).unwrap_or_default()
            }),
            VERIFICATION.wants_series_context,
        ))
        .output_format(OutputFormat::json())
        .policy(StagePolicy {
            tools: ToolScope::All,
            max_turns,
            temperature,
            ..Default::default()
        })
        .reduce(|state, out: VerificationOutput| {
            let mut new_findings = Vec::new();
            for finding in out.findings {
                let is_preexisting = finding
                    .get("preexisting")
                    .and_then(|v| v.as_bool())
                    .unwrap_or(false);
                if is_preexisting {
                    let concern = json!({
                        "type": finding.get("problem").and_then(|v| v.as_str()).unwrap_or("Pre-existing Issue"),
                        "description": finding.get("problem").and_then(|v| v.as_str()).unwrap_or(""),
                        "reasoning": finding.get("severity_explanation").and_then(|v| v.as_str()).unwrap_or(""),
                        "preexisting": true,
                        "locations": finding.get("locations").cloned().unwrap_or(json!([])),
                    });
                    state.concerns.push(concern);
                }
                new_findings.push(finding);
            }
            state.findings = new_findings;
        })
        .build()
}

pub fn report_stage(max_turns: usize, temperature: f32) -> Stage<OpenBsdPatchReviewState, String> {
    Stage::builder(REPORT.name)
        .system_prompt(openbsd_system_prompt(true))
        .user_prompt(
            PromptTemplate::<OpenBsdPatchReviewState>::new(format!(
                r#"{STAGE_REPORT_INSTRUCTION}

Findings:
{{{{findings}}}}

Return raw text output, not JSON."#
            ))
            .include_file("stages/report.md")
            .include_file("inline-template.md")
            .with_var("findings", |s: &OpenBsdPatchReviewState| {
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
// Complete OpenBSD Review Workflow Graph
// ---------------------------------------------------------------------------

/// Constructs the complete declarative workflow for OpenBSD kernel patch review.
pub fn build_openbsd_patch_review_workflow() -> Workflow<OpenBsdPatchReviewState> {
    build_openbsd_patch_review_workflow_with_options(20, 0.0)
}

/// Constructs the declarative workflow with custom per-stage interaction limits and temperature.
pub fn build_openbsd_patch_review_workflow_with_options(
    max_turns: usize,
    temperature: f32,
) -> Workflow<OpenBsdPatchReviewState> {
    Workflow::builder("openbsd_patch_review")
        .stage(prescreen_stage())
        .dynamic_parallel(
            planning_stage(),
            move |state| resolve_analysis_stages_with_options(state, max_turns, temperature),
            ParallelPolicy::BestEffort,
        )
        .early_exit_if(
            |s| s.all_concerns.is_empty(),
            "No concerns raised in initial analysis stages",
        )
        .stage(deduplication_stage(max_turns, temperature))
        .early_exit_if(
            |s| s.deduplicated_concerns.is_empty(),
            "No concerns remaining after deduplication",
        )
        .stage(conflict_resolution_stage(max_turns, temperature))
        .early_exit_if(
            |s| s.patch_concerns.is_empty(),
            "No concerns remaining after conflict resolution",
        )
        .stage(verification_stage(max_turns, temperature))
        .early_exit_if(
            |s| s.findings.is_empty(),
            "No findings validated in verification stage",
        )
        .stage(report_stage(max_turns, temperature))
        .build()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Each stage's instruction is an `@include` naming that stage's own file.
    ///
    /// A directive naming the wrong file still renders, and renders something
    /// plausible -- another stage's instruction -- so a transposed name would
    /// survive every test that only checks the prompt is non-empty. The
    /// pairing is asserted rather than assumed.
    #[test]
    fn test_each_stage_instruction_names_its_own_file() {
        for stage in ANALYSIS_STAGES {
            assert_eq!(
                stage.instruction,
                format!(r#"@include("stages/{}.md")"#, stage.name),
                "analysis stage {} includes the wrong file",
                stage.name
            );
        }
        for (stage, instruction) in [
            (&DEDUPLICATION, STAGE_DEDUPLICATION_INSTRUCTION),
            (&CONFLICT_RESOLUTION, STAGE_CONFLICT_RESOLUTION_INSTRUCTION),
            (&VERIFICATION, STAGE_VERIFICATION_INSTRUCTION),
            (&REPORT, STAGE_REPORT_INSTRUCTION),
        ] {
            assert_eq!(
                instruction,
                format!(r#"@include("stages/{}.md")"#, stage.name),
                "consolidation stage {} includes the wrong file",
                stage.name
            );
        }
    }

    /// Every `@include` the system prompt writes must be registered with
    /// `include_file()`.
    ///
    /// The renderer only substitutes directives it was told about: it walks the
    /// registered inclusions and replaces each one's directive text. A
    /// directive the template writes but never registers is therefore not an
    /// error and not empty -- it survives verbatim, and the model is handed the
    /// string `@include("identity.md")` where the reviewer persona should be.
    #[tokio::test]
    async fn test_system_prompt_resolves_every_directive() {
        let base = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("prompts/openbsd");
        let rendered = openbsd_system_prompt(true)
            .render_for_model(&OpenBsdPatchReviewState::default(), &base)
            .await
            .expect("system prompt renders");
        assert!(
            !rendered.contains("@include("),
            "unresolved include directive in rendered system prompt:\n{rendered}"
        );
        assert!(
            rendered.contains("expert OpenBSD kernel developer"),
            "identity.md did not reach the prompt: a missing file renders empty, \
             which the directive check above cannot tell from a resolved one"
        );
        assert_eq!(
            rendered.matches("TOOL USAGE").count(),
            1,
            "tool-usage guidance is stated twice: it belongs to the harness and \
             lives in this template, so a prompt set must not restate it"
        );
    }

    #[test]
    fn test_build_openbsd_patch_review_workflow() {
        let workflow = build_openbsd_patch_review_workflow();
        assert_eq!(workflow.name, "openbsd_patch_review");
    }

    /// The name a stage registers under is the name the tables recognise and
    /// label. A stage the progress display cannot label is counted but never
    /// shown, so the lookup is checked for every entry rather than a sample.
    #[test]
    fn test_stage_names_are_resolvable_and_labelled() {
        for stage in ANALYSIS_STAGES {
            assert!(
                analysis_stage_by_name(stage.name).is_some(),
                "analysis stage {} does not resolve",
                stage.name
            );
            assert_eq!(stage_short_label(stage.name), Some(stage.short));
        }
        for stage in CONSOLIDATION_STAGES {
            assert!(
                consolidation_stage_by_name(stage.name).is_some(),
                "consolidation stage {} does not resolve",
                stage.name
            );
            assert_eq!(stage_short_label(stage.name), Some(stage.short));
        }
    }
}
