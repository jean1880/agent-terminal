# Cross-agent runtime verification

The implementation uses the existing standalone executable, adapters, approval
policies and single store writer. It adds no installed daemon, listener port or
system-library requirement. The separate homelab `cross-agent` CLI is unchanged.

## Evidence types

- [Installed-provider evidence](delegation-route-evidence.md): actual Claude and
  Codex CLIs against deterministic local model fixtures, plus actual agy cloud
  enforcement/attestation with a read-only native authentication bind. These are
  labelled separately and do not claim a complete cloud GUI route.
- Supervisor tests use real scripted provider subprocesses and SQLite to exercise
  bootstrap, authentication, durable operations, cancellation, replay and child
  publication. Test route injection is private to the test harness.
- [Bridge checks](delegation-discovery/bridge-implementation.md) exercise the pinned
  SDK, protocol lifecycle, cancellation, reconnect limits and transport budgets.
- [Worker UI checks](delegation-discovery/worker-view-implementation.md) cover scoped
  history, stale controls, approvals and diffs on a private display.
- [Registration hardening](delegation-discovery/registration-hardening.md) covers
  private recovery, ownership, corrupt journals, selected environments and the
  actual headless shim diagnostic.
- [Window transfer checks](delegation-discovery/window-transfer-regressions.md)
  exercise real tab transfer and scripted provider restarts in isolated windows.

## Verification checkpoints

On 2026-10-15, the first integrated ordinary suite passed 1,749 tests, with 37
deliberately skipped tests. Strict workspace clippy and all-target compilation
passed. `make delegation-smoke` passed against the built executable without a
display, session binding or credentials. The registered QA unit suite passed
45 tests at that checkpoint.

The integrated `make check` passed 1,774 ordinary tests, 34 serial display tests and
45 QA tests. The final instrumented suite passed 1,777 ordinary tests and 34 display
tests, holding the unchanged 86% floor at **87.74%**. The final route-admission matrix
also passed all six focused tests after its reviewed gate update.
The debug binary opened successfully through the preview MCP with isolated XDG
homes, disabled provider profiles and updates disabled; the main window and
Settings rendered without a GTK failure. Screenshots are under ignored
`target/qa-artifacts/delegation-*-preview.png`. The preview was stopped afterwards.
This visual check uses the demo backend; scoped worker behaviour is covered by
the separate scripted native tests.

Coverage exposed a real concurrent
first-open snapshot race: the preflight could read version zero before another
creator committed, then see its tables afterwards and incorrectly refuse the
store. A deterministic regression failed before the consistent-read fix and passed
afterwards. All 25 preflight tests and the original concurrent-open test passed,
including an instrumented run; unknown databases continue to be refused. The final
source also passes 108 delegation tests, including 25 supervisor runtime cases,
and 190 session tests. Three receipt regressions cover exact message frames,
ambiguous echoes and cross-worker control replies. Strict workspace clippy and
the no-default-features build pass. The release executable passes the headless
MCP smoke without a display or credentials.

The PR and Release workflow dry run gate the exact pushed commit again. The latter
builds Debian, Fedora and Arch packages and installs them in fresh containers;
each installed binary must pass the headless MCP smoke. Workflow artifacts retain
package/source identity. A workflow-dispatch dry run publishes no release.

## Review and rollback

Native independent reviews found and addressed failures in environment fallback,
pending cancellation, protected launch metadata, interrupt escalation and setup
undo ownership. A fresh independent Claude review on 2026-10-15 returned PASS for
terminal commit-before-publication, durable subtree failure, generation fencing,
actual final AGY launch facts and the narrow opted-out root exception. The native
AGY re-review passed its three previously reported blockers: retained launch facts,
post-start revocation and single-use hello expiry. Native receipt review also
passed. These source reviews are distinct from the tests and provider fixtures.
Earlier rate-limited or timed-out attempts were not counted as passes.

The pinned rmcp 3.5.1 declares Rust 1.88, within the application's Rust 1.89
minimum. Tokio remains confined to the headless bridge runtime. No GTK floor or
installed service was added.

The store's normal backup transaction upgrades schema 3 or 4 to schema 5. Schema
5 retains explicit generation metadata on event links, including zero for unknown
historical rows. Rollback requires keeping the pre-migration backup rather than
opening the migrated database with an older binary. Disable profile delegation,
close supervised threads and remove managed agy registration to disable the
feature. Worktrees are never force-removed and child edits are never merged
automatically.
