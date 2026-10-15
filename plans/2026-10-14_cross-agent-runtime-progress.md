# Cross-agent runtime: build progress

Branch: `feat/cross-agent-runtime`. Hive topic: `agent-terminal-cross-agent-build` (#40).
The coordinator owns Cargo.lock, commits, merges, schema sequencing and release.

## Final implementation checkpoint — 2026-10-15

P1–P9 are integrated, including the SDK bridge, supervised sessions, scoped native
worker UI, messaging and controls, managed agy registration and application/window
lifecycle. Schema 5 uses the normal backed-up migration. Installed-provider proof,
fresh native reviews and independent Claude review admit Claude/agy bridge children
from all three parents. Codex children remain explicitly Unsupported because its
installed native-spawn disable controls fail the real trigger test. AGY's bridge-free
fallback is unimplemented, unadvertised and Unverified.

The full local gate passed 1,774 ordinary, 34 display and 45 QA tests; three final
receipt regressions subsequently pass within the 108-test delegation suite. Final
coverage passed 1,777 ordinary and 34 display tests at 87.74%, above the unchanged
86% floor. Strict clippy, six focused route tests and no-default-features checks pass
after those fixes. The release build and headless MCP smoke pass. Instrumented and
exact-commit CI/package evidence is recorded in `docs/qa/cross-agent-runtime.md` and
the delivery PR. Shipping requires
green PR checks and a Release workflow dry run, including all three fresh-container
package installs; no manual installation or live projection change is part of this
work. Companion source PR: https://github.com/jean1880/agent-config/pull/1.

The entries below retain earlier checkpoints and decisions; their then-outstanding
work is superseded by this final checkpoint and the verification report.

## User decisions

- agy is a parent in v1, through a managed global `mcp_config.json` entry plus allow rules, written from Settings. The
  coordinator adds the agent-config sync entry (P9).
- Depth and mode limits cover only agents started by agent-terminal. Children get native sub-agents and other MCP servers
  turned off; roots are unchanged.
- Claude `this_run` is best-effort. A message that lands in the next turn is recorded as the delivery state
  `misdelivered`.
- A private CODEX_HOME for Codex children only is allowed as the G-X1 fallback.

- 2026-10-15 (user): closing a root's page STOPS its children (cancel runs, wait for observed exits); no detach.
- 2026-10-15 (user): Always rules and session grants DO apply in children, but only for paths inside the child's own
  worktree lease and never beyond the parent's mode ceiling.
- 2026-10-15 (coordinator, P4 design conflicts):
  - Codex strict `this_run` gets a separate steer command with no new-turn fallback.
  - Claude child read-back happens at `system/init`, before any model output, and approval requests are held until then.
  - Claude best-effort delivery stays `unknown` until P5 adds `--replay-user-messages`.
  - Codex rebind restarts the process.
  - Moving a root between windows relaunches the root.

## Done (on the feature branch)

- P0: discovery (`docs/qa/delegation-discovery/`) and the frozen contract (`docs/delegation/contract-v1.md`,
  `p1-types.md`, `contract-v1-review.md`). Codex reviewed six rounds, ending in APPROVE; agy reviewed three rounds, ending
  in APPROVE.
- P2 sanitiser merged (Codex APPROVE, 4 rounds).
- P1 config: stable profile IDs merged (agy review fixes applied).
- rusqlite `backup` feature enabled.
- Shipping fix: PR #44 `fix/agy-workspace-plugin-hooks` (Codex APPROVE). Waiting for the user to merge.

## Wave 3 (done)

- P3a store merged: preflight/backup, v4 tables, durable_call. Codex data-safety review APPROVE after 4 rounds. Scope
  decision: another local user racing the user's own state dir is out of the threat model.
- P1 types merged: SipHash-2-4-128 attestation, tightened schemas. Codex security and protocol reviews passed after 3
  rounds. Full suite: 1567 passed.

## Wave 4 (running)

- P3b typed store API: Builder-P3b (opus, worktree) on `feat/cross-agent-p3b-store`.
- P4 design: Designer-P4 (opus) writing `docs/delegation/p4-supervisor.md`, with the lane split.

## Next

### Completion continuation — 2026-10-15

User authorizes full implementation, commits, PR creation and merges. Coordinator: Relay-Codex.
P4 C integrated as `b297fbdd`; P4 D and P3b were already integrated at `bf001e37`.
Recovered substantial uncommitted P4 B work; Builder-Session is completing it without discarding changes.
Builder-Supervisor owns P4 A; Builder-MCP owns P5. Worktrees are recorded by `git worktree list`.
Hive jobs 42–44 record the lanes; the coordinator retains claims. Main checkout owns application lifecycle,
profile opt-in, Settings, shared Cargo integration and shipping. Existing `.playwright-mcp/` is untouched.

Outstanding: finish and integrate A/B/P5; implement scoped worker UI and controls, mailbox continuation and
managed setup; prove each provider route before admitting it; full make check/coverage/package checks and
cross-harness review; ship feature PR on green CI. No live user registration or infrastructure was changed.
Rollback is disable delegation profile opt-ins and close supervised sessions; retain schema-4 history and
pre-migration backup. Never downgrade a migrated store in place.

Verified continuation checkpoints:
- Integrated continuation check: `cargo check --workspace --all-targets` and strict workspace
  clippy passed. First complete integrated `make test`: 1749 passed, 37 skipped. Final gate is
  still pending subsequent security fixes; these are checkpoints, not release approval.
- Installed agy cloud proof passed A1/A3/A5 using only a read-only native-auth bind in a
  private home. Claude/Codex CLI enforcement proofs use deterministic local model fixtures.
- P9 hardened journal ownership, corrupt-journal rejection and checked selected environments:
  11 tests and actual production shim handshake passed. Window transfer/start cancellation
  smoke passed in a private display, retaining the view and routing actions to its new owner.
- Fresh native review found and fixed resolver fallback, pending-cancel dispatch, protected
  metadata persistence and interrupt escalation. Additional committed-feed/replay fixes are
  in progress. Schema 5 explicitly records event generation, with tested normal migration
  from schema 4. Never publish child events before their writer commit receipt.
- `eab737d8`: recovered P4 B committed with secret scan; `d24bbf00` retains upstream PR #44;
  `88a6e0fc` integrates both into the feature branch. No user work was discarded.
- B semantic integration found and fixed child fresh-hook fallback, post-await/stale approval grants,
  and reopened generation reuse. Latest session filter: 186 passed; final integration gate still pending.
- Bridge: 11 SDK/IPC/cancellation/bounds regressions passed and clippy clean in a GTK-free harness.
- Managed registration: four data-safety tests passed; identity persistence regression passed.
- Installed-provider local-model proofs: Claude restricted Bash denial and no capability persistence pass;
  Codex MCP isolation, token persistence and fresh overlay on resume pass. Codex native delegation **fails**:
  a valid native spawn starts a worker despite process/config feature-disable controls. Codex child admission
  must report unsupported until an enforceable mechanism exists; this is not a model self-report.
- Claude cross-harness reviewer is rate-limited; agy review timed out twice. No review verdict inferred.
- Companion source PR: https://github.com/jean1880/agent-config/pull/1, commit `880cbf6`, CI and secret check green.
  Isolated-home agent-sync apply/check: zero drift, 131 items. Live user projections and existing dirty
  agent-config checkout untouched. Merge companion after application support is verified.
- Coordinator approved additive seam fixes: generation_base on SessionOptions, replayed() on WorkerSubscriber
  (history cannot trigger current diff baselines), and trusted worker_scope via ChatBackend.

- Review P1 (Codex) and P3a (Codex with a red-team lens, since it is data-loss sensitive); merge.
- P3b: typed store API over P1 types, atomic event+link publication, mailbox ACK transactions, replay epoch.
- P4 supervisor and authority, then P5/P6/P7 in parallel, then P8 matrix, P9 setup, P10 release.
