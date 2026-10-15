# Cross-agent runtime: build progress

Branch: `feat/cross-agent-runtime`. Hive topic: `agent-terminal-cross-agent-build` (#40).
The coordinator owns Cargo.lock, commits, merges, schema sequencing and release.

## Wave 1 (started 2026-10-14)

| Lane | Worker (tier) | Output | Status |
| --- | --- | --- | --- |
| P0 D1–D7 Claude | Scout-Claude (sonnet) | `docs/qa/delegation-discovery/claude.md`, `tests/fixtures/real/delegation/claude/` | done (Claude 2.1.300): D1/D2/D3/D5/D7 PROVEN, D4/D6 PARTIAL. Contract deltas: child authority is enforceable only with `--restricted` (user allow rules otherwise bypass Ask; it drops Bash unless `--tools` names it); effort never reported (`launch_configured` only); confirmed model = `assistant.message.model`; plan mode switches model; `_meta["claudecode/toolUseId"]` gives exact call linking; Claude respawns a dead bridge, so the bridge is stateless; `--replay-user-messages` echo = delivery receipt. Side effect: real runs used the real login; a plan file and a transcript dir were created and then deleted, and `~/.claude.json` may hold a trust entry for the throwaway cwd |
| P0 D1–D7 Codex | Scout-Codex (sonnet) | `docs/qa/delegation-discovery/codex.md`, `tests/fixtures/real/delegation/codex/` | done: D1–D3 PROVEN, D4–D7 PARTIAL. Contract deltas: resend the per-thread `config` overlay on every start/resume (it is not persisted, and a resume falls back to the user's same-name server); MCP needs `default_tools_approval_mode=approve`, so the bridge enforces all authority; MCP env is only HOME/LANG/LOGNAME/PATH/SHELL/USER plus `env`/`env_vars`; `turn/interrupt` sends no MCP cancel; effort/model reported = request echo only |
| P0 D1–D7 agy | Scout-Agy (opus) | `docs/qa/delegation-discovery/agy.md`, `tests/fixtures/real/delegation/agy/` | running |
| P0 D8 SDK and SQLite backup/lock proof | Scout-Infra (opus) | `docs/qa/delegation-discovery/sdk-and-store.md` | done: rmcp =3.5.1 (server, transport-io; tokio current_thread rt/time/io-std); backup/lock PROVEN; 3 amendments to §5 (single-instance is the real guard; preflight must not write; bounded `step(-1)`, never `run_to_completion`) |
| P2 streaming sanitiser | Builder-Sanitiser (opus, worktree) | branch `feat/cross-agent-p2-sanitiser` | running |

## Next

- Freeze the contract (P0 close-out) from the discovery verdicts; review it with a cross-harness checker.
- Wave 2: P1 types/IDs, then P3 store (needs the P0 store proof).
