# Claude Code delegation discovery fixtures (D1-D7)

Recorded 2026-10-14 with `claude --version` = **2.1.300 (Claude Code)**, model alias `haiku` (resolved `claude-haiku-5-5`).
Scrubbed: account block, command/skill lists, hook output, thinking signatures, home path and user name.
Frames are `{dir: "->"|"<-"|"##", t: seconds, line: <stream-json frame>}`; `->` is what the driver wrote, `<-` what claude printed.
`mcp-server.ndjson` is the fake MCP server's log (every frame, and environment variable NAMES only, never values).

## Harness (`harness/`)

- `fake_mcp.py` is the stdio MCP server (tools `echo`, `slow`, `crash`, `whoami`). `python3 -I fake_mcp.py <log> <label>`.
- `drive.py <scenario> <outdir>` spawns claude with the app's own flags and writes `frames.ndjson`, `mcp-server.ndjson`, `argv.txt`.
- `scrub.py` produced these fixtures from the raw runs (kept under the ignored `target/delegation/claude/out/`).
- Raw working copies live in `target/delegation/` (git-ignored). Real runs use the real login (an isolated
  `CLAUDE_CONFIG_DIR` has no auth: `Not logged in`, though it did prove the overlay and MCP handshake for free).

## Common argv (the app's `ClaudeAdapter::argv` plus the overlay)

```
claude --output-format stream-json --verbose --input-format stream-json --permission-prompt-tool stdio \
  --include-partial-messages --mcp-config <overlay.json> --model haiku --permission-mode <mode> \
  [--strict-mcp-config] [--no-session-persistence]
```

Overlay file: `{"mcpServers": {"fake": {"type":"stdio","command":"python3","args":["-I","fake_mcp.py","<log>","A"],"env":{"OVERLAY_VAR":"from-overlay"}}}}`.
The claude process was started with `FAKE_BEARER=fake-bearer-123` (a fake) in its environment.

## Scenarios

| Dir | Gate | Command (driver scenario) / what it shows |
| --- | --- | --- |
| `d1_basic` | D1, D3 | `--strict-mcp-config`, bypass; initialize / tools/list / tools/call with `_meta` |
| `d1_precedence` | D1 | overlay server named `plans` (same as a user-scope server), no strict flag |
| `d4_effort_low`, `d4_effort_max` | D4 | `--effort low` / `--effort max`: nothing in the stream reports effort |
| `d4_badmodel`, `d4_badeffort` | D4 | `--model claude-nonexistent-9` fails at first request; `--effort bogus` silently accepted |
| `d5_busy_replay` | D5 | `--replay-user-messages`; mid-turn user line, then idle follow-up in the same process |
| `d5_idle_a`, `d5_idle_b_resume` | D5 | `--session-id <uuid>` then a fresh process with `--resume=<uuid>` |
| `d6_plan` | D6 | `--permission-mode plan`: Write/MCP blocked, ExitPlanMode arrives as `can_use_tool` |
| `d6_ask_user_rules` | D6 | default mode + `--setting-sources local`: Write outside cwd still allowed by a `user_permanent` rule |
| `d6_default_nouser_mcp_ask` | D6 | default mode: MCP call asks via `can_use_tool` with `mcp_server` |
| `d6_restricted` | D6 | `--restricted`: Write inside cwd asks, outside cwd hard-denied, MCP asks |
| `d7_crash` | D7 | tool that exits the server: in-flight `Connection closed`, server respawned in the same turn |
| `d7_killmcp` | D7 | server killed externally mid-call: same, then next call works on a new pid |
| `d7_interrupt` | D7 | `control_request interrupt` during a 30 s MCP call |

Not recorded: persistent registration was never run (it would write the real `~/.claude.json`); see the doc.
Side effects on the real home from claude itself: one plan-mode file in `~/.claude/plans/`, one project transcript dir
for the resume test, both removed afterwards; `~/.claude.json` may carry a trust/project entry for the throwaway cwd.
