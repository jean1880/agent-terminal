# agy delegation discovery fixtures (P0, D1–D7)

Recorded 2026-10-14 against `agy` 1.3.5 by the discovery scripts in the git-ignored
`target/delegation/agy/bin/` folder. Every run used `env -i` with a throwaway `HOME`
(`$TMP_HOME`), a throwaway git workspace (`$WORKSPACE`) and `--mode plan`. None used
`--dangerously-skip-permissions`. The real `~/.gemini` was never read or written: the strace
captures show no opens under it. Credentials came from the desktop keyring over the session
D-Bus, not from `HOME`.

The fake stdio MCP server (`fake_mcp.py <label> <logdir>`) logs every frame, its cwd, and the
**names** of its environment variables (never their values). It exposes one tool,
`delegation_probe`.

How the captures were scrubbed: absolute paths are replaced with `$TMP_HOME`, `$WORKSPACE`,
`$WORKTREE`, `$MAIN_REPO`, `$ALT_GEMINI_DIR`, `$LOGDIR`, `$BIN` and `$REAL_HOME`. UUIDs are
replaced with stable fakes (`00000000-0000-4000-8000-00000000000N`), consistently across files.
pid and ppid were dropped. The captures held no credentials.

Server labels map to the source each server was declared in:

| label | declared in | MCP name |
| --- | --- | --- |
| `global` | `$TMP_HOME/.gemini/config/mcp_config.json` | `gfake` |
| `global-dup` | same file | `dup` |
| `ws-dup` | `$WORKSPACE/.agents/mcp_config.json` | `dup` (never started: global wins) |
| `wsagents` | `$WORKSPACE/.agents/mcp_config.json` | `wfake` |
| `wsplugin` | `$WORKSPACE/.agents/plugins/wplug/mcp_config.json` | `wplug_wpfake` |
| `homeplugin` | `$TMP_HOME/.gemini/config/plugins/hplug/mcp_config.json` | `hplug_hpfake` |
| `agycli`, `agycliplugin`, `geminiplugin` | `.gemini/antigravity-cli/mcp_config.json`, `.gemini/antigravity-cli/plugins/aplug/`, `.gemini/plugins/gplug/` | never started |
| `wsdotmcp`, `wsgemini` | `$WORKSPACE/.mcp.json`, `$WORKSPACE/.gemini/config/mcp_config.json` | never read |

| File | Gate | What it shows |
| --- | --- | --- |
| `d2-mcplist.txt` | D2 | `agy mcp list` lists only the global file |
| `d2-config-probes.txt` | D2 | which config files a turn opens (strace: path and result) |
| `d2-gemini-dir-probes.txt`, `d2-gdirabs.txt` | D2 | the undocumented `--gemini_dir=<dir>` flag moves MCP, hooks and settings to `<dir>` |
| `d2-all-sources-denied.stream.ndjson` | D2/D6 | every source's tool is visible. Headless mode denies each `mcp` call without an allow rule |
| `d1-d2-server-starts.jsonl` | D1/D2 | per run: which servers started, their cwd, and the env-var **names** they inherited (`DELEG_CANARY_PARENT`, `AGENT_TERMINAL_DELEGATION_ENDPOINT` = launch env) |
| `d1-d3-mcp-exchange.global.mcp-server.jsonl` | D1/D3 | `server/discover` (-32601), `initialize` 2025-11-25, `tools/list`, then two `tools/call` with `_meta.antigravity.google/conversation_id` and `progressToken "<uuid>:<step_index>"` |
| `d3-d5-two-calls-then-queued-turn.stream.ndjson` | D3/D5 | the matching stream: `call_mcp_tool` steps 2 and 3, then the result. The pre-written second message runs as a separate turn (`num_turns` 2) |
| `d5-d6-allow-rule-denial-merges-turns.stream.ndjson` | D5/D6 | the allow rule `mcp(gfake/delegation_probe)` runs gfake and denies wfake. The turn that ended on the denial emitted **no** result before the queued message ran: one result for two prompts |
| `d4-d7-resume-base-model-effort.stream.ndjson` | D4/D7 | `--conversation --model gemini-3.8-flash --effort low`: init echoes `model` and not effort. Every server restarts with a fresh `initialize` |
| `d4-models.tsv` | D4 | `agy models` (the effort is part of each id) |
| `d6-worktree-probes.txt` | D6 | in a linked worktree, agy probes `<worktree>/.agents` and `<worktree>/sub/.agents`, never the main tree's `.agents` |
| `d6-workspace-plugin-hook.stream.ndjson` | D6 | `$WORKSPACE/.agents/plugins/wplug/hooks.json` (PreToolUse `.*`) **ran** on a `call_mcp_tool` (marker file created) |
| `d7-interrupt.stream.ndjson`, `d7-interrupt.global.mcp-server.jsonl` | D7 | SIGINT mid-turn gives `result.status ERROR "interrupted"`. The queued second message was dropped and no server outlived agy |

Caveats: the strace extracts miss `<unfinished ...>` lines, so the probe lists are lower
bounds. The SIGINT run went through `timeout`, which signals `strace`, so how the servers were
terminated is unconfirmed (no orphans remained).
