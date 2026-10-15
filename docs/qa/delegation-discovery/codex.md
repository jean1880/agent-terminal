# Delegation discovery: Codex (`codex app-server`), parent and child

Version: `codex-cli 0.161.0`. Recorded 2026-10-14. Frames: `tests/fixtures/real/delegation/codex/` (README lists exact
commands). Isolated `CODEX_HOME` with a symlinked `auth.json` only; real `~/.codex` untouched.

Verdicts: D1 PROVEN, D2 PROVEN, D3 PROVEN, D4 PARTIAL, D5 PARTIAL, D6 PARTIAL, D7 PARTIAL.

## Contract-changing surprises (read first)

1. **Bind per thread, not per process.** `thread/start`/`thread/resume` accept a `config` map; `mcp_servers.<name>.*`
   keys in it spawn a **separate MCP child process per thread** with that thread's own env. This is the parent-binding
   channel: no `-c`, no config file, no shared app-server state.
2. **The binding is not persisted.** `thread/resume` without `config` silently falls back to the user's
   `config.toml` server of the same name (or none). Every start AND resume must resend the overlay.
3. **MCP calls need approval even under `approvalPolicy: never`**: the call fails with `MCP tool call requires approval,
   but approval policy is never`. Registration must set `default_tools_approval_mode = "approve"` (or the bridge is
   unusable in `never` children). Consequence: Codex gives no native veto over bridge calls; all authority is the
   bridge's own.
4. **MCP processes get a minimal environment.** Only `HOME LANG LOGNAME PATH SHELL USER` plus the config `env` table and
   `env_vars` names. Ambient app-server variables and all `CODEX_*` are NOT forwarded.
5. **`thread/settings/updated` "model/effort" is an echo of the request, not confirmation.** `no-such-model` and
   effort `bogus` are both echoed; rejection arrives only as `turn/completed` with `status: failed` (not at
   `thread/start`/`turn/start`).
6. **`turn/interrupt` does not cancel the in-flight MCP call**: no `notifications/cancelled` reaches the server, and the
   `mcpToolCall` item never gets `item/completed` (dangling item). Bridge must clean up its own wait.
7. **A dead MCP server stays dead for that thread**: `Transport closed` on every later call; no restart, no
   `startupStatus` change. A fresh `thread/resume` respawns it.

## D1 parent binding: PROVEN

Evidence: `d1-launch-override.*`, `d1-thread-config-binding.*`, `d1-name-precedence`, `d1-resume-*`.
- Frames the server sees: `initialize` (`protocolVersion 2025-06-18`, `clientInfo {name: "codex-mcp-client", title:
  "Codex", version: "0.161.0"}`, capabilities `elicitation{form,url}`), `notifications/initialized`, `tools/list` (with
  `_meta.progressToken`), then `resources/list` and `resources/templates/list` (answer or error; error `-32601` is
  tolerated). Server cwd = thread cwd (a status-list probe spawns one with the app-server cwd).
- Env: see surprise 4. `env={K="v"}` reaches the server; `env_vars=["NAME"]` forwards that name from the app-server's own
  env (proven for `DELEG_PROBE_TOKEN`); ambient `DELEG_PROBE_PARENT` did not arrive.
- Two threads in one app-server with different `config` env: two server pids with different env hashes (`bd4a5b49`,
  `080f752f`). Each thread's MCP process serves only that thread, so a per-worker token in `env` is an unforgeable
  parent identity (never from task text).
- Name precedence for `mcp_servers.fake`: thread `config` > launch `-c` > user `config.toml`; replacement is wholesale
  per server name (the user's `fake` env is gone). Unrelated user servers (`other`) still start in the child.
  UNVERIFIED: `mcp_servers.<other>.enabled=false` to suppress them.
- Restart: see surprise 2 (`d1-resume-b/c`: with config -> `resume-override`; without -> `user-config`).
- Consequence: bridge registration name collision with a user's `cross_agent` entry is silently overridden for app-launched
  threads (plan item 10 should still show it as a visible conflict). Bridge must re-assert identity on every tool call
  from the token, and Test must run through the same `thread/start config`.

## D2 registration: PROVEN (launch override and per-thread config); persistent shape documented, not applied

- Launch override works: `codex -c 'mcp_servers.NAME.command="…"' -c 'mcp_servers.NAME.args=[…]' app-server`
  (TOML values; raw string fallback). Per-thread `config` with dotted keys works (preferred, see D1).
- Recommended config keys: `command`, `args`, `env`, `env_vars`, `default_tools_approval_mode="approve"`,
  `tool_timeout_sec`. Also present in the binary: `enabled_tools`, `disabled_tools`, per-tool `tools.<t>.approval_mode`
  (`prompt|writes|approve` strings seen; `approve` and `prompt` proven, `writes` UNVERIFIED).
- Persistent shape (only if a user wants it outside the app; do not apply to the real home):

  ```toml
  [mcp_servers.cross_agent]
  command = "/usr/bin/agent-terminal"
  args = ["--cross-agent-mcp"]
  default_tools_approval_mode = "approve"
  ```

  Without a per-thread env token this entry cannot bind a parent; the app must treat it as `route_unverified`.
- Isolation recipe that worked: temp `CODEX_HOME` + symlink to `auth.json` (no copy, no read). `codex_home.rs` only
  resolves the home; it does not build one.

## D3 invocation correlation: PROVEN

`tools/call` reaches the server with `params._meta`:
`callId` = the app-server `mcpToolCall` item id (`exec-<uuid>`, same value as `item/started.params.item.id`),
`threadId`, `sessionId`, `windowId`, `itemId` (the model's provider call id, `ctc_…`, NOT the app-server id),
`progressToken` (small per-connection integer), and `x-codex-turn-metadata` {`thread_id`, `turn_id`, `model`,
`reasoning_effort`, `sandbox_mode`, `codex_version`, `workspaces` (git remote URL and HEAD hash: scrub/never store)}.
Consequence: link the bridge request to the child card by `(threadId, callId)`. The JSON-RPC `id` is per-connection and
useless. `_meta.callId` is advisory only; authority still comes from the token.

## D4 model/effort: PARTIAL

- Set: `thread/start {model}`, `turn/start {model?, effort?}` (per turn, sticky in `thread/settings/updated`), or
  `thread/start config {model_reasoning_effort}` (then `thread/start` result reports `reasoningEffort`).
- Reported: `thread/start` result `model` (+ `reasoningEffort` only when set via config); `thread/settings/updated`
  `{model, effort, collaborationMode.settings}` at turn start; `tools/call _meta` `model`/`reasoning_effort`.
  All of these echo the request: provenance is `launch_configured`, never `provider_reported`-confirmed.
- Failures: unknown model -> `turn/completed` `failed` (ChatGPT-account 400) plus a `warning` notification; unsupported
  effort string (`bogus`) -> `failed` listing `none|minimal|low|medium|high|xhigh|max`.
- `model/list` effort lists are advisory: `ultra` is not in the server's own list but luna accepted it and completed
  (UNVERIFIED whether it has any effect). Defaults with no model: `gpt-6.1-sol`, effort unreported.
- Consequence: record `requested` + `launch_configured`; flip to `provider_reported` only after a successful
  `turn/completed`. Validate effort against `model/list` as a warning, not a hard refusal; treat `failed` turn as
  `unsupported_effort`/`provider_unavailable`.

## D5 delivery: PARTIAL

- `turn/steer {threadId, expectedTurnId, input:[{type:"text",text}]}` during a running turn (slow tool in flight):
  result `{turnId}` (same id); the text lands as a `userMessage` item in the SAME turn and the model used it
  (`DONE STEERED`). Receipt = response; consumption evidence = that `userMessage` `item/completed`.
- Wrong/stale `expectedTurnId` or no active turn: JSON-RPC error `-32600 "no active turn to steer"`, no side effect, no
  new turn started. (The adapter's "refusal starts a turn" is app policy, not Codex's.)
- Turn-end race: 5/5 trials with the steer sent right after the final `agentMessage` completed were ACCEPTED and folded
  into the same turn (turn extended; a second agent message followed). The refused window was not hit in 5 trials, so
  both outcomes must be handled; the mailbox stays authoritative. UNVERIFIED: a steer losing outright (accepted but
  dropped): none seen.
- Idle follow-up: `turn/start` on the same thread keeps context (asked for earlier word, answered `STEERED`) and the same
  MCP process.

## D6 safety: PARTIAL

- `sandbox` is a thread/start string (`read-only`, `workspace-write`) and is reported back as `sandbox` object
  (`readOnly{networkAccess:false}`, `workspaceWrite{writableRoots:[],excludeSlashTmp:false}`) next to `approvalPolicy`.
  Kernel enforcement PROVEN with forced attempts: read-only gives `read-only file system`; workspace-write refuses a
  write in the parent of cwd. `/tmp` is writable in workspace-write (`excludeSlashTmp:false`): not a violation, but the
  child can write there.
- Approval round trip PROVEN: read-only + `on-request` raises `item/commandExecution/requestApproval`; decline -> no
  file, item `declined`; accept -> file written. Under `never` nothing is asked and the command is simply blocked by the
  sandbox. Existing fixtures cover `item/fileChange/requestApproval`.
- MCP tool approval: `default_tools_approval_mode`: `approve` runs silently (works even under `never`); `prompt`/unset
  raises `mcpServer/elicitation/request` (`_meta.codex_approval_kind: mcp_tool_call`, `tool_params`), decline gives
  item `failed "user rejected MCP tool call"`.
- UNVERIFIED: Plan-mode refusal (`collaborationMode`) for a child; user-config MCP servers inheriting into a read-only
  child (they do start, see D1). Refuse a combination whose sandbox/approval does not read back as requested.

## D7 lifecycle: PARTIAL

- Completion: `turn/completed {turn.status: completed|interrupted|failed}`; `thread/status/changed` brackets it.
- Interrupt: `turn/interrupt {threadId, turnId}` -> `{}` immediately, then `turn/completed` `interrupted`. See surprise 6.
- Server death mid-call: item `failed` (`Transport closed`), turn still `completed` (model reports it); subsequent calls
  fail the same way (surprise 7). `startupStatus` stays `ready`.
- App-server exit kills its MCP children (no orphan found; no graceful stdin EOF logged), so closing stdin is a hard
  disconnect for the bridge.
- UNVERIFIED: bridge reconnect while app-server stays up (the MCP transport is not re-established), root-stop cascade
  to grandchildren.
