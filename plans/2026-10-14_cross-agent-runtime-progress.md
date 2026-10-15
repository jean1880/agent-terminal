# Cross-agent runtime: build progress

Branch: `feat/cross-agent-runtime`. Hive topic: `agent-terminal-cross-agent-build` (#40).
The coordinator owns Cargo.lock, commits, merges, schema sequencing and release.

## Decisions

- 2026-10-15 (user): agy as a parent is in v1, through a managed entry in the global
  `~/.gemini/config/mcp_config.json` written from Settings. The coordinator also adds the entry to
  `agent-config/sync/mcp.toml` so agent-sync does not revert it.
- 2026-10-15 (coordinator, from the contract): spawning needs no operator card inside the mode ceiling (the child's edits
  still ask). The 8 agy allow rules come with the managed entry.

## Done

- P0 discovery: `docs/qa/delegation-discovery/` and fixtures (committed).
- Shipping fix PR #44 `fix/agy-workspace-plugin-hooks`: workspace plugin hooks, config-dir flags, and a fresh proof on
  every flagged start. Codex APPROVE after 3 rounds.

## In progress

- P0 close-out: `docs/delegation/contract-v1.md` and `p1-types.md` (written; the architect seat is read-only). Under
  review by Codex (architect and red-team lenses) and by agy (red-team, agy facts).
- P2 sanitiser `feat/cross-agent-p2-sanitiser` (commits dc3c7135, e36d9071, 75133244). Round 4: the generic `K::hash`
  could alias keys, so it moves to a bounded concrete StreamKey.

## Next

- After contract APPROVE: commit; P1 builder (agent-core delegation module, worktree); merge P2 into the feature branch.
- Then P3 store (agent-kit), in parallel with P1 where it needs only the frozen table shapes.
