//! Property test: graceful degradation for optional content.
//!
//! A prompt set only has to carry the files the render actually names. As long
//! as those exist -- identity.md and the stage file being requested -- the
//! system prompt and the stage prompt render regardless of whether the optional
//! pieces (the subsystem and patterns directories) are present.

use proptest::prelude::*;
use sashiko::worker::kernel_workflow::{KernelReviewState, kernel_system_prompt};
use sashiko::workflow::PromptTemplate;

proptest! {
    #[test]
    fn optional_components_may_be_absent(
        with_subsystem in any::<bool>(),
        with_patterns in any::<bool>(),
    ) {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("identity.md"), "identity").unwrap();
        let stages = dir.path().join("stages");
        std::fs::create_dir_all(&stages).unwrap();
        std::fs::write(stages.join("stage-1.md"), "# Stage 1").unwrap();
        if with_subsystem {
            std::fs::create_dir_all(dir.path().join("subsystem")).unwrap();
        }
        if with_patterns {
            std::fs::create_dir_all(dir.path().join("patterns")).unwrap();
        }

        let rt = tokio::runtime::Runtime::new().unwrap();
        let base = dir.path();

        prop_assert!(
            rt.block_on(
                kernel_system_prompt(true).render_for_model(&KernelReviewState::default(), base)
            )
            .is_ok()
        );
        prop_assert!(
            rt.block_on(
                PromptTemplate::<()>::new("@include(\"stages/stage-1.md\")")
                    .include_file("stages/stage-1.md")
                    .render_for_model(&(), base)
            )
            .is_ok()
        );
    }
}
