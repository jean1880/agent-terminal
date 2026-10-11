# Cross-agent feature parity audit (Claude · agy · Codex)

Status: DONE 2026-10-10 (report: `docs/feature-parity-report.md`; 5 High / 6 Medium / 12 Low) · Target: HEAD `8502282` (3.3.0~beta) · Output: `docs/feature-parity-report.md`

## Question
Does a thread give the same experience whatever agent drives it? Specifically:
1. **Thoughts**: is reasoning captured, streamed, rendered, persisted and replayed for each agent?
2. **Actions**: does every tool or action kind (read, edit, write, shell, search, web, MCP, subagent,
   file-change card, diff, checkpoint/undo) render and behave the same way?
3. **Approvals**: do Ask / Accept edits / Plan / bypass, live approve/deny, always-allow patterns,
   session-scoped auto-approvals, permission export and the sync-discrepancy indicator work everywhere they claim to?
4. **Shared experience**: queueing, interrupt, model switch, compact, questions, `@` file suggestions,
   usage/context indicators, handoff/switch, fork, worktrees, environment review, store replay, redaction.

## Known premises (verified 2026-10-10)
- CLIs: claude 2.1.293, agy 1.3.3, codex-cli 0.161.0, all on PATH. agy approval hook is present in `~/.gemini/config/hooks.json`.
- `crates/agent-core/src/caps.rs` is the claimed matrix. Its Codex column says "not verified by recording, Codex not installed", but Codex is now installed, so that column is unverified.
- Discrepancy to settle: caps says agy `streams_reasoning: false`, but `c56486ea` "surface AGY thoughts".
- `~/.local/bin/agent-terminal` → `target/release` (stale, 11:37). It shadows the apt package. Out of scope; flagged to the user.

## Evidence tiers (each matrix cell cites its strongest tier)
| Tier | Source | Proves |
|---|---|---|
| T0 Claimed | README, `docs/`, `docs/releases/*.md`, `caps.rs`, adapter code (`claude.rs`/`agy.rs`/`codex.rs`, `approval.rs`, `event.rs`) | what is supposed to work |
| T1 Tested | `cargo test --workspace`, `python3 tests/qa/gate.py --native-only` (scripted_tests.rs, ux_tests.rs, `scripted_codex.py`) | synthetic behaviour holds |
| T2 Live | real agent, real turn, driven in the GUI via `preview_app` + Playwright | it works end to end today |

A cell is **PASS** (T2 shown), **PASS-SYNTH** (only T1), **CLAIMED** (only T0), **GAP** (capability absent and greyed out correctly), **FAIL** (claimed but broken, or not greyed out), or **DRIFT** (code, docs and caps disagree).

## Method
### Phase 1: Inventory + automated proof (1 × sonnet)
- Build the feature list from README, `docs/`, all `docs/releases/*.md`, `plans/2026-10-06_*`, and the commits since 3.0.
- Map each feature to adapter code paths per agent, and to the caps field (if any) that gates the UI.
- Run `cargo build` (debug only; never touch `target/release`), `cargo test --workspace`, and `gate.py --native-only`.
  Record the test names that cover each feature × agent.
- Output: `target/parity/inventory.md`, a feature × agent matrix at T0/T1, plus the scenario script for Phase 2.

### Phase 2: Live runs (3 × sonnet in parallel, one per agent)
- Sandbox: `mktemp -d` git repo per agent, seeded with a small identical fixture (a few source files, one test, one fake `secrets.env`).
  No real project is ever opened.
- Launch: `preview_app` on `target/debug/agent-terminal`, with `config_files` seeding a config that points at the sandbox.
  Drive it with Playwright, taking a screenshot per step. Never hand-run broadwayd or dbus, and never kill the user's running instance.
- Identical scenario per agent (prompts kept short to bound token cost):
  1. Reasoning prompt → thinking block shown, streamed, collapsible, persisted (close and reopen the thread → replayed).
  2. Read + search → tool cards titled consistently.
  3. Edit + new file → file-change card, diff panel, diff vs turn baseline, checkpoint and undo.
  4. Shell command in **Ask** → approval card → **deny** → agent sees the denial. Repeat → **approve** → output card.
  5. Always-allow pattern (session scope) → second matching command auto-approves; a non-matching one still asks.
  6. **Plan** mode → attempt an edit → blocked (record agy's documented hookless caveat separately).
  7. Accept-edits mode → edit applies without a prompt, shell still asks.
  8. Queue two prompts mid-turn → pinned at the bottom, run in order. Interrupt → process kept or restarted as caps claims.
  9. Model switch, `/compact`, `@` file suggestion, structured question, usage/context indicator. Expect a GAP where caps says false; check it is greyed out.
  10. Secret redaction: the agent reads `secrets.env` → the store row for that event is scrubbed (query the thread's SQLite store in the preview XDG data dir).
- Cross-agent (run by the Claude runner after 1–10): switch the thread Claude → agy → Codex, check the handoff brief is carried and redacted, then fork, then the environment review.
- Proof per step: screenshot path, store event excerpt (redacted), `preview_app_log` excerpt.
- Output: `target/parity/live-<agent>.md`.

### Phase 3: Report (1 × sonnet, same agent as Phase 1 via SendMessage)
- Merge into `docs/feature-parity-report.md`: an executive summary, the matrix with tier and verdict per cell, and evidence links.
  Also a DRIFT list (caps vs code vs docs vs live), ranked FAIL/DRIFT findings with repro steps, and test-coverage gaps (cells at T0 only).
- Orchestrator (me) spot-checks every FAIL/DRIFT and 3 random PASS cells against the raw evidence before reporting.

## Guardrails
- Report only: no source edits, no commits, no `make install`, and no release build.
- Sandboxes are under `/tmp` and deleted at the end. Screenshots stay in `target/parity/`, which is gitignored.
- Secrets: never print token or env files. The redaction step uses a fake canary value only.
- Cost bound: about 12 short turns per agent and one switch chain.
- Rollback: delete `target/parity/`, `/tmp/parity-*` and the report file. Nothing else changes.
