# Design: OpenBSD Kernel Review Support

## Context

Sashiko's review pipeline was built for the Linux kernel: the eleven review
stage prompts, the reviewer identity, and the Phase 0 pre-screening text were
hardcoded in `src/worker/prompts.rs` with Linux-specific vocabulary (LKML, KNF's
absence, Linux APIs and subsystem layout). Reviewing OpenBSD changes needs a
different prompt set — Kernel Normal Form (style(9)), OpenBSD kernel APIs
(`pool_get`, `rwlock`/`mutex`/`spl`, `tsleep`, `timeout_add`/`task_add`,
`refcnt`, `bus_dmamap_sync`, byte-order helpers, queue(3) macros), a single
source tree with no MAINTAINERS file, and a plain-text tech@ mailing-list reply
style — without forking the binary or the pipeline.

The goal of this work is to make the prompt set a swappable, file-backed asset
selected at runtime, and to ship a complete (draft) OpenBSD prompt set, so the
existing `review` binary can check patches against the OpenBSD source tree
standalone.

## Design Decisions

- **File-backed prompts, not hardcoded strings.** All stage instructions, the
  reviewer identity, and the per-stage attachments now live as files under a
  prompt directory. The binary chooses a directory at runtime; nothing about a
  prompt set is compiled in. This is what lets Linux and OpenBSD coexist.
- **Byte-identical refactor.** The extraction of the Linux prompts was verified
  byte-for-byte against goldens captured from the previous hardcoded
  implementation, so moving to files changed no Linux behaviour.
- **One prompt directory per project, fully self-contained.** A `PromptRegistry`
  resolves every path relative to its base directory and refuses to read outside
  it, so a Linux review can never read OpenBSD prompt files or vice versa.
- **Declarative per-stage attachments.** Which guidance files a stage loads is
  declared in `stages/manifest.toml`, so the two prompt sets can attach
  different files with no code change.
- **Source-tree location stays in config.** The OpenBSD source tree is selected
  via `git.repository_path` in `Settings.toml` (or the `SASHIKO__GIT__REPOSITORY_PATH`
  environment variable). A dedicated command-line override is intentionally
  deferred.

## Architecture

### PromptRegistry (refactored)

`PromptRegistry { base_dir: PathBuf }` assembles prompt content from the base
directory. The relevant methods:

- `build_context(selected_prompts)` — reads `identity.md`, then appends the
  selected `subsystem/` and `patterns/` guides, wrapped in
  `<global_review_guidelines>`. The current-date line is generated in code; the
  identity text comes from the file.
- `get_stage_prompt(stage)` — reads `stages/stage-{N}.md` (N in 1..=11) and
  appends the attachments declared for that stage in the manifest.
- `load_manifest()` — parses `stages/manifest.toml` on demand (the file is
  tiny). Missing is empty; malformed is fatal.
- `get_all_manifest_attachments()` — basenames of every manifest attachment,
  used so a guide loaded per-stage is not also loaded into the Phase 0 shared
  context.
- `validate_prompt_directory()` — startup check that the directory is complete.

Stage instructions and attachments are read from disk; the Phase 0 pre-screening
prompt was made project-agnostic ("preparing a system software patch review").

### OpenBSD prompt directory

```text
third_party/prompts/openbsd/
├── identity.md                 # reviewer persona: OpenBSD, KNF, single tree
├── callstack.md                # how to trace OpenBSD call paths (attached: stage 3)
├── technical-patterns.md       # OpenBSD anti-patterns (attached: stage 3)
├── false-positive-guide.md     # common false positives (attached: stage 10)
├── severity.md                 # severity rubric (attached: stage 10)
├── inline-template.md          # tech@ reply format (attached: stage 11)
├── stages/
│   ├── manifest.toml           # per-stage attachment declarations
│   └── stage-1.md … stage-11.md
└── subsystem/                  # Phase 2: subsystem.md index + per-subsystem guides
```

The Linux set under `third_party/prompts/kernel/` has the same shape.

### Stage manifest schema

`stages/manifest.toml` maps a stage number to the files it loads in addition to
its own `stage-{N}.md`:

```toml
[stage.3]
attachments = ["callstack.md", "technical-patterns.md"]

[stage.10]
attachments = ["false-positive-guide.md", "severity.md"]

[stage.11]
attachments = ["inline-template.md"]
```

Deserialized as:

```rust
#[derive(Debug, Deserialize, Default)]
#[serde(deny_unknown_fields)]
pub struct StageManifest {
    #[serde(default)]
    pub stage: HashMap<String, StageConfig>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StageConfig {
    pub attachments: Vec<String>,
}
```

`deny_unknown_fields` makes an unrecognized key a fatal error. The `toml` crate
only yields string table keys, so keys are stored as `String` and validated to
be `u8` in `load_manifest`: a non-numeric or overflowing key (e.g. `300`) is a
fatal schema error, while a valid-but-unused key (e.g. `50`) is accepted and
never queried, since stages are only requested for 1..=11. Attachment order is
significant and preserved.

### `--prompts` CLI flag integration

The `review` binary's `--prompts` flag (default `third_party/prompts/kernel`)
selects the directory. At startup the binary:

1. constructs `PromptRegistry::new(args.prompts)` and calls
   `validate_prompt_directory()`, exiting non-zero with a clear message if the
   directory is missing or incomplete;
2. anchors the optional `read_prompt` tool to the same directory;
3. uses the registry for `build_context` and all eleven `get_stage_prompt`
   calls.

So `review --prompts third_party/prompts/openbsd` runs an OpenBSD review and
never touches the kernel prompt directory. Relative `--prompts` paths resolve
against the current working directory.

The baseline (the git revision the patch is compared against) is resolved with
`git_ops::get_commit_hash`. An unset `--baseline` defaults to HEAD; an explicit
`--baseline` that cannot be resolved logs a warning and falls back to HEAD
rather than aborting. The `review` binary never constructs a `BaselineRegistry`,
so it never reads MAINTAINERS — exactly the single-tree behaviour OpenBSD needs.

## Error Handling

- **Missing prompt directory / missing `identity.md` / missing stage files** —
  `validate_prompt_directory()` reports the offending absolute path(s), listing
  every missing stage file together, and the binary exits non-zero before any
  review work.
- **Malformed `stages/manifest.toml`** (invalid TOML, unknown key, non-`u8`
  stage key) — fatal, naming the manifest path.
- **Missing manifest** — treated as "no attachments", not an error.
- **Missing attachment file** — a warning is logged and the attachment skipped;
  the remaining attachments still load.
- **Empty / whitespace-only / absent `identity.md` during context build** — a
  warning is logged and an empty identity is used; turning a missing identity
  into a hard error is solely `validate_prompt_directory`'s job.
- **Path traversal** — an attachment path containing `..`, or one that
  canonicalizes outside the base directory (e.g. via a symlink), is rejected
  with an error naming the out-of-bounds reference.
- **Unresolvable explicit `--baseline`** — warn and fall back to HEAD; an unset
  baseline that fails to resolve is a genuine repository error and propagates.

## Status

Phase 1 — file-backed prompts, the OpenBSD prompt directory (identity, stages,
manifest, shared assets), and the CLI wiring — is implemented. Phase 2 adds the
OpenBSD `subsystem/` guides loaded via Phase 0 pre-screening. The OpenBSD prompt
content is a first draft (each file carries a TODO marker) pending review
against `share/man/man9` and current `sys/` practice. Daemon-level auto-routing
between Linux and OpenBSD is out of scope.
