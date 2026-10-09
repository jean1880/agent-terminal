# Automated UX regression plan

Status: implemented and verified locally; live builder adoption remains pending.

The shared gate and its exact native case manifest live in `tests/qa/`. CI and
beta packaging configuration require the same source and native checks; tagged publication depends
on the tested commit. Scripted stdin/stdout provider fixtures cover approval
identity, recovery, cancellation and switching without real accounts or network.
Completed plans now retire from the active shelf into expandable transcript
history, retaining unfinished statuses when interrupted or failed.

Implementation limits: packaged smoke checks provenance, installed bytes, version
and isolated startup; widget/provider regressions run against the source test
binary. Packaged widget exercises remain future work. Native layout cases save
synthetic screenshots before geometry assertions. Failure artefacts contain
synthetic logs, assertion geometry and source identity. The optional real Codex
probe reports compatibility failures explicitly;
it does not change permissions or bypass the sandbox.

Verification checkpoint (2026-10-08): the complete shared gate passed formatting,
Clippy, all 760 standard tests and all seven exact native cases. Its 18 synthetic
screenshots cover styled scrolling, full-window layouts and the six-case content
matrix. Provider regressions reproduced and corrected cancellation, stale scopes
and delayed duplicate requests. Following now stays within the original frame
and viewport thresholds. Eight Python harness/provenance checks and the full
debian-maintainer verification (including 66 tests and Docker builds) also passed.
Evidence is in `target/qa-delivery-verified/gate.json` and adjacent logs/screenshots.

The live beta builder's mounted configuration still uses the previous build-only
steps; applying and reloading the committed configuration requires a separate
approved deployment. The real local Codex 0.161.0 compatibility probe failed at
its app-server socket directory ownership/mode guard before executing the SSH
parse command. This remains a public-release blocker, rather than a successful
compatibility claim. No user permissions or SSH configuration were changed.

## Objective

Catch disappearing approvals, off-screen messages, misleading worker activity
and unrecoverable failures before a beta build reaches a user. Tests must
exercise the path from provider frames through transport, session, storage and
native GTK widgets. Passing parser tests alone does not prove usable UI.

## Verified starting point

- `.github/workflows/ci.yml` runs formatting, Clippy and workspace tests under
  Xvfb on master pushes and pull requests. Beta pushes are not included.
- The release workflow builds and install-tests packages but has no dependency
  on this UX regression suite. Adding tests to CI alone will not gate publication.
- Display-dependent regression tests are marked ignored and consequently do
  not run in the default workspace test command.
- The current PlanPanel in `src/chat/view.rs` reveals the tracker whenever the
  plan is non-empty, regardless of whether every step is completed. This
  explains why a completed plan can keep occupying the active-work area. The
  planned fix must define completed-history access rather than simply delete
  the model's plan. `Transcript::apply(PlanUpdated)` currently replaces the
  latest plan; retaining completed plans needs an explicit history representation
  and tests in addition to changing shelf visibility.
- Existing fixtures, fake backends, temporary stores, GTK accessibility tests,
  viewport bounds helpers and isolated native preview infrastructure can be
  reused. Keep pure checks in agent-core and model tests; use the app layer for
  process, storage and widget integration.
- The installed 3.0.2 approval fix passed two approved host commands in a
  reopened conversation. The subsequent transcript-width fix has passed native
  checks and was pushed as 18994489; installation is user-reported, not yet
  independently checked for this plan.

## Test matrix

| Priority | Gap | Automated scenario | Required outcome |
|---|---|---|---|
| P0 | Reused approval identities | Replay resolved requests 0–3, reconnect a scripted Codex process, issue the same wire IDs on fresh turns/items; repeat twice | Every new request produces a pending card and needs-approval status; answering returns the original typed wire ID and unblocks the fixture command |
| P0 | Approval recovery | Close/reopen a stored thread, switch away/back, cancel a turn while approval is pending, then retry; include numeric and string IDs, questions and duplicate frames | Historical cards remain inert; a genuine duplicate creates no extra card; stale answers cannot approve a new request; the current request stays answerable |
| P0 | Off-screen messages | Replay more than 150 rows, then long command, web-search and file-change titles, an unbroken filename, URL, code block and user message | User-message bounds remain inside the actual viewport; no descendant forces horizontal expansion; long code can scroll within its own region; full text remains inspectable |
| P0 | Following and reading | Append a user prompt and stream a reply; scroll upward, continue streaming, prepend older rows, resize, jump to latest | Following catches up within one settled frame; reading retains its visible anchor; prepending loses no rows; explicit jump/send restores following |
| P0 | Visible execution failures | Fixture denies access, returns an SSH configuration error, fails to spawn, exits unexpectedly or disconnects while a command/approval is pending | Error and actionable status appear; pending cards expire appropriately; draft/history survive; retry can complete a fresh turn |
| P0 | Stale plan tracker | Emit pending/in-progress steps, complete the final step during a turn, finish the turn, reopen history, then start a new plan; also cancel/fail with incomplete steps | Completion updates immediately; a wholly completed plan stops occupying the active-work shelf and remains accessible in history; cancellation/failure does not falsely complete steps; the next plan replaces the active tracker without reviving the old one |
| P1 | Worker counts and waits | Two active workers, one completion, explicit closure, failure, interruption, restart and sparse updates; replay after reconnect | Counts match active workers; completed and closed remain distinct; names/tasks survive; unknown status is not reported as running; explicit worker waits clear correctly |
| P1 | Cross-agent flow | Codex → Claude → Codex using scripted providers, including a failed switch and a late result from the previous process | Correct provider receives responses; old results cannot change the new session; failed switches recover; mode and draft remain coherent |
| P1 | Permission policy | Request a mode change during a turn, approve/deny a command, start the next turn; change a user default that the application overrides | UI shows the effective policy and pending change; wire policy matches it; next turn adopts the new mode; no approval is silently bypassed |
| P1 | Accessibility | Keyboard navigation to pending card, Allow/Deny, question choices, jump-to-latest and worker details | Focus remains visible and reachable; accessible names/states/relations are correct; approval targets meet 44×44 geometry; resolving a card does not cause an unsolicited scroll jump |

## Native layout coverage

Use fixed synthetic content and the bundled CSS. Cover 360, 560 and 950 pixel
window widths at 100% and 200% chat text size, with sidebar/diff drawer open and
closed. Test reachable header breakpoints immediately below and above each
threshold without taking a full Cartesian product. Reproduce the real Paned →
chat → viewport hierarchy, not only standalone cards.

Assert widget bounds in viewport coordinates, clamp allocations and actual last
row visibility. For older-history anchoring, compare the same row's visible
position before and after prepend/stream operations. Also assert persisted and
materialised message order/text, so clipping is distinguishable from lost data.
Do not use platform-dependent pixel equality as the primary pass criterion.

## Implementation sequence

1. **Make existing regressions run automatically.** Add `beta/**` push coverage
   and workflow_dispatch to CI. Run the two relevant ignored native tests
   explicitly, each in a separate process, under Xvfb. Keep the default suite,
   formatting and Clippy. Do not run every ignored test indiscriminately:
   measurement-only tests are not release gates.
   Extract a reusable gate that the tag release workflow also requires before
   its Publish job. Require the same tested commit for beta delivery through
   debian-maintainer, either by checking its successful CI result or running
   the shared gate before packaging; branch coverage alone is not delivery gating.
2. **Build the scripted provider integration fixture.** Reuse existing transport
   and temporary-store test helpers. Add a deterministic, repository-owned
   fixture process only where the current fake backend cannot exercise actual
   stdin/stdout, process exit and reconnect. It implements just the necessary
   Codex handshake/thread/turn/approval protocol; responses come from checked-in
   fixtures, with no model calls. Assert outgoing frames as well as visible UI.
3. **Add the P0 recovery and layout scenarios.** Extend the existing approval
   reconnect, approval round-trip, newest-row and long-title tests. Add process
   generation/stale-response cases, completed-plan shelf cleanup and controlled
   layout changes. Demonstrate
   that the existing two regressions fail against their pre-fix source.
4. **Add P1 lifecycle and accessibility scenarios.** Reuse worker schema fixtures
   and GTK's test accessibility backend. Use scripted alternate providers for
   agent switches. Add focused assertions rather than broad snapshot copying.
5. **Verify delivery separately.** After the source gates pass, build packages
   through the existing CI/debian-maintainer pipeline. Add an opt-in packaged
   smoke job on a supported runner, using synthetic threads and fixture
   providers. Assert the executable/package version and tested commit before
   claiming that a deployed version passed. Never interact with a user's
   running app or manually install over their package.

## Execution and artefacts

Fast tests must need no display, credentials, network or live agents. Native
tests run on a private display with temporary settings, data, state and store;
fixture processes receive a minimal explicit environment. Do not seed real
conversation history or copy user tokens. Local interactive inspection uses
the preview MCP; CI native assertions use Xvfb.

Run GTK scenarios in separate test processes to avoid GTK main-thread/global
state conflicts. Use observable completion and frame/layout conditions instead
of sleeps. Bound individual asynchronous waits and whole jobs; a timeout is a
failure, not a reason to approve automatically or silently retry until green.
Bound approval waiting separately from command execution: a shell timeout
cannot time out a command that has not started.

On failure, upload the synthetic fixture transcript, test name, process exit
code, widget/scroll geometry and package/commit identity. Save a native screenshot
when the supported runner can capture one; missing screenshot capture must not
hide an assertion failure. Keep logs free of real credentials and conversation
text. Metadata must distinguish awaiting approval, executing and disconnected.

Representative existing commands, run separately:

```sh
cargo fmt --all -- --check
cargo clippy --workspace --all-targets -- -D warnings
xvfb-run -a cargo test --workspace --all-targets
xvfb-run -a cargo test -p agent-terminal chat::view::tests::long_tool_titles_keep_user_messages_inside_the_viewport -- --exact --ignored --nocapture
xvfb-run -a cargo test -p agent-terminal chat::view::tests::the_newest_row_stays_in_view_while_following -- --exact --ignored --nocapture
```

## Release criteria

- Master/PR/beta CI execute the P0 suite; ignored display tests are selected
  explicitly and their non-zero test counts are checked.
- All P0 assertions pass with no unexplained flakes, hidden approvals or lost
  messages. Quarantining a P0 test does not clear its blocker.
- P1 failures have an explicit disposition before public release; tests must
  not label unavailable worker telemetry as success.
- Packaged verification passes independently of source tests. A healthy build
  alone is not evidence that the user is running the tested executable.
- The known sandbox SSH ownership incompatibility is represented by a fixture
  failure and recovery case. Keep a separate opt-in real-Codex compatibility
  probe on Linux: SSH configuration parse inside versus approved outside the
  sandbox. Invoke the installed Codex sandbox/command execution facility directly
  without an assistant turn, using synthetic settings/host data and no network,
  tokens or paid inference. Gate that runner's compatibility claim on the probe;
  do not make ordinary CI depend on GitHub credentials or a paid model.

Rollback: the application and builder configuration are separate repository
commits. Revert the affected commit
if necessary while preserving stored plan history and retaining the associated
release blocker until equivalent behaviour and coverage are restored. No
production permission change is part of this plan.
