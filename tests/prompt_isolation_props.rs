//! Property test: prompt-directory isolation.
//!
//! A prompt must only ever read files inside its configured base directory.
//! This exercises the inclusion path resolved from state — the one place a
//! model-supplied string picks which file is read, since the pre-screen stage
//! names the subsystem guides and the workflow joins those names onto the
//! prompt directory — and asserts that any name using parent-directory
//! traversal is rejected rather than read, regardless of the traversal depth
//! or leaf name.

use proptest::prelude::*;
use sashiko::workflow::PromptTemplate;
use std::path::PathBuf;

proptest! {
    #[test]
    fn parent_traversal_guides_are_always_rejected(
        depth in 1usize..=5,
        leaf in "[a-z]{1,8}",
    ) {
        let guide = format!("{}{leaf}.md", "../".repeat(depth));

        let dir = tempfile::tempdir().unwrap();
        // Nest the base one level down so a "../" actually has somewhere to go.
        let base = dir.path().join("base");
        std::fs::create_dir_all(&base).unwrap();
        // A real file outside the base: traversal must be refused even when the
        // target exists and would otherwise read cleanly.
        std::fs::write(dir.path().join(format!("{leaf}.md")), "secret").unwrap();

        let tmpl = PromptTemplate::<Vec<String>>::new("body")
            .include_files_from_state(|s: &Vec<String>| s.iter().map(PathBuf::from).collect());
        let state = vec![guide.clone()];

        let rt = tokio::runtime::Runtime::new().unwrap();
        let res = rt.block_on(tmpl.render_for_model(&state, &base));
        prop_assert!(
            res.is_err(),
            "traversal guide {guide} was not rejected"
        );
    }
}
