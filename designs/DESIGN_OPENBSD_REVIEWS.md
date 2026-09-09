# Design: OpenBSD Kernel Review Support

## Context

Reviewing OpenBSD changes needs its own prompt set and its own review workflow.
The tree has Kernel Normal Form (style(9)); its own kernel APIs (`pool_get`,
`rwlock`/`mutex`/`spl`, `tsleep`, `timeout_add`/`task_add`, `refcnt`,
`bus_dmamap_sync`, the byte-order helpers, the queue(3) macros); a single source
tree with no MAINTAINERS file; and a review etiquette of plain-text replies to
the tech@ mailing list. None of that survives translation from a Linux review.

The reviewed codebase is an explicit dimension, `ProjectId`. OpenBSD is one of
its values, alongside Linux and Sashiko, and supplies its own workflow module,
prompt set and output validators.

## Design Decisions

- **A project, not a prompt override.** OpenBSD is `ProjectId::OpenBsd` with its
  own workflow module. What distinguishes a project is more than prompt text:
  the stage table, the report format and the output validators all differ, and a
  module is what can carry them. A directory override would leave those in the
  Linux workflow, where OpenBSD cannot change them without changing Linux too.
- **File-backed stage instructions.** The stage instructions, the reviewer
  identity and the per-stage guides are files under the prompt set, reached by
  `@include`. They are the part that gets iterated on, and editing a Markdown
  file to retune a review beats editing a string constant.
- **What describes the harness stays in Rust.** The turn budget and the
  `truncated` field of the tool protocol are properties of this program, not of
  a project. They live in the workflow's system prompt template. A prompt set
  that restated them would drift the first time the protocol changed.
- **No MAINTAINERS.** `ProjectId::OpenBsd::uses_maintainers()` is false. OpenBSD
  has no MAINTAINERS file, so no index is built and nothing downstream of one --
  subsystem attribution, maintainer-derived access -- applies. Access comes from
  the `[server.acl]` lists.
- **Source-tree location stays in config.** The OpenBSD source tree is selected
  via `git.repository_path` in the settings file, or
  `SASHIKO__GIT__REPOSITORY_PATH`.

## Architecture

### Project selection

`ProjectId::OpenBsd` reports `as_str() == "openbsd"` and
`prompt_dir() == "openbsd"`. Three places name the project, and they must agree:

- `--project openbsd` on the command line. The variant is pinned with
  `#[value(name = "openbsd")]`; clap would otherwise spell it `open-bsd` while
  serde and `FromStr` both read `openbsd`.
- `[project] kind = "openbsd"` in the settings file, checked against the
  selected project. A configuration naming one project and used for another is
  pointing at the wrong database.
- The database, stamped on first open by `ensure_project_stamp`. A mismatch is
  fatal: one project per instance, per database, per port.

The daemon spawns each review worker with `--project` and no `--prompts`, into a
cleared environment. The project selector therefore decides which prompt set a
review loads. `--prompts` remains available to point a single run at a directory
on disk.

### The prompt bundle

`build.rs` compiles two roots into one bundle namespace: the vendored
`third_party/prompts/`, whose `kernel/` set is pinned by a `REVISION` file, and
the first-party `prompts/`. A relative path appearing in both is a build error.
At runtime the bundle is installed under the XDG data directory, keyed by
revision, and `prompt_bundle::project_prompts_path(project)` resolves a
project's directory within it.

The OpenBSD set is first-party, so it lives at `prompts/openbsd/`, a sibling of
`prompts/sashiko/` rather than an edit to the vendored kernel snapshot.

### OpenBSD prompt set

```text
prompts/openbsd/
├── identity.md                 # reviewer persona: OpenBSD, KNF, single tree
├── callstack.md                # tracing OpenBSD call paths      (execution-flow)
├── technical-patterns.md       # OpenBSD anti-patterns           (execution-flow)
├── false-positive-guide.md     # common false positives          (verification)
├── severity.md                 # severity rubric                 (verification)
├── inline-template.md          # tech@ reply format              (report)
├── stages/
│   ├── prescreen.md
│   └── goal.md, implementation.md, execution-flow.md, resources.md,
│       locking.md, security.md, hardware.md, deduplication.md,
│       conflict-resolution.md, verification.md, report.md
└── subsystem/                  # subsystem.md index + per-subsystem guides
```

Stage files are named for the stage they serve, which the stage table already
names, so `analysis_stage()` derives the path from the stage name rather than
restating the list.

### `openbsd_patch_review`

The module owns its stage tables, its system prompt and its output validators.
It shares the execution state -- `OpenBsdPatchReviewState` is an alias for
`LinuxPatchReviewState` -- and the stage output structs: a review carries the
same shape of data whatever the project, and only the prompts and the stage
table differ.

Seven analysis stages fan out -- goal, implementation, execution-flow,
resources, locking, security, hardware -- followed by four consolidation stages:
deduplication, conflict-resolution, verification, report. A pre-screen stage
selects subsystem guides from `subsystem/subsystem.md`, and a planning stage may
narrow the fan-out.

`workflows::stage_short_label`, `default_stage_count` and `planned_stages_from`
dispatch on `ProjectId`, so the progress display counts this workflow's stages.

### Reports

The report stage renders `stages/report.md` with `inline-template.md` attached.
Its output is checked structurally in Rust: plain text, no code fences, `> `
quoted context, a `commit <hash>` header and an `Author:` line. Rejection
feedback names `inline-template.md`, so the prompt set decides what the format
is.

### Pre-existing problems

A problem the patch did not introduce is reported with the review. The
verification stage states it explicitly ("This problem wasn't introduced by this
patch, but...") and filters by severity: Low- and Medium-severity pre-existing
problems are discarded, and only High and Critical are reported. The report
stage flags each one inline, so a tech@ reader sees it is not the submitter's
doing.

tech@ has no bug-tracker handoff. A pre-existing problem worth raising is worth
raising in the reply, and one not worth the reply is not worth filing either.
The severity filter is what keeps such findings from burying the patch under
unrelated noise.

Findings that still carry `preexisting` are additionally handed to `linux_bug`
by the daemon. That is left alone: the finding is already in the report, so the
handoff duplicates it rather than diverting it.

### Baselines

OpenBSD is a single tree with no MAINTAINERS, so `BaselineRegistry` offers
linux-next only for trees that parsed one, leaving the local HEAD fallback
directly reachable. An explicit `--baseline` that cannot be resolved warns and
falls back to HEAD; an unset baseline that fails to resolve is a genuine
repository error and propagates.

## Error Handling

- **A missing `@include` renders empty rather than failing.** This is what lets
  an optional guide be absent, and what lets subsystem guides be added one at a
  time to a workflow that already runs. The cost is that a *required* file going
  missing is silent, so it is caught by test instead: `tests/openbsd_prompts.rs`
  asserts the set carries `identity.md`, `stages/prescreen.md` and a file for
  every stage the tables name.
- **An unregistered directive survives verbatim.** The renderer substitutes only
  the inclusions a template registered with `include_file()`, so an `@include`
  written into a template but never registered reaches the model as literal
  text. A test renders the system prompt and asserts no directive survives, and
  that the identity actually arrived -- a missing file renders empty, which the
  directive check alone cannot distinguish from a resolved one.
- **Path traversal.** An inclusion path that is absolute or contains a
  parent-directory component is refused by the renderer, the sink every include
  passes through. Guide names chosen by the model during pre-screening are
  filtered at the source by `guard::sanitize_guide_name`.
- **Project mismatch.** A settings file naming a different project, or a
  database stamped for one, is fatal before any review work.

## Status

The OpenBSD prompt set, the `openbsd_patch_review` workflow,
`ProjectId::OpenBsd` and its dispatch are implemented. The prompt content is a
first draft pending review against `share/man/man9` and current `sys/`
practice.

`linux_bug` is Linux-only: its prompts are Linux-worded, and a project that
attributes no MAINTAINERS sections files bugs with no subsystems, visible only
to the `admins` and `security` ACL lists. The Sashiko project reports
`uses_maintainers() == false` and runs the same path, so this is not specific to
OpenBSD, and pre-existing problems reach their readers through the review
regardless.

Open, if that handoff is ever wanted in earnest: what subsystem attribution and
bug access mean for a tree without MAINTAINERS. The source path
(`sys/dev/pci/`, `sys/uvm/`) is the natural analogue, and `[server.acl]` is
where access is decided.
