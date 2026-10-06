# Recorded agent streams

Real NDJSON recorded on 2026-10-06 against `claude` 2.1.291 and `agy` 1.3.0, in a sandbox with an
isolated `HOME`, by the Phase 0 probe (see `plans/2026-10-06_v3-structured-agents.md`).

Each line is `{"dir":"in"|"out"|"err","t_ms":N,"frame":<json>|{"raw":"…"}}`:
- `in` is what the client wrote to the agent's stdin;
- `out` is a stdout line;
- `err` is a stderr line;
- `{"signal":"INT"}` marks a SIGINT the probe sent.

They were scrubbed before being committed. Account email, home directory and sandbox paths are
replaced by `user@example.com`, `/home/user`, `/work/repo` and `/work/scratch`. They contain no
credentials, and gitleaks finds nothing in them.

| File | Scenario |
|---|---|
| `claude-turn-approval.ndjson` | initialize; one edit turn with a `can_use_tool` approval (allowed) |
| `claude-controls-slash.ndjson` | mcp_status, get_settings, list_models, file_suggestions, get_context_usage; `/cost` (local command); `/compact`; `set_model sonnet` + a turn |
| `claude-interrupt.ndjson` | an `interrupt` control request mid-stream |
| `agy-edit.ndjson` | one edit turn in default mode (view_file, replace_file_content) |
| `agy-hook-approval.ndjson` | `--dangerously-skip-permissions` + a PreToolUse hook: one command allowed, one denied |
| `agy-hook-payloads.ndjson` | what the hook received on stdin (`payload`) and whether the env var reached it |
| `agy-interrupt.ndjson` | SIGINT mid-response, then the process exits |
| `agy-resume.ndjson` | `--conversation <id> --model gemini-3.1-pro-low` resume |
| `agy-local-command-error.ndjson` | `/model` inside a stream-json session: an error, then the process exits |
| `codex-synthetic-turn.ndjson` | **SYNTHETIC, not recorded** (codex was not logged in): built from the codex 0.160.1 app-server protocol (`codex app-server generate-json-schema` and openai/codex `codex-rs/app-server-protocol/src/protocol/`: `common.rs` registry, `v1.rs` InitializeParams/Response, `v2/thread.rs` ThreadStart/Resume/TokenUsage, `v2/turn.rs` TurnStart/Turn notifications, `v2/item.rs` ThreadItem + approval params, `v2/thread_data.rs` Thread/Turn). Thread objects carry only the verified core fields. Covers initialize, thread/start, turn/start, reasoning summary deltas, agentMessage deltas, a commandExecution requestApproval answered accept, tokenUsage, turn/completed. Replace with a real recording. |
