# Window transfer and pending-start regression checks

Builder-MCP, 2026-10-15. Added to the existing isolated
`window::imp::tests::test_window_initialization` walkthrough, through its real
scripted Codex session. The fixture is a deterministic local Python app-server;
there is no provider request or quota consumption.

`PendingStart` contains the production continuation's weak slot, switch serial
and start-attempt number. Stop invalidates it, cancellation clears only the
matching attempt's `starting` flag, and a retry becomes current. An old completion
cannot clear a newer attempt, including an attempt with the same serial. The
existing production startup path calls these same methods before and after
supervisor preparation.

The transfer check invokes the actual Libadwaita `transfer_page` path to a second
window. It verifies the old process's observed exit and a new generation of the
same session, retained slot and view, new thread ownership and widget ancestry.
It submits `/new` through the original view's real composer: the destination
gets the new thread and the source does not. A scripted approval then proves the
pre-existing session sink updates the destination's running/approval state; Stop
clears both. Transferring back again starts another process generation and keeps
the original slot. Supervisor descendant revocation remains covered separately
by supervisor tests; this check does not claim to install a live supervisor.

The private Xvfb/D-Bus/XDG walkthrough passed: **1 test, 18.71 seconds**. Rustfmt
and `git diff --check` passed. A further same-serial assertion was added to the
test after that run; the coordinator's integrated gate must include it. These
checks do not change an installed app, profile registration or user's store.

P6 audit: the orphan-event queue is bounded to 64 events / 4 MiB; controls retain
their displayed exact scope and request mapping. Historical transcripts are
retained like the root transcript. The asynchronous supervisor replay/live
overlap was reported to its owner for ordering and duplicate-delivery fixes;
the view cannot deduplicate identical text deltas without an event sequence.
