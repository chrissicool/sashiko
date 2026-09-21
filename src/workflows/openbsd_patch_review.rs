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
    AnalysisStage, ConsolidationStage, LinuxPatchReviewState, POST_VERIFICATION_STAGE_NAMES,
    PlanningOutput, PostVerificationOutput, PrescreenOutput, SERIES_CONTEXT_PLACEHOLDER,
    StageConcernsOutput, VerificationOutput, append_stage_dismissed_concerns_with_prompts,
    append_stage_items_with_prompts, batch_hard_cases_by_severity, collect_stage_prompts,
    enrich_post_verification_output, enrich_verification_output, extra_prompt_paths_for_items,
    extra_prompt_paths_for_state, format_post_verification_feedback,
    format_verification_stage_feedback, record_verified_findings,
    validate_post_verification_batch_output, validate_verification_stage_output,
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

@include("review-core.md")

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
    .include_file("review-core.md")
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

const STAGE_VERIFICATION_INSTRUCTION: &str = r#"@include("stages/verification.md")"#;

const STAGE_POST_VERIFICATION_INSTRUCTION: &str = r#"@include("stages/post-verification.md")"#;

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

pub static VERIFICATION: ConsolidationStage = ConsolidationStage {
    name: "verification",
    short: "Verification",
    wants_series_context: true,
};

/// Stands for the `post-verification-N` stages that hard cases fan out to.
/// Its name is the placeholder the progress display replaces with the stages
/// actually resolved, so it has to match the Linux workflow's.
pub static POST_VERIFICATION: ConsolidationStage = ConsolidationStage {
    name: "post-verification",
    short: "Post-Verification",
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
    &[&VERIFICATION, &POST_VERIFICATION, &REPORT];

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
    if POST_VERIFICATION_STAGE_NAMES.contains(&normalized.as_str()) {
        return Some(&POST_VERIFICATION);
    }
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
            .reduce_with_outcome(
                move |state: &mut OpenBsdPatchReviewState,
                      out: StageConcernsOutput,
                      outcome: &crate::workflow::stage::StageOutcome| {
                    let prompts =
                        collect_stage_prompts(&state.selected_guides, def.guides, outcome);
                    append_stage_items_with_prompts(
                        &mut state.all_concerns,
                        &out.concerns,
                        def.name,
                        "General",
                        &prompts,
                    );
                    append_stage_dismissed_concerns_with_prompts(
                        &mut state.all_dismissed_concerns,
                        &out.dismissed_concerns,
                        def.name,
                        &prompts,
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

/// The finding fields both verification stages emit, with OpenBSD's naming
/// rules for the title.
const FINDING_FIELDS: &str = r#""problem" (a short naming string. BUG NAME RULES: 1) less than 80 characters, 2) preferably start with a short subsystem prefix like 'uvm:', 'netinet:' or 'iwx:', 3) NEVER use backquotes, 4) if referring to a function, use fn_name() format, 5) try to describe the root cause instead of the consequence of the problem), "severity" ("Low", "Medium", "High", "Critical", or "Unknown"), "severity_explanation" (the reasoning and proof), "preexisting" (boolean), and "locations" (an array of objects with file, function_or_symbol, line, code_snippet, and why_this_location_matters)"#;

pub fn verification_stage(
    max_turns: usize,
    temperature: f32,
) -> Stage<OpenBsdPatchReviewState, VerificationOutput> {
    let series_context = series_context_placeholder(VERIFICATION.wants_series_context);
    let user_template = with_series_context(
        PromptTemplate::<OpenBsdPatchReviewState>::new(format!(
            r#"{STAGE_VERIFICATION_INSTRUCTION}

<false_positive_guide>
@include("false-positive-guide.md")
</false_positive_guide>

<severity_guidelines>
@include("severity.md")
</severity_guidelines>@includes

CRITICAL REVIEW DIRECTIVE: To dismiss a concern as a false positive, you must have concrete evidence in the code that proves the concern is invalid. Never drop a raised concern into 'dismissed_concerns' in this stage: every consolidated concern must be placed either in 'findings' (if well-justified with concrete code proof and no competing dismissal) or in 'hard_cases' (if contested, speculative, or requiring tool verification). Also inspect every standalone dismissed_concern: if it dismissed a plausible bug using an unverified assumption, promote it into 'hard_cases' with '"signal_reason": "speculative_dismissal"'.{series_context}

Aggregated Concerns:
{{{{aggregated_concerns}}}}

Aggregated Dismissed Concerns:
{{{{aggregated_dismissed_concerns}}}}

Return ONLY a JSON object with 'findings', 'hard_cases', and 'dismissed_concerns' arrays.
- Each object in 'findings' (Category 1a: well-justified concerns) MUST use the keys: {FINDING_FIELDS}, and may include "stages" (array of stage names) and "prompts" (array of prompt files).
- Each object in 'hard_cases' (Category 2: speculative or contested) MUST use the keys: "type", "description", "estimated_severity" ("Low", "Medium", "High", "Critical", or "Unknown"), "signal_reason" ("mixed_signals", "speculative_concern", "speculative_dismissal", "series_interaction", or "other"), "concern_arguments" (consolidated arguments for why the bug can occur), "dismissal_arguments" (consolidated arguments and snippets from any competing or standalone dismissal, or "" if none), "verification_question" (the specific code question post-verification must answer with tools), "preexisting" (boolean), and "locations" (array of location objects), and may include "stages" and "prompts".
- Each object in 'dismissed_concerns' (Category 1b: well-justified dismissals) MUST use the keys: "type", "description", "reasoning", and "locations", and may include "stages" and "prompts".
Do not invent line numbers; use null when exact values are unknown."#
        ))
        .include_file("stages/verification.md")
        .include_file("false-positive-guide.md")
        .include_file("severity.md")
        .include_files_from_state(|s: &OpenBsdPatchReviewState| {
            extra_prompt_paths_for_state(s, analysis_stage_by_name)
        }),
        VERIFICATION.wants_series_context,
    )
    .with_var("aggregated_concerns", |s: &OpenBsdPatchReviewState| {
        serde_json::to_string_pretty(&s.all_concerns).unwrap_or_default()
    })
    .with_var(
        "aggregated_dismissed_concerns",
        |s: &OpenBsdPatchReviewState| {
            serde_json::to_string_pretty(&s.all_dismissed_concerns).unwrap_or_default()
        },
    );

    Stage::builder(VERIFICATION.name)
        .system_prompt(openbsd_system_prompt(true))
        .user_prompt(user_template)
        .output_format(
            OutputFormat::json()
                .with_validator(validate_verification_stage_output)
                .with_feedback_formatter(format_verification_stage_feedback),
        )
        .policy(StagePolicy {
            tools: ToolScope::All,
            max_turns,
            temperature,
            ..Default::default()
        })
        .reduce_with_outcome(|state, mut out: VerificationOutput, outcome| {
            enrich_verification_output(state, &mut out, outcome, analysis_stage_by_name);
            record_verified_findings(state, out.findings);
            state.hard_cases = out.hard_cases;
            state
                .deduplicated_dismissed_concerns
                .extend(out.dismissed_concerns);
        })
        .build()
}

pub fn post_verification_stage(
    stage_name: &'static str,
    batch: Vec<Value>,
    max_turns: usize,
    temperature: f32,
) -> Stage<OpenBsdPatchReviewState, PostVerificationOutput> {
    let expected_items = batch.len().max(1);
    let candidate_json = serde_json::to_string_pretty(&batch).unwrap_or_default();
    let batch_for_prompts = batch.clone();
    let batch_for_reduce = batch;
    let series_context = series_context_placeholder(POST_VERIFICATION.wants_series_context);
    let user_template = with_series_context(
        PromptTemplate::<OpenBsdPatchReviewState>::new(format!(
            r#"{STAGE_POST_VERIFICATION_INSTRUCTION}

<false_positive_guide>
@include("false-positive-guide.md")
</false_positive_guide>

<severity_guidelines>
@include("severity.md")
</severity_guidelines>@includes

CRITICAL REVIEW DIRECTIVE: To dismiss a candidate issue as a false positive, you must find concrete evidence in the code that proves the issue is invalid and quote that disproving code in `dismissed_concerns[].locations`. If you cannot find concrete proof of safety, you must validate and report the finding in `findings`.{series_context}

Candidate Hard Case(s) to Verify:
{{{{candidate_hard_cases}}}}

Return ONLY a JSON object with 'findings' and 'dismissed_concerns' arrays. Every candidate in this batch MUST be accounted for in either 'findings' (if validated) or 'dismissed_concerns' (if disproved by concrete code); never return both arrays empty.
- Each object in 'findings' MUST use: {FINDING_FIELDS}.
- Each object in 'dismissed_concerns' MUST use: "description" (the candidate issue that was disproved), "reasoning" (step-by-step explanation of how the inspected code disproves the candidate), and "locations" (a non-empty array of objects with file, function_or_symbol, line, code_snippet, and why_this_location_matters, quoting the verbatim disproving code).
Do not invent line numbers; use null when exact values are unknown."#
        ))
        .include_file("stages/post-verification.md")
        .include_file("false-positive-guide.md")
        .include_file("severity.md")
        .include_files_from_state(move |s: &OpenBsdPatchReviewState| {
            extra_prompt_paths_for_items(
                &s.selected_guides,
                &batch_for_prompts,
                analysis_stage_by_name,
            )
        }),
        POST_VERIFICATION.wants_series_context,
    )
    .with_var("candidate_hard_cases", move |_: &OpenBsdPatchReviewState| {
        candidate_json.clone()
    });

    Stage::builder(stage_name)
        .system_prompt(openbsd_system_prompt(true))
        .user_prompt(user_template)
        .output_format(
            OutputFormat::json()
                .with_validator(
                    move |out: &PostVerificationOutput, _: &OpenBsdPatchReviewState| {
                        validate_post_verification_batch_output(out, expected_items)
                    },
                )
                .with_feedback_formatter(format_post_verification_feedback),
        )
        .policy(StagePolicy {
            tools: ToolScope::All,
            max_turns,
            temperature,
            ..Default::default()
        })
        .reduce_with_outcome(move |state, mut out: PostVerificationOutput, outcome| {
            enrich_post_verification_output(
                &state.selected_guides,
                &batch_for_reduce,
                &mut out,
                outcome,
                analysis_stage_by_name,
            );
            record_verified_findings(state, out.findings);
            state
                .deduplicated_dismissed_concerns
                .extend(out.dismissed_concerns);
        })
        .build()
}

/// One post-verification stage per batch of hard cases, batched by severity
/// and named `post-verification-1` onwards. No hard cases, no stages.
pub fn resolve_post_verification_stages_with_options(
    state: &OpenBsdPatchReviewState,
    max_turns: usize,
    temperature: f32,
) -> Vec<Box<dyn ExecutableStage<OpenBsdPatchReviewState>>> {
    batch_hard_cases_by_severity(&state.hard_cases)
        .into_iter()
        .enumerate()
        .map(|(idx, batch)| {
            Box::new(post_verification_stage(
                POST_VERIFICATION_STAGE_NAMES[idx],
                batch,
                max_turns,
                temperature,
            )) as Box<dyn ExecutableStage<OpenBsdPatchReviewState>>
        })
        .collect()
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
        .skip_if(|s| s.skip_report)
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
            |s| s.all_concerns.is_empty() && s.all_dismissed_concerns.is_empty(),
            "No concerns or dismissed concerns raised in initial analysis stages",
        )
        .dynamic_parallel(
            verification_stage(max_turns, temperature),
            move |state| {
                resolve_post_verification_stages_with_options(state, max_turns, temperature)
            },
            ParallelPolicy::BestEffort,
        )
        .early_exit_if(
            |s| s.findings.is_empty(),
            "No findings validated in verification stages",
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
            (&VERIFICATION, STAGE_VERIFICATION_INSTRUCTION),
            (&POST_VERIFICATION, STAGE_POST_VERIFICATION_INSTRUCTION),
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
    /// string `@include("review-core.md")` where the reviewer persona should be.
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
            "review-core.md did not reach the prompt: a missing file renders empty, \
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
