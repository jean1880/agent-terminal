# Cross-agent runtime: build progress

Branch: `feat/cross-agent-runtime`. Hive topic: `agent-terminal-cross-agent-build` (#40).
The coordinator owns Cargo.lock, commits, merges, schema sequencing and release.

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

- Review P1 (Codex) and P3a (Codex with a red-team lens, since it is data-loss sensitive); merge.
- P3b: typed store API over P1 types, atomic event+link publication, mailbox ACK transactions, replay epoch.
- P4 supervisor and authority, then P5/P6/P7 in parallel, then P8 matrix, P9 setup, P10 release.
