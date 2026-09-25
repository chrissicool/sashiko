//! Smoke test that the OpenBSD prompt set is complete and loads cleanly.
//!
//! This does not run a review (no AI); it only confirms every file a review
//! renders is present, and that the files the review actually renders -- the
//! stage instructions -- come out of the OpenBSD directory rather than from
//! anywhere else.

use sashiko::workflow::PromptTemplate;
use sashiko::workflows::openbsd_patch_review::{
    ANALYSIS_STAGES, CONSOLIDATION_STAGES, is_stage_exclusive_guide,
};
use std::path::PathBuf;

/// The stage instruction files a review renders, named for the stage each one
/// belongs to.
///
/// Read from the workflow's own tables rather than restated here, so a stage
/// added to the workflow cannot escape this check: the file it needs is
/// required the moment the table names it, instead of leaving an unresolved
/// directive in a live prompt.
fn stage_names() -> Vec<&'static str> {
    ANALYSIS_STAGES
        .iter()
        .map(|s| s.name)
        .chain(CONSOLIDATION_STAGES.iter().map(|s| s.name))
        .collect()
}

fn openbsd_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("prompts/openbsd")
}

/// An `@include` naming a file that is absent resolves to nothing rather than
/// failing, which is right for an optional guide but would let a review run
/// with a silently empty stage instruction. Asserting the set is complete is
/// what turns that into a test failure instead of a quiet, useless review.
#[test]
fn openbsd_prompt_set_is_complete() {
    let base = openbsd_dir();
    let mut missing = Vec::new();
    for required in ["identity.md", "stages/prescreen.md"] {
        if !base.join(required).is_file() {
            missing.push(required.to_string());
        }
    }
    for stage in stage_names() {
        let rel = format!("stages/{stage}.md");
        if !base.join(&rel).is_file() {
            missing.push(rel);
        }
    }
    assert!(
        missing.is_empty(),
        "OpenBSD prompt set is missing: {missing:?}"
    );
}

#[tokio::test]
async fn openbsd_stage_prompts_load() {
    let base = openbsd_dir();
    for stage in stage_names() {
        let path = format!("stages/{stage}.md");
        let rendered = PromptTemplate::<()>::new(format!("@include(\"{path}\")"))
            .include_file(path)
            .render_for_model(&(), &base)
            .await
            .unwrap_or_else(|e| panic!("OpenBSD stage {stage} failed to render: {e}"));

        assert!(
            !rendered.contains("@include("),
            "OpenBSD stage {stage} did not resolve: {rendered}"
        );
        assert!(
            rendered.trim_start().starts_with('#'),
            "OpenBSD stage {stage} content missing its heading: {rendered}"
        );
        // Linux text in a rendered OpenBSD stage means the include resolved
        // against some other prompt set, which a check for non-empty output
        // cannot tell from a correct one.
        assert!(
            !rendered.contains("Linux"),
            "OpenBSD stage {stage} rendered Linux text: {rendered}"
        );
    }
}

/// The index and the guide directory agree in both directions.
///
/// A row naming a file that is not there selects nothing: the include resolves
/// to empty and the review runs without the guidance it asked for. A guide with
/// no row cannot be selected, because pre-screening is the only path that
/// offers one. The exception is a guide a stage attaches itself: the pre-screen
/// drops those selections, so a row for one would offer a choice that is
/// discarded.
#[test]
fn openbsd_subsystem_index_matches_the_guides() {
    let dir = openbsd_dir().join("subsystem");
    let index = std::fs::read_to_string(dir.join("subsystem.md")).expect("subsystem index");

    let mut listed: Vec<String> = Vec::new();
    for line in index.lines() {
        let row = line.trim();
        if !row.starts_with('|') || !row.ends_with(".md |") {
            continue;
        }
        let file = row
            .trim_end_matches('|')
            .trim_end()
            .rsplit(' ')
            .next()
            .expect("a row ends with its file")
            .to_string();
        assert!(
            dir.join(&file).is_file(),
            "the index names a guide that is not there: {file}"
        );
        listed.push(file);
    }
    assert!(!listed.is_empty(), "the index lists no guides at all");

    for entry in std::fs::read_dir(&dir).expect("subsystem directory") {
        let name = entry
            .expect("entry")
            .file_name()
            .to_string_lossy()
            .into_owned();
        if name == "subsystem.md" || !name.ends_with(".md") {
            continue;
        }
        assert!(
            listed.contains(&name) || is_stage_exclusive_guide(&name),
            "{name} is in neither the index nor a stage table, so nothing can load it"
        );
    }
}
