# Design review implementation

Review source: `/home/jdesroches/agent-terminal-design-review-2026-10-08.md`.
Authorised 8 October 2026. No deployment or package installation is included.

## Agreed contract

Codex collaboration operations and worker identities are distinct. The installed
Codex app-server's generated experimental schema verifies receiver thread IDs,
worker status snapshots, worker nickname/role metadata and explicit close/wait
operations. Normalise these into `WorkersUpdated { workers, waiting_for }`.
Each worker has stable ID, optional name/task/activity and a lifecycle:
Starting, Running, Waiting, Completed, Failed, Stopped, Closed or Unknown.
Only verified worker/closure events establish lifecycle. Full replacement events
retain known history; tool-operation completion never closes a worker.

The transcript owns the worker registry and derives active/history summaries.
Header and composer share the same activity description. Explicit worker waits
can coexist with an open main turn; missing reasoning falls back to Working.
Historical/live-unavailable worker activity is settled rather than guessed.

## Ownership

- Engineer: canonical events, Codex adapter and adapter regression fixtures.
- Designer: cards, usage, sub-agent explorer/shelf, model picker, transcript and CSS.
- Accessibility reviewer: composer, settings, onboarding, sidebar and window controls.
- Coordinator: shared transcript/activity integration, chat scaling, tests/gates,
  release build, preview verification and final review.

Changes are reversible through their targeted file diffs. No git reset, existing
session termination, manual install, release publication or live-infrastructure
mutation is required.

## Verification

Reproduce source-level failures with meaningful regressions: Stop with a draft,
Codex late file-change metadata, worker identity/lifecycle and explicit waits,
all-provider usage details, empty/malformed questions, focus and validation.
Run workspace format, clippy with warnings denied, tests on a private display,
and the release build. GUI checks use the managed preview, isolated settings and
synthetic agents. Native screen-reader behaviour is reported separately from
browser canvas snapshots. Never fabricate unavailable usage or worker activity.

## Status

- Contract verified against installed Codex schema; implementation integrated.
- Codex permission-mode changes during an active turn now report the effective
  mode and the requested next-turn mode separately. `Accept edits` sends
  `on-request` + `workspaceWrite` on the next `turn/start`; pending requests on
  an older turn retain their original policy. No automatic approval bypass.
- Workspace regressions: 226 core, 141 kit, 364 application and 6 icon tests
  passed. Local approval-socket tests require temporary socket access outside
  the execution sandbox.
- Native GTK construction/focus/accessibility smoke passed on a private display
  with `GTK_A11Y=test`. New checks exercise real Stop callbacks, completion
  selection, field validation, permission/diff actions, model effort, usage
  scope, worker group transitions and sidebar focus restoration.
- Format, strict workspace Clippy and the release build passed on the final
  source. Native layout acceptance passed at 100% and 200%, including minimum
  width, reachable breakpoint edges, unknown workers, long activity names,
  Accept edits, the sticky approval shelf and the diff panel. The private
  Broadway display caps widths at 1024 px; larger edges were not exercised.
  Compact Deny/Allow targets retain at least 44×44 px. Resizing and text scale
  changes emit no mode choices. A full measurement report is saved at
  `/home/jdesroches/agent-terminal-layout-check-2026-10-08.txt`.
- Engineer, designer and accessibility source reviews passed. Real provider
  prompts and Orca speech output were not used as verification.
- Updated screenshots could not be captured: the browser has a disconnected
  Broadway modal and its dismissal was explicitly rejected. Original review
  screenshots remain linked from the review source; no replacement captures
  are claimed.
- Final result: all authorised local implementation and checks complete.
  The installed application was not replaced; deployment remains the normal
  Debian CI/package pipeline. No live provider prompts, commits or pushes.

## Reproduce the checks

Run these from `/home/jdesroches/git/agent-terminal`:

1. `cargo fmt --all -- --check` — no formatting differences.
2. `cargo clippy --workspace --all-targets --offline -- -D warnings` — no warnings.
3. `cargo test --workspace --offline -q -- --skip test_window_initialization` —
   737 tests pass; temporary local socket access is required for approval tests.
4. `cargo build --release --offline` — produces `target/release/agent-terminal`.

For native checks, use the managed `preview_app` with the test executable
reported by `cargo test --bin agent-terminal --no-run --offline`, passing
`window::imp::tests::test_window_initialization --exact --nocapture --test-threads=1`,
then separately
`window::imp::tests::small_window_fits --exact --ignored --nocapture --test-threads=1`.
Both pass; the latter checks layout and pointer target bounds on a private display.

Undo: the repository was clean at the start. Review `git diff` and selectively
restore only this implementation's listed files; also remove the added synthetic
worker fixture and this record if abandoning the change. No live settings or
installed package require rollback.
