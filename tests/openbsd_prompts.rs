//! Smoke test that the OpenBSD prompt set is complete and loads cleanly.
//!
//! This does not run a review (no AI); it only confirms the directory passes
//! the same validation the worker performs, and that the files the review
//! actually renders -- the stage instructions -- come out of the OpenBSD
//! directory rather than from anywhere else.

use sashiko::worker::prompts::PromptRegistry;
use sashiko::workflow::PromptTemplate;
use std::path::PathBuf;

fn openbsd_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("third_party/prompts/openbsd")
}

#[test]
fn openbsd_prompt_directory_validates() {
    PromptRegistry::new(openbsd_dir())
        .validate_prompt_directory()
        .expect("OpenBSD prompt directory should pass validation");
}

#[tokio::test]
async fn openbsd_stage_prompts_load() {
    let base = openbsd_dir();
    for n in 1u8..=11 {
        let path = format!("stages/stage-{n}.md");
        let rendered = PromptTemplate::<()>::new(format!("@include(\"{path}\")"))
            .include_file(path)
            .render_for_model(&(), &base)
            .await
            .unwrap_or_else(|e| panic!("OpenBSD stage {n} failed to render: {e}"));

        assert!(
            !rendered.contains("@include("),
            "OpenBSD stage {n} did not resolve: {rendered}"
        );
        assert!(
            rendered.contains(&format!("Stage {n}")),
            "OpenBSD stage {n} content missing its heading: {rendered}"
        );
        // The predecessor of this test called get_stage_prompt(), which returned
        // hardcoded Linux constants whatever the base directory was -- so it
        // passed while reading no OpenBSD file at all. Assert the text is the
        // OpenBSD one, which is what that mistake could not have satisfied.
        assert!(
            !rendered.contains("Linux"),
            "OpenBSD stage {n} rendered Linux text: {rendered}"
        );
    }
}
