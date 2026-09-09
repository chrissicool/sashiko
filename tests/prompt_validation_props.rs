//! Property test: directory-validation completeness.
//!
//! For any subset of the required files (`identity.md`, `stages/prescreen.md`,
//! `stages/stage-{1..11}.md`)
//! that is missing, `validate_prompt_directory` returns an error identifying the
//! missing file(s); for a complete directory it succeeds. Identity is checked
//! before the stage files, so when identity is absent the error names identity
//! and need not also enumerate the stages.

use proptest::prelude::*;
use sashiko::worker::prompts::PromptRegistry;

fn write_complete(dir: &std::path::Path, omit_identity: bool, omit_stages: &[u8]) {
    if !omit_identity {
        std::fs::write(dir.join("identity.md"), "identity").unwrap();
    }
    let stages = dir.join("stages");
    std::fs::create_dir_all(&stages).unwrap();
    std::fs::write(stages.join("prescreen.md"), "prescreen").unwrap();
    for n in 1..=11u8 {
        if !omit_stages.contains(&n) {
            std::fs::write(stages.join(format!("stage-{n}.md")), "x").unwrap();
        }
    }
}

proptest! {
    #[test]
    fn validation_reports_every_missing_required_file(
        omit_identity in any::<bool>(),
        omit_stages in proptest::collection::vec(1u8..=11, 0..=11),
    ) {
        let dir = tempfile::tempdir().unwrap();
        write_complete(dir.path(), omit_identity, &omit_stages);
        let reg = PromptRegistry::new(dir.path().to_path_buf());
        let res = reg.validate_prompt_directory();

        if omit_identity {
            let err = res.expect_err("missing identity must be fatal").to_string();
            prop_assert!(err.contains("identity.md"), "error: {err}");
        } else if !omit_stages.is_empty() {
            let err = res.expect_err("missing stages must be fatal").to_string();
            for n in &omit_stages {
                prop_assert!(
                    err.contains(&format!("stage-{n}.md")),
                    "stage {n} missing but not reported: {err}"
                );
            }
        } else {
            prop_assert!(res.is_ok(), "complete dir must validate: {res:?}");
        }
    }
}
