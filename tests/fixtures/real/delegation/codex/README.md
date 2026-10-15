# Codex delegation discovery fixtures (gates D1-D7)

Recorded 2026-10-14 against `codex-cli 0.161.0` (`codex app-server`, stdio NDJSON), models `gpt-5.6-luna` (cheap)
and the account default. Each file is NDJSON `{"t", "dir", "frame"}`; `dir` is `in` (app-server to client) or `out`
(client to app-server) in the `*.app-server.jsonl` files, and `in`/`out`/`meta` as seen by the fake MCP server in
`*.mcp-server.jsonl` (`meta` rows list env var NAMES and a short sha256 of `DELEG_PROBE_*` marker values only).

Scrubbed: home path -> `/home/user`, emails, token-shaped strings, git commit hash, remote owner. Dropped:
`account/*`, `thread/tokenUsage/*`, `remoteControl/*` notifications; `mcpServer/startupStatus/updated` kept only where
the gate is about startup. The `mcpServerStatus/list` result is trimmed to names.

## How they were made

Isolated home: `CODEX_HOME=<repo>/target/delegation/codex/home-<n>` containing only a symlink `auth.json` -> the real
auth file (never read, printed or copied) and, for D1 precedence, a temp `config.toml`. The real `~/.codex` was never
written (mtime of `auth.json`/`config.toml` verified unchanged). Fake MCP server:
`target/delegation/fake-mcp-codex/server.py <logfile>` (git-ignored; tools `echo`, `slow`, `die`).
Drivers: `target/delegation/codex/s1..s9*.py` (git-ignored), run as `python3 -I <script>`.

| File prefix | Driver | Exact launch / request |
| --- | --- | --- |
| `d1-launch-override` | s1 | `codex -c 'mcp_servers.fake.command="python3"' -c 'mcp_servers.fake.args=["<server.py>","<log>"]' -c 'mcp_servers.fake.env={DELEG_PROBE_EXPLICIT="explicit-marker"}' -c 'mcp_servers.fake.env_vars=["DELEG_PROBE_TOKEN"]' app-server`; ambient `DELEG_PROBE_PARENT` set in the app-server env |
| `d1-thread-config-binding` | s2 | `codex app-server` (no `-c`); `thread/start {cwd, approvalPolicy:"never", sandbox:"read-only", model:"gpt-5.6-luna", config:{"mcp_servers.fake.command":..., "mcp_servers.fake.args":[...], "mcp_servers.fake.env":{...}}}` twice with different env markers |
| `d1-name-precedence` | s4 | home `config.toml` defines `fake` and `other`; launch `-c` and `thread/start config` override `fake` |
| `d1-resume-*` | s5 | start with config, restart app-server, `thread/resume` with and without `config` |
| `d3-d6-mcp-approval` | s3 | `default_tools_approval_mode` `approve`/`prompt`/unset x `approvalPolicy` `never`/`on-request`/`untrusted`; `mcpServer/elicitation/request` answered accept/decline |
| `d4-model-effort` | s9 | `thread/start model`, `turn/start effort`/`model`, bad values |
| `d5-d7-steer-interrupt-death` | s6 | `turn/steer` during a `slow` tool call, stale ids, idle follow-up turn, `turn/interrupt`, `die` tool |
| `d5-steer-turn-end-race` | s7 | `turn/steer` fired on `item/completed` of the final `agentMessage`, 5 trials |
| `d6-sandbox-policy` | s8 | shell write under `readOnly` / `workspaceWrite`, forced to attempt (last run overwrote earlier s8 frames) |

Model-dependent outputs (what the model said) are not contractual; the protocol frames are.
