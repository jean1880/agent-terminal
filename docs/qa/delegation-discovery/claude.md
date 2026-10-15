# Delegation discovery: Claude Code as parent and child (D1-D7)

Claude Code 2.1.300, recorded 2026-10-14. Fixtures and exact commands: `tests/fixtures/real/delegation/claude/` (README there).
Claude is the same process in both roles: a "parent" is a Claude thread that calls our MCP bridge, a "child" is a Claude
worker the supervisor launches. Both use the app's stream-json argv. Evidence paths below are fixture directories.

## Summary

| Gate | Verdict | One line |
| --- | --- | --- |
| D1 parent binding | PROVEN | `--mcp-config` overlay spawns the bridge per launch, parent env reaches it, overlay wins by name |
| D2 registration | PROVEN (overlay) / persistent not run | no touch of `~/.claude.json`; `claude mcp add` scopes documented only |
| D3 correlation | PROVEN | `_meta."claudecode/toolUseId"` equals the stream `tool_use.id` |
| D4 model/effort | PARTIAL | model confirmed by `assistant.message.model` and `modelUsage`; effort never reported |
| D5 delivery | PROVEN | live steer folds into the open turn; idle follow-up and resume both work; `--replay-user-messages` acks consumption |
| D6 safety | PARTIAL | enforceable only with `--restricted`; ambient user rules otherwise bypass the host's approval path |
| D7 lifecycle | PROVEN | result event ends a turn; the CLI restarts a dead stdio server; interrupt returns a distinct result |

## D1 parent binding: PROVEN

Command: `claude <app flags> --mcp-config overlay.json --strict-mcp-config` (`d1_basic`).

- Init frame lists the server with `"source":"dynamic"` and registers tools `mcp__fake__echo` etc.
  (`system/init` `mcp_servers:[{"name":"fake","status":"connected","source":"dynamic"}]`).
- Server saw: `server/discover` probe (we must answer JSON-RPC `-32601`; Claude then falls back), then
  `initialize` (`protocolVersion "2025-11-25"`, `clientInfo.name "claude-code"`, version 2.1.300, capabilities
  `roots`, `elicitation`), `notifications/initialized`, `tools/list`, `tools/call`.
- Environment: the claude process's own variables reach the server (the fake `FAKE_BEARER` check logged
  `fake_bearer_matches:true`), as do the overlay's `env` entries (`OVERLAY_VAR`). Claude also injects
  `CLAUDE_CODE_SESSION_ID` (equal to the init `session_id`, verified by hash), `CLAUDE_PROJECT_DIR`, `CLAUDE_PID`,
  `CLAUDE_CODE_MESSAGING_SOCKET` and `CLAUDE_CODE_MESSAGING_TOKEN` (names only recorded). `CLAUDE_EFFORT` is inherited
  from the launching environment, not derived from `--effort` (it read `medium` under `--effort max`).
- Precedence (`d1_precedence`): an overlay server named `plans` replaced the user-scope `plans` server (a single `plans`
  entry, `source:"dynamic"`, tool answered `I am OVERLAY`). Without `--strict-mcp-config`, user-scope servers still load
  (19 here) and cost startup time.
- Restart: see D7. The cwd of the server is the claude cwd.

Contract consequences:
- Pass the bearer capability as an overlay `env` entry (or inherit it); never in argv or a file. It reaches every
  respawn of the bridge because the overlay env is re-applied.
- Always use `--strict-mcp-config` for children (no ambient servers, no name collisions), and name our server
  something unlikely to collide (a user server with the same name is silently shadowed, which is fine for us but
  means a user-registered `cross-agent` would be replaced for that launch only).
- The bridge must answer unknown methods with `-32601` rather than hang (`server/discover`).
- Claude may defer MCP tool schemas behind its `ToolSearch` tool (the model needed one extra turn to load
  `mcp__fake__echo`); tool descriptions must be self-explanatory and the first call may be preceded by a ToolSearch.

## D2 registration: PROVEN for the overlay

Overlay requires no persistent state: the runs above read nothing from `~/.claude.json` for the bridge, and an
isolated `CLAUDE_CONFIG_DIR` (empty dir) also connected the server (init showed it `connected`) before failing auth
with `Not logged in`. So the overlay needs no home access; real auth is the only reason a home is needed.

Persistent registration (documentation only, NOT run): `claude mcp add [-s local|user|project] [-e K=V] <name> -- <cmd> [args]`
writes `local` (default, per-project in `~/.claude.json`), `user` (`~/.claude.json` top level) or `project` (`.mcp.json`
in the repo, needs per-user approval). The app should not use it: overlays are enough for app-launched sessions.
Only an external, non-app Claude (a terminal the user starts by hand) would need persistent registration, and for that
the bridge must refuse to run without the app endpoint (`runtime_unavailable`).

## D3 invocation correlation: PROVEN

`tools/call` params contain `_meta`:

```
"_meta": {"claudecode/toolUseId": "toolu_01JqmgHtuKFaLatCTWfyf523", "progressToken": 2}
```

and the stream's `assistant` frame carries `{"type":"tool_use","id":"toolu_01JqmgHtuKFaLatCTWfyf523","name":"mcp__fake__echo"}`;
the following `user` frame's `tool_result.tool_use_id` is the same id. `progressToken` is a per-connection counter
(restarts at 1 or 2 on a new process), not a stable id. Long calls emit `tool_progress` heartbeats every 30 s with
`tool_use_id "<id>-heartbeat-0"` and `parent_tool_use_id "<id>"`.

Consequence: link bridge request to the native call card by `_meta["claudecode/toolUseId"]`; treat absence as
unlinked (the principal still authenticates). Parallel calls in one batch are distinguishable by this id.

## D4 model/effort: PARTIAL

- Alias in, resolved model out: `--model haiku` gives init `model:"claude-haiku-5-5"`; the `initialize` control response
  has `models[]` with `value`, `resolvedModel`, `supportsEffort`, `supportedEffortLevels` (opus 4.6 lacks `xhigh`).
- Init `model` echoes the requested string even when invalid: `--model claude-nonexistent-9` still produced an init with
  that name, then the first request failed: synthetic assistant text "There's an issue with the selected model", `result`
  `is_error:true`, `terminal_reason:"api_error"`, process exit 1, stderr `[claude-code:unrecognized_model]` (`d4_badmodel`).
  There is no silent fallback to another model. `--fallback-model` exists but was not exercised.
- Provider-confirmed model = `assistant.message.model` and the key of `result.modelUsage`. Init alone is not confirmation.
- **Surprise:** in `--permission-mode plan` the assistant model was `claude-sonnet-5-5` while `--model haiku` was requested
  (`d6_plan`). Effective model can differ by mode; record it from the first assistant frame of each turn.
- Effort: `--effort low|max` is accepted but NOTHING in init, assistant, or result frames reports it (`d4_effort_*`);
  `--effort bogus` is silently accepted with rc 0 (`d4_badeffort`). `per_turn_effort_active:true` in init is the only hint.
  Validate against `models[].supportedEffortLevels` ourselves.

Consequence: effort provenance is `launch_configured` only; model provenance is `provider_confirmed` after the first
assistant frame. Reject unknown effort before launch; treat `api_error` result with `unrecognized_model` as `provider_unavailable`.

## D5 delivery: PROVEN

- Busy child: a `user` line written while a turn is running is accepted and folded into that turn: one `result`, no new
  init, final text `alpha BRAVO` (`d5_busy_replay`). But it is consumed only after the in-flight tool returns
  (6 s MCP call: the line was written at t=6.2, consumed at t=14.0). So steering never interrupts a running tool call.
- `--replay-user-messages` re-emits each user line with `"isReplay":true`, `uuid` and the original `timestamp`, the first at
  turn start and the mid-turn one at the moment it is consumed. That is the delivery receipt: queued (written) versus
  consumed (replayed). Use it for `this_run` receipts.
- Idle follow-up on the same process: a new `user` line after `result` starts a new turn with a fresh `system/init`
  (same `session_id`, same MCP server pid, no respawn) (`d5_busy_replay`, `CHARLIE`).
- Resume: `--session-id <uuid>` then a new process with `--resume=<uuid>` recalled the earlier turn (`PINEAPPLE`) and
  re-spawned the MCP bridge with the overlay env (`d5_idle_*`). Persistence writes the transcript into the real
  `~/.claude/projects/` (isolated config has no auth), unless `--no-session-persistence`.
- Turn-end race: a user line written exactly at turn end starts the next turn (existing adapter comment, consistent with
  the idle case); not exercised under load.

Consequence: `this_run` is supported for Claude, with the replay receipt; deliver `next_turn` as a plain user line to an
idle process; on respawn use `--resume` and re-supply the capability env.

## D6 safety: PARTIAL

- Plan mode (`d6_plan`): Write was redirected to a plan file under `~/.claude/plans/` (outside the workspace, in the user
  home: Claude's own behaviour, I removed the file afterwards), MCP calls were not executed, and `ExitPlanMode` arrived as
  a `can_use_tool` control request which the host denied (`permission_denials` in the result). So plan = no workspace
  edits and no MCP calls, but it does write to `~/.claude/plans/`.
- **Surprise:** `--permission-mode default` (the app's Ask) did NOT ask for Write even with `--setting-sources local`:
  `tool_result_meta.permission_decision = {decision:"accept", source:"user_permanent", reason_type:"rule"}`; a Write outside
  the cwd was also created (`d6_ask_user_rules`). Ambient user permission rules (from the user's real settings) bypass
  the host approval path, and `--setting-sources` did not drop them. A child launched this way is NOT enforceably "Ask".
- `--restricted` fixes it (`d6_restricted`): user/project/local settings ignored; Write inside the cwd produced
  `can_use_tool` (deny honoured, `permission_denials` populated); Write outside the cwd was hard-denied with
  `system/permission_denied` "--restricted: path outside the working directory"; MCP calls produced `can_use_tool` with
  `mcp_server:{name,source:"dynamic"}` and `permission_suggestions` (localSettings rule: never apply those). Cost: Bash and
  other code-running tools are removed unless named by `--tools`, and `bypassPermissions` is refused.
- Hooks/prompt tool: `--permission-prompt-tool stdio` routes all prompts to our host as `can_use_tool` frames; user
  `SessionStart` hooks still ran in unrestricted launches (visible as `hook_started` frames).

Consequence: spawn Claude children with `--restricted --strict-mcp-config` plus explicit `--tools` for the task mode,
and answer `can_use_tool` ourselves; refuse `accept_edits`/`ask` children if `--restricted` is unavailable (older
CLI). An MCP call by the child to our own bridge is a `can_use_tool` the host should auto-allow by `mcp_server.name`.
Plan children need the home-write exception documented or a separate `--settings` plan directory (not tested).

## D7 lifecycle: PROVEN

- Turn completion: exactly one `result` frame per turn (`subtype`, `terminal_reason:"completed"`, `modelUsage`).
- MCP server death mid-call (`d7_crash`, `d7_killmcp`): the in-flight call gets a `tool_result` with
  `is_error:true, content:"Connection closed"` immediately (<0.1 s after the kill); Claude respawns the stdio server
  automatically (new pid, full re-initialize, same env and same session id) and the next call succeeds in the same turn.
  The turn does not fail. A model may retry; a retried non-idempotent call would run twice, so the bridge must use the
  idempotency keys.
- Interrupt (`d7_interrupt`): `control_request {subtype:"interrupt"}` during a 30 s MCP call answered
  `control_response success {still_queued:[]}` at once, the tool result became "The user doesn't want to proceed...", and
  `result` was `subtype:"error_during_execution", terminal_reason:"aborted_tools", is_error:true`; the process then
  exited rc 1 when stdin closed. No MCP cancellation was observed reaching the server before the CLI killed it, and no
  fake server process was left behind (checked with pgrep).
- Exit: closing stdin ends claude; the bridge sees stdin close.

Consequence: the bridge must tolerate being restarted and killed at any time and be stateless per process (state in the
app); in-flight bridge operations at death resolve as `transport_lost`/`delivery_unknown` on the supervisor side;
cancel = `interrupt`, then terminal only after the `aborted_tools` result or process exit.

## Open items for the coordinator

- Not exercised: `--fallback-model`, nested Claude-as-parent spawning a Claude child (needs the real bridge), plan-mode
  home write relocation, wait semantics beyond a 30 s call.
- Deferred-tool `ToolSearch` indirection may hide `cross_agent_*` descriptions; consider whether an always-load hint exists.
