# Cross-agent delegation contract, v1 (frozen candidate, revision 6)

Status: revision 6 of the frozen candidate, 2026-10-15, after five rounds of Codex and agy cross-harness reviews and the
user's decisions of 2026-10-15 (resolution tables: `docs/delegation/contract-v1-review.md`). Baseline: branch `feat/cross-agent-runtime`; provider versions Claude Code
2.1.300, Codex 0.161.0, agy 1.3.5; rmcp =3.5.1. Evidence: `docs/qa/delegation-discovery/{claude,codex,agy,sdk-and-store}.md`
and `tests/fixtures/real/delegation/`. Plan: `plans/2026-10-14_cross-agent-runtime-implementation.md` (this contract wins where
they differ; differences are listed there under "P0 amendments").

Changing anything here after P1 starts needs a new contract version or an explicit coordinator decision on Hive topic 40.

Proof gates are named `G-<provider><n>` (C Claude, X Codex, A agy). A gate is a recorded P5/P8 fixture. Until it passes,
whatever depends on it is refused (fail closed), unless the text says the gate is advisory.

Implementation amendments approved by the coordinator, 2026-10-15:

- Schema 5 adds each event link's process generation independently of its run. A new process's
  bootstrap or session events must never be replayed under the previous generation. The normal
  backup and migration transaction upgrades schema 4; historical links retain zero for unknown.
- A deliberate credential rotation within the same provider, profile, policy and workspace
  invalidates the old generation's token immediately and expires its approvals. It does not
  also apply a synthetic `GenerationExited` authority-epoch bump: that would invalidate the
  already accepted follow-up which required the rotation. Capacity and overlay removal still
  wait for observed process exit. A real Stop, Close, restriction, configuration change or
  unexpected exit retains its revocation and wins over a pending rotation.
- Claude Code 2.1.300 does not expose `TodoWrite` in strict tool readback. Explicit child tool
  lists omit that obsolete name; all remaining requested tools still require exact readback.
- Codex 0.161.0 executes native `collaboration.spawn_agent` despite both process and thread
  feature disables. Codex child routes are `unsupported` until a new enforcement proof passes.
  Its parent role is independently admissible. See `docs/qa/delegation-route-evidence.md`.

## 1. Names and versions

| Item | Value |
| --- | --- |
| Control contract version | `api_version: 1` (IPC hello negotiates `min..=max`; v1 accepts exactly 1) |
| MCP server name (all providers) | `agent_terminal` |
| Bridge command | absolute installed path of `agent-terminal`, args `["--cross-agent-mcp"]`, dispatched before GTK (`src/main.rs`, same boundary as `--approval-hook`) |
| MCP SDK | `rmcp =3.5.1`, `default-features=false`, features `server`, `transport-io`; tokio `rt,time,io-std`, `current_thread`, `enable_time()` mandatory |
| `serverInfo` | `{name: "agent-terminal", version: CARGO_PKG_VERSION}` (set explicitly; rmcp defaults to its own) |
| Tool names | `cross_agent_capabilities`, `cross_agent_spawn`, `cross_agent_send`, `cross_agent_read`, `cross_agent_wait`, `cross_agent_list`, `cross_agent_cancel`, `cross_agent_close` |
| Provider-visible tool ids | Claude `mcp__agent_terminal__<tool>`; Codex `agent_terminal` server; agy `mcp(agent_terminal/<tool>)` |

MCP behaviour (rmcp handles the lifecycle; the handler must do the rest):
- `server/discover` (Claude, agy) and `initialize` (all three) are both served; unknown methods get `-32601`.
- Unknown tool name: JSON-RPC `-32602`. Arguments that fail the schema: tool result `isError: true`, code `invalid_argument`
  (rmcp does not validate `inputSchema`).
- Success: `CallToolResult::structured(ok)`, i.e. `structuredContent` plus the same JSON as a text block. Failure:
  `structured_error({"error": ToolError})`.
- `notifications/cancelled` → IPC `cancel` for that call (it only releases a `wait`; it never undoes a mutation).
  No provider was observed sending it on interrupt (claude.md D7, codex.md surprise 6), so the supervisor also releases a
  principal's waits when its turn ends.
- Tool descriptions must make sense on their own: Claude may hide them behind `ToolSearch` (claude.md D1).
- **Request id bound.** rmcp accepts JSON-RPC ids that are strings of any length, and every response echoes the id. So
  before any dispatch, the bridge checks the id's **encoded** JSON length (quotes and escapes included): at most
  `MAX_REQUEST_ID_BYTES = 256` B.
  - A longer id is refused with JSON-RPC error `-32600` (Invalid Request), carrying `"id": null`, so the oversized id
    is never echoed. The call never reaches the supervisor.
  - The same check covers notifications that carry an id, and a `notifications/cancelled` that names one.
  - Every response is therefore at most `OUTPUT_BUDGET` + 256 B + the fixed envelope, under 1 MiB.
  - A boundary test sends ids of 256 B and 257 B, string and number forms, including one with escaped characters.
- **Stdio input bound.** The bridge reads MCP messages through its own bounded line reader: at most 1 MiB per
  newline-delimited message, checked before any JSON parsing. A longer line ends the bridge process (stdio cannot
  resynchronise). A unit test feeds a 1 MiB + 1 line.

**Startup and degraded mode.** The bridge answers `initialize` at once and starts its hello in parallel. `tools/list`
waits for the hello outcome, but never past the **welcome deadline: 2 s from process start**, covering connect, hello
and welcome. The bridge is degraded when any of these holds:
- there is no `AGENT_TERMINAL_DELEGATION_ENDPOINT`;
- the **lineage self-check** fails (below);
- connect fails;
- the hello is `refused`;
- the deadline expires;
- the connection is lost and the one reconnect fails (§4).

**Lineage self-check.** Before any hello, the bridge reads its parent pid P, then P's parent from `/proc/P/stat` (parsed
after the last `)`). That grandparent must equal `AGENT_TERMINAL_DELEGATION_APP_PID`. The app spawns every provider
directly with `gio::SubprocessLauncher`, so the provider's parent is the app. A bridge started by a nested process, such
as a native sub-agent, a profile wrapper that does not `exec`, or a provider started by the agent, fails the check. It
then never says hello, so it never consumes or presents the token.
- For agy, the self-check is mandatory.
- For Claude and Codex, the bridge only logs a failure and still says hello, until P5 fixtures confirm the parent
  chain. For Codex, the chain to check is MCP process → app-server → app. In degraded mode
`tools/list` returns only `cross_agent_capabilities`, which returns `runtime_unavailable`, or the refusal code, with an
explanation. Degraded mode lasts for the life of that bridge process. The bridge answers within the deadline. This
applies to the user's own terminal agy sessions, because the global entry starts in every agy session (agy.md D2).

## 2. Common types (JSON)

- Handles are opaque ASCII, `^[a-z]{1,3}_[A-Za-z0-9]{8,40}$`, with prefixes `w_` worker, `r_` run, `m_` message and `op_`
  operation. The bridge never interprets them, and they are never native ids.
- `cursor`: opaque string, at most 256 bytes.
- `idempotency_key`: `^[A-Za-z0-9._:-]{1,128}$`.
- `profile_id`: `^[a-z0-9-]{1,64}$`.
- `expected_authority_epoch`: integer ≥ 0, **optional**. When present and not current: `stale_epoch`. When absent, the
  current epoch applies, and the receipt echoes the epoch the operation was accepted under.

### Canonical request

- It is the request after validation and **syntax normalisation**: every omitted optional field is filled with its
  documented default, and `idempotency_key` and `expected_authority_epoch` are removed.
- Every free-text field is scrubbed with `redact`, then the result is serialised as JSON with sorted keys. It is stored
  and compared byte for byte; it is not hashed.
- It holds the request's **syntax, not its resolution**: `"parent"` stays `"parent"`, and `base: "HEAD"` stays `"HEAD"`.
- What the request resolves to is computed once, at acceptance, and stored with the operation as its `resolution`:
  - the target worker;
  - the run(s) a cancel is bound to (§3);
  - the base commit.
- Dispatch uses that stored resolution and never resolves again, so a replay after HEAD moved still refers to the
  original commit.
- Dispatch sends exactly the scrubbed text that was compared. Two requests whose secrets scrub to the same text are the
  same request, and no secret is ever delivered to a child.

### Idempotency and replay

Idempotency is unique on `(principal_worker_id, operation_kind, idempotency_key)` across all epochs. The checks run in
this order (the first that applies wins):

1. `expected_authority_epoch` is present and not current: `stale_epoch`, before any lookup.
2. The key is stored under an older epoch: `stale_epoch`, with `details.operation_id`. An old request is never re-run
   under a new epoch, even when it is identical.
3. The key is stored under the current epoch with a different canonical request: `idempotency_conflict`, with
   `details.operation_id`.
4. The key is stored under the current epoch with the identical canonical request: a **replay**, decided by the
   operation's stored dispatch state:

| Stored dispatch state | Replay returns |
| --- | --- |
| `accepted` (never reserved) | the original receipt, `replayed: true`, `dispatch: "accepted"` |
| `dispatch_reserved` while this app instance is dispatching it | waits, bounded by the call timeout, for the state to move on, then applies this table |
| `unknown` (including any reservation found after a restart) | `delivery_unknown`, `details: {operation_id, receipt: <original receipt>}`. Never re-dispatched |
| `submitted`, `confirmed` | the original receipt, `replayed: true`, `dispatch` = the current state |
| `failed` | the stored error, with `details.operation_id` and `details.replayed: true` |

The original receipt is stored when the operation is accepted, separately from its dispatch state (§8 item 5), so a
replay never re-derives it.
- **Frozen fields.** The receipt holds only what is known at acceptance, and it is never updated. A replay refreshes
  exactly two envelope fields, `replayed` and `dispatch`.
- **Facts learned later** are never written into the receipt: a message's observed recipient run and terminal delivery
  state, a run's outcome, an identity observation. They live on the records `read`, `wait` and `list` return:
  `PageMessage.delivery` and `recipient_run_id`, `WorkerSummary`, and `PageEvent`.

### Attestation arguments (separate from the canonical request)

The canonical request above identifies an operation for idempotency, so it is scrubbed and leaves out the key and the
epoch. Attestation (§4) asks a different question: is this exactly the call the model made? It therefore uses its own
form, built by **one** helper, `attestation_arguments(tool, raw)`. The same helper is used for the arguments seen in the
provider stream (`record`) and for the bridge call's arguments (`attest`):
- `raw` is a JSON object. A JSON string holding an object is parsed once.
- It is validated with the tool's schema. An invalid value cannot be attested.
- It is syntax-normalised (every omitted default filled) and **includes every input field**, `idempotency_key` and
  `expected_authority_epoch` among them.
- It is **not scrubbed**: it is the original trusted input.
- It is serialised as sorted-key JSON.
- Only a salted, in-memory 128-bit digest of the result is kept. It is never persisted or logged.

So a substituted idempotency key, a dropped or changed epoch fence, or a different secret that would scrub to the same
text each produce another digest.

### Types

```jsonc
// Provenance of one identity value
"Provenance": {"enum": ["provider_reported", "provider_accepted", "launch_configured", "unknown", "unsupported"]}
//  provider_reported: the provider named the effective value (effective is set).
//  provider_accepted: the provider ran a turn with the requested value without refusing it; effective stays null.
//                     It is never an identity attestation.
//  launch_configured: we passed it; nothing came back.
"ValueProvenance": {"requested": "string|null", "effective": "string|null", "source": "Provenance"}
"Identity": {"driver": "claude|agy|codex", "profile_id": "string", "model": "ValueProvenance", "effort": "ValueProvenance"}
"TaskMode": {"enum": ["ask", "accept_edits", "plan"]}
"Workspace": {"oneOf": [
  {"kind": "shared_read_only"},
  {"kind": "isolated_worktree", "base": "string (commit-ish, default \"HEAD\" of the caller's tree; resolved to a commit at accept and stored)"}
]}
"WorkerState": {"enum": ["starting","running","waiting","completed","failed","stopping","stopped","closed","unknown"]}
"RunState": {"enum": ["reserved","starting","running","stopping","completed","interrupted","failed","unknown"]}
"DeliveryState": {"enum": ["queued","queued_next_turn","dispatch_reserved","submitted","provider_confirmed","misdelivered","failed","cancelled","unknown"]}
//  9 values (DeliveryState::ALL has 9, Unknown included). Terminal: provider_confirmed, misdelivered, failed, cancelled.
//  misdelivered: the provider consumed the message, but in a run other than the one targeted at acceptance (Claude
//  best-effort this_run landing in the next turn). The actual run is PageMessage.recipient_run_id. It is a delivery
//  fact, never an error.
"ThisRunSupport": {"enum": ["strict", "best_effort", "unsupported"]}
//  strict: the provider fences the run (Codex turn/steer expectedTurnId). best_effort: delivered while the run is live,
//  and the record reports misdelivered if it lands in the next turn (Claude). unsupported: agy.
"DelegationStatus": {"code": "available|workspace_untrusted|rebind_required|restricted|bridge_disabled|unavailable",
  "reason": "string|null (≤256 B, scrubbed)", "paths": ["string (relative, ≤256 B)"] /* ≤16; L2 offenders only */}
"DispatchState": {"enum": ["accepted","dispatch_reserved","submitted","confirmed","failed","unknown"]}
"ToolError": {"code": "ErrorCode", "message": "string (≤1 KiB, scrubbed)", "retryable": "bool", "details": "object|null (≤64 KiB serialised, MAX_ERROR_DETAILS_BYTES)"}
"Warning": {"code": "string (snake_case)", "message": "string (≤512 B, scrubbed)"}   // at most 8 per receipt; code ≤64 B
"Controls": {"send": "bool", "this_run": "ThisRunSupport", "cancel": "bool", "close": "bool"}
// Every mutating tool's receipt starts with this envelope (flattened into the tool's output object):
"MutationReceipt": {"api_version": 1, "operation_id": "op_", "authority_epoch": "integer (the epoch it was accepted under)",
  "replayed": "bool", "dispatch": "DispatchState", "warnings": ["Warning"] }
"StateRef": {"oneOf": [{"domain": "worker", "value": "WorkerState"}, {"domain": "run", "value": "RunState"}]}
"WorkerSummary": {
  "worker_id": "w_", "parent_id": "w_|null", "name": "string", "origin": "root|native|supervised",
  "state": "WorkerState", "run_id": "r_|null", "run_state": "RunState|null", "identity": "Identity|null",
  "depth": "0..2", "can_spawn": "bool", "controls": "Controls", "task_mode": "TaskMode|null",
  "workspace": {"kind": "...", "path": "string|null (display, home shown as ~, ≤1 KiB)", "base_commit": "string|null (≤64)"},
  "delegation": "DelegationStatus",
  "last_activity": "string|null (≤512 B scrubbed)", "updated_at": "RFC 3339"
}
// Output bounds (bytes of UTF-8 before JSON escaping):
//  - name ≤80 chars;
//  - identity model strings ≤128 and effort strings ≤32;
//  - profile_id ≤64;
//  - workspace.path ≤1 KiB, shortened in the middle with "…" when longer;
//  - last_activity ≤512; delegation.reason ≤256; ≤16 paths of ≤256 each.
// A serialised WorkerSummary is therefore ≤ 48 KiB, even with worst-case six-fold escaping (MAX_SUMMARY_BYTES).
"PageEvent": {"seq": "integer", "worker_id": "w_", "run_id": "r_|null", "at": "RFC 3339",
  "category": "lifecycle|activity|result|approval|message|error", "state": "StateRef|null",
  "summary": "string (≤2 KiB scrubbed, never token streams)"}
"PageMessage": {"seq": "integer", "message_id": "m_", "from": "w_", "to": "w_", "kind": "MessageKind", "reply_to": "m_|null",
  "text": "string (≤16 KiB scrubbed)", "delivery": "DeliveryState",
  "recipient_run_id": "r_|null (the run it was delivered into; null while queued or when pulled from a mailbox)",
  "acknowledged": "bool", "at": "RFC 3339"}
"MessageKind": {"enum": ["instruction","report","question","reply"]}
```

Direction rules: parent→child allows `instruction` and `reply`; child→parent allows `report`, `question` and `reply`.
Any other combination is `invalid_argument`. Text from a child is always labelled delegated data and is never a user
message. **Every parent→child delivery can drive the child's execution, whatever its kind.** So the mode ceiling (§6)
applies to every one of them, not only to `instruction`.

## 3. Tool schemas (input → structured output)

All inputs have `additionalProperties: false`. Bounds are enforced before any work starts. The outputs of `spawn`,
`send`, `cancel` and `close`, and read's `ack` object, all start with the `MutationReceipt` envelope (§2).

### Output budget (every tool)

Counts alone do not bound a response. Every tool output O is therefore measured at the real transport boundary.
- The supervisor computes `transport_cost(O)` = `len(json(O))` + `len(json_string(json(O)))`. That is the
  `structuredContent` plus the same JSON again as an escaped text block, the two representations the MCP stdio response
  carries (§1).
- O must satisfy `transport_cost(O) ≤ OUTPUT_BUDGET = 786_432` B (768 KiB). That leaves at least 256 KiB of the 1 MiB
  limit for the JSON-RPC and `CallToolResult` envelope. The IPC `result` frame carries `json(O)` once, so it always
  fits too.
- Escaping is counted exactly, because both lengths are measured on the real serialisation, not estimated.
- Each output reaches the budget by its own deterministic rule:
  - capabilities: below;
  - read and wait pages: §3 page rules;
  - list: §3 list;
  - receipts and errors cannot exceed it:
    - every receipt is at most `MAX_RECEIPT_BYTES = 48 KiB` serialised. That is sized from the maximal spawn
      receipt (8 warnings with six-fold escaping, identity and workspace path: about 34 KiB) and asserted by test.
    - `ToolError.details` is at most `MAX_ERROR_DETAILS_BYTES = 64 KiB` serialised, so
      `delivery_unknown.details = {operation_id, receipt}` always holds a maximal original receipt.
    - An error output is therefore at most about 71 KiB of JSON, or about 213 KiB of transport cost.
- `transport_cost` is checked again before sending. An output that still exceeds the budget is a **server bug**. It is
  replaced by `internal_error` (not retryable), with `details.reason: "output_budget"` and `details.operation_id` when
  an operation exists, and logged. It is never sent oversized.
  - The guard changes only what is sent. It never changes an operation's identity, stored receipt, dispatch state or
    outcome.
  - An accepted mutation stays accepted, and a later call with the same idempotency key replays it under the §2 table.
  - `internal_error` is never stored as an operation's error.

### `cross_agent_capabilities`
In: `{profile_id?: string, refresh?: bool}`.
Out:
```jsonc
{"api_version": 1,
 "caller": {"worker_id": "w_", "root_id": "w_", "driver": "...", "depth": 0, "can_spawn": true,
            "task_mode_ceiling": ["ask","plan"]},          // live: recomputed on every call (§6)
 "authority_epoch": 4,
 "delegation_scope": "supervised",                          // §6: limits apply to agents started through agent-terminal
 "profiles": [{"profile_id": "...", "name": "...", "driver": "codex", "available": true,
   "reason": "string|null",
   "modes": [{"task_mode": "plan", "route_status": "verified|unverified|unsupported", "route_blockers": ["string"],
              "child_bridge": "with_bridge|without_bridge"}],   // one row per TaskMode (route admission, §11)
   "models": [{"id": "string", "display": "string", "efforts": ["string"], "default_effort": "string?",
               "tags": ["code","review"], "tier": "fast|standard|deep?", "best_for": "string?",
               "effort_for": {"fast": "low", "standard": "medium", "deep": "high"}}],  // advisory (codex.md D4)
   "usage": {"headroom": "ok|low|exhausted|unknown", "load": 0.42, "age_s": 30,
             "windows": [{"group": "string?", "label": "5-hour", "used": 0.42, "resets_at": "RFC 3339?"}],
             "refresh_failed": false, "signed_out": false},
   "workspaces": ["shared_read_only","isolated_worktree"],
   "delivery": {"this_run": "strict|best_effort|unsupported", "next_turn": true}, "controls": {"cancel": true, "close": true}}],
 "limits": {"max_task_bytes": 65536, "max_message_bytes": 16384, "max_page_events": 500, "max_page_bytes": 262144,
            "max_wait_ms": 30000, "max_depth": 2, "active_runs_per_root": 4},
 "tag_vocabulary": {"code": "writes/refactors code"}, "tags_status": "string|null",
 "effort_policy": {"ladder": ["low","medium","high"], "fast": "low", "standard": "medium", "deep": "high",
                   "reserved": ["xhigh","max"]},
 "truncated": false, "omitted": {"profiles": 0, "models": 0}}
```
- It never contains environment values, tokens, socket paths or account labels.
- **Tags, tier, `best_for` and `effort_policy`** come from the model tag file (`model_tags_path` in the config,
  default `<config dir>/model-tags.json`). It is read off the main thread and parsed again only when its mtime or
  size changes. A missing or invalid file never fails the call: models are listed with empty tags and
  `tags_status` says why. Patterns match model ids case-insensitively with `*` wildcards; the first entry wins.
- **`effort_for`** is each tier's policy effort clamped to the efforts the model offers: the effort itself if
  offered, else the highest offered below it on the ladder, else the lowest offered. A `reserved` effort is never
  recommended. Null when there is no policy or no tier gets an effort. agy takes the effort in the model id
  (`gemini-4-argon-high`): a spawn composes it.
- **`usage`** is the agent's quota reading. `load` is the fullest window's share used, the value to rank agents
  by; it is null exactly when `headroom` is `unknown` (no windows, or a reading older than 20 min). `headroom` is
  `low` at 0.80 and `exhausted` at 0.98. The same reading is written, without any account field, to
  `$XDG_CACHE_HOME/agent-terminal/usage.json` (0600, atomic, one write in flight) for readers outside
  agent-terminal.
- **`refresh: true`** probes, before answering, each agent whose last reading or attempt is at least 60 s old, but
  never a signed-out agy (its login opens a browser). It waits for those probes at most
  min(20 s, `max_wait_ms`); a probe still running then is left to finish. The caller's authority is checked
  again after every wait.
- `delivery.this_run`: Codex `strict`, Claude `best_effort`, agy `unsupported`.
- **Per-field bounds**, enforced when the catalogue is read:
  - at most 32 profiles, with names of at most 80 chars and `reason` at most 256 B;
  - at most 64 models per profile, each with at most 8 efforts; ids at most 128 B, efforts at most 32 B;
  - at most 16 blockers per mode, each at most 64 B.

  These alone do not fit the budget: a maximal ASCII catalogue serialises to about 0.86 MiB, and about 1.7 MiB as an
  MCP response.
- **Truncation to the output budget.** It is deterministic, and it runs after the per-field bounds:
  1. Profiles are in Settings order and models in the provider catalogue's order. Nothing is ever reordered.
  2. **Models are dropped before profiles.** While `transport_cost` exceeds the budget, the last model of the last
     profile that still lists a model is removed. Only when no profile lists a model is the last whole profile
     removed. The first profile is never removed.
  3. A profile is kept or dropped whole. Its identity and status fields are never dropped or shortened:
     `profile_id`, `name`, `driver`, `available`, `reason`, `modes`, `usage`, `workspaces`, `delivery` and
     `controls`. `tag_vocabulary` and `effort_policy` are never cut, and count toward the budget.
  4. `truncated` is true if anything was removed. `omitted.profiles` and `omitted.models` count the removed entries.
     A profile whose model list was cut is still in the response; its missing models count in `omitted.models`.
- One profile with no models costs at most 64 KiB (about 21 KiB of JSON, tripled), even with worst-case escaping, so truncation always ends
  within the budget.
- With `profile_id`, only that profile is returned, under the same rule.
- Models are advisory (codex.md D4). A spawn naming a model that was omitted is still judged against the full catalogue
  (§3 spawn rules).

### `cross_agent_spawn`
In: `{profile_id, name (1..80 chars), task (1..65536 B), task_mode: TaskMode, model?: string(≤128), effort?: string(≤32), workspace: Workspace, expected_authority_epoch?, idempotency_key}`.
Out: `MutationReceipt` + `{worker_id, run_id, state: "accepted"|"queued"|"starting",
identity: Identity (model/effort source launch_configured or unknown), authority: {task_mode, workspace: {kind, path, base_commit}, can_spawn, depth}}`.
Rules:
- `task_mode` must be in the caller's live ceiling (§6).
- The route must be admitted for `(parent driver, child driver, task_mode, child bridge)` (§11), else
  `route_unverified`. For an agy child:
  - with its bridge blockers unproven, or when §7 L2 withholds the token for its worktree, the child runs **without a
    bridge**, if that variant is admitted;
  - its `WorkerSummary` then has `delegation.code: "bridge_disabled"` and `can_spawn: false`;
  - it reports only through its final result, and it can still receive `next_turn` sends.
- `ask` and `accept_edits` require `isolated_worktree`. `shared_read_only` requires `plan` and a provider read-only
  enforcement that is proven (§5).
- A model or effort the provider's catalogue does not list:
  - Claude refuses an effort outside `models[].supportedEffortLevels` (`unsupported_effort`).
  - Codex accepts it with `warnings: [{code: "effort_not_listed"}]`.
  - agy uses the adapter's composed id.

### `cross_agent_send`
In: `{target: "parent" | w_, text (1..16384 B), kind: MessageKind, reply_to?: m_, delivery: "auto"|"this_run"|"next_turn", expected_run_id?: r_, expected_authority_epoch?, idempotency_key}`.
Out: `MutationReceipt` + `{message_id, delivery: DeliveryState, target_run_id: r_|null}`.
The receipt is frozen at acceptance (§2):
- `delivery` is the state at acceptance;
- `target_run_id` is the run this delivery was aimed at when accepted, or null for `next_turn` and queued delivery.

Where the message actually landed is reported only on the message record: `PageMessage.delivery` and
`recipient_run_id`.

Rules:
- A parent→child send is refused with `authority_denied` when the recipient's `task_mode` is outside the sender's live
  ceiling, whatever the `kind` (§6). The check runs at acceptance and again at dispatch (E7).
- `this_run` follows the profile's `delivery.this_run`. In every case, no live run or a run that does not match
  `expected_run_id` gives `stale_run`.

  | Provider | `this_run` | How it is delivered and reported |
  | --- | --- | --- |
  | Codex | `strict` | `turn/steer` pinned to the run. A refused steer is `stale_run`, and nothing is delivered |
  | Claude | `best_effort` | The line is written only if the target run is still live at dispatch. If that recheck fails, the message becomes `failed`, with `stale_run` as its reason, and nothing is written. The record then becomes `provider_confirmed` when the echo appears in the targeted run; `misdelivered`, with `recipient_run_id` set to the actual run, when it appears in a later turn; or `unknown` if no echo arrives before the process exits |
  | agy | `unsupported` | `unsupported_delivery` |
- `auto`:
  - Codex steers and records `queued_next_turn` if the steer is refused.
  - Claude writes a live user line with `target_run_id: null`. The record gets `provider_confirmed` with the run the
    echo appeared in. `auto` is never `misdelivered`, since it targets no run.
  - agy queues for the next turn.

### `cross_agent_read`
In: `{target?: w_ (default: caller's own mailbox and subtree), after?: cursor, max_events?: 1..500 (100), max_bytes?: 1024..262144 (65536), acknowledge?: [m_] (≤100), idempotency_key? (required iff acknowledge), expected_authority_epoch?}`.
Out: `{api_version, snapshot: WorkerSummary|null, events: [PageEvent], messages: [PageMessage], next_cursor, truncated: bool, epoch_changed: bool, ack: (MutationReceipt + {acknowledged: [m_], rejected: [{message_id, code}]})|null}`.

Page rules (also for `wait`):
- **Order and units.** Events and messages share one total order: the root's committed `seq`.
  - Each `seq` is one **indivisible unit**, with exactly one representation in a page. A message's `seq` appears only
    in `messages`; its event is never repeated in `events`. Every other `seq` appears only in `events`.
  - `max_events` counts units across both arrays. Each array is in ascending `seq`.
  - `next_cursor` resumes after the highest `seq` returned, and every lower `seq` that matches the filter was returned.
- **Cursor binding.** A cursor encodes the root, the stream epoch, the last `seq` and a digest of the filter it was issued
  for: caller, `target` or `workers`, and `mailbox`.
  - Reusing it with another filter gives `invalid_argument` with `details.field: "after"`.
  - A cursor from an older stream epoch gives `cursor_expired`. Re-read with no cursor.
- **Bytes.** `max_bytes` counts the serialised JSON of `snapshot` (first), then of each unit. The envelope fields are
  not counted. A unit is included whole or not at all.
- **Output budget.** The page's measured `transport_cost` never exceeds `OUTPUT_BUDGET` (selection and measurement
  below). Units are taken in `seq` order, a page never skips a unit to fit a later one, and a cut page has
  `truncated: true`.
- **Selection, then measurement.** Units are chosen with a conservative reservation, then the real page is measured.
  1. **Reservation (an upper bound, not exact).** The assembler adds units in `seq` order while two checks hold:
     - the `max_bytes` check on JSON length: the snapshot's `len(json)`, plus each unit's `len(json)`;
     - the `OUTPUT_BUDGET` check on transport cost: the **maximal envelope reservation** plus, per unit,
       `len(json(unit)) + len(json_string(json(unit))) + 4`.

     The maximal envelope reservation is the `transport_cost` of the page serialised with empty unit arrays and every
     variable envelope field at its maximum: `snapshot`, `ack`, a 256 B `next_cursor`, and all flags present. The
     `+ 4` per unit covers a separator and slack in both copies. These figures may overestimate; they are never claimed
     to be exact.
  2. **Measurement.** The assembler serialises the actual final page and measures its real `transport_cost`, that is
     `structuredContent` plus the escaped text copy.
  3. **Correction.** If the measured cost exceeds `OUTPUT_BUDGET`, the last unit is dropped and the page is measured
     again. This repeats, but **the first unit is always kept**. `truncated` is set, and `next_cursor` follows the last
     unit kept.
  4. `internal_error` is reached only if a page with just the snapshot and one unit is still over the budget. That
     cannot happen under the bound below, so it would be a bug.

  The reservation can stop early, but it always includes the first unit, so a page with any unit available advances
  the cursor.
- **Minimum complete page.** The minimum is the envelope plus one unit. Its parts, in JSON:
  - fixed fields and cursor: ≤2 KiB;
  - `ack`: ≤64 KiB, i.e. a ≤48 KiB receipt plus at most 100 acknowledged ids and 100 rejections;
  - snapshot: ≤48 KiB;
  - one unit: ≤100 KiB.

  That is at most 214 KiB of JSON, or at most 642 KiB + 2 B of transport cost (the array separators), under
  `OUTPUT_BUDGET`. So a valid request always gets a page that advances the cursor whenever a unit is available. It
  never gets an error instead.
- **Progress.** The snapshot, when requested, is always included. If any unit is available, the page holds at least
  one, even if the snapshot plus that unit exceed `max_bytes`. The outcome is deterministic, `truncated: true`, never an
  error.
- **Fit.** The text block re-escapes the JSON, which at most doubles it, so `transport_cost` is at most 3 × `len(json)`.
  The minimum complete page above fits, so progress never conflicts with the budget, and `encode_frame` never refuses
  a page the rules allow.
- `truncated` means that more entries were available when the page was built.
- **ACK eligibility.** A message can be acknowledged by its recipient principal only after a page has returned it to that
  principal, and only if it is not already acknowledged. Acknowledging it again is idempotent: it is listed in
  `acknowledged`.
- **ACK rejections.**
  - An invisible or unknown id: `not_found`.
  - An id not yet presented: `invalid_argument`, with `details.reason: "not_presented"`.
  - A rejected entry consumes nothing else.
- Reading never acknowledges. ACKs are applied before the page is built.

### `cross_agent_wait`
In: `{workers?: [w_] (≤32), mailbox?: bool (true), after?: cursor, timeout_ms?: 0..30000 (10000), max_events?, max_bytes?}`.
Out: the same page as `read` (with `ack: null`), plus `timed_out: bool`. A timeout never stops a worker and never means
failure. Waits are released on disconnect, on cancel, and when the principal's turn ends.

### `cross_agent_list`
In: `{target?: w_ (subtree root, default caller), cursor?: cursor, limit?: 1..100 (50)}`.
Out: `{api_version, workers: [WorkerSummary], next_cursor: cursor|null, truncated: bool}`. Only the caller's descendants are listed;
never ancestors or other roots. A list cursor is bound to its `target` (same rule as above).
- **Bounds.** A list page uses the same selection and measurement as pages: a conservative reservation, then the real
  response is measured, and the last worker is dropped while it is over budget. It always holds at least one worker:
  48 KiB × 3 = 144 KiB of cost. The first worker that does not
  fit ends the page, and `next_cursor` resumes at it. A cut page has `truncated: true`; the field is always present.

### `cross_agent_cancel`
In: `{worker_id, expected_run_id?, expected_authority_epoch?, idempotency_key}`.
Out: `MutationReceipt` + `{worker_id, run_id|null, state: "stopping"|"idle"|"stopped"}`.
- **Bound at acceptance.** The operation records a **bound set**: the target's current run, if any, plus every
  descendant run active at that moment. Dispatch interrupts only those runs.
  - A bound run that has already ended is a no-op.
  - Cancel never targets a run that started after acceptance.
  - An idle target with active descendants has a non-empty bound set. The cancel is dispatched to the descendants, and
    the output's `run_id` is null.
  - Only an **empty** bound set gives `state: "idle"` and no dispatch.
- `expected_run_id`, when given, must equal the target's current run, else `stale_run`. Models should pass it.
- **Completion.** The cancel operation is `confirmed` when every bound run is terminal: the target run on its
  interrupted or terminal result, and each descendant run on a terminal result or an observed process exit.
  `state` reports `stopping` until then. A worker whose process is still alive after the interrupt goes to `waiting`;
  only an observed exit makes it `stopped`.

### `cross_agent_close`
In: `{worker_id, expected_authority_epoch?, idempotency_key}`.
Out: `MutationReceipt` + `{worker_id, state: "closing"|"closed"}`.
- Closing is permanent: it revokes the subtree's capabilities, invalidates queued operations and keeps the transcript.
- An ancestor or the caller itself: `authority_denied`.

### Error codes (`ErrorCode`, snake_case; an unknown code from a newer build decodes as `unknown`)

25 codes, plus `unknown` (`ErrorCode::ALL` has 26 entries). Revision 5 adds `internal_error`.
Revision 3 added no code. Misdelivery is a delivery state
(`misdelivered`, §2), not an error.

| Code | Retryable | Meaning |
| --- | --- | --- |
| `runtime_unavailable` | no | not launched by a running agent-terminal, welcome deadline expired, rebind required (§6), or the app is shutting down |
| `authentication_failed` | no | hello refused (token, instance, peer, single-use already consumed, agy hello window elapsed) |
| `workspace_untrusted` | no | agy launch failed §7 L2 (workspace source, argv/env redirection, home mismatch, missing allow rules); delegation withheld |
| `protocol_incompatible` | no | no common `api_version` |
| `invalid_argument` | no | schema or bounds failure, cursor filter mismatch, ACK not presented; `details.field` / `details.reason` |
| `not_found` | no | handle unknown or not visible to the caller |
| `provider_unavailable` | no | binary/auth missing, unrecognised model (Claude `unrecognized_model`, Codex failed turn), child read-back mismatch (§5) |
| `route_unverified` | no | pair not proven, or required enforcement (`--restricted`, proven hook, Codex MCP isolation) unavailable |
| `unsupported_delivery` / `unsupported_effort` / `unsupported_workspace` | no | combination not supported (e.g. `this_run` where `delivery.this_run` is `unsupported`) |
| `authority_denied` | no | outside ceiling, depth, scope, ancestor close, a restricted target, or an unattested invocation (`details.reason: "unattested"`, §4) |
| `approval_required` | no | operator must act in the UI first |
| `quota_exhausted` | no | provider quota |
| `capacity_reached` | yes | a §9 limit; `details.limit` |
| `idempotency_conflict` | no | same key, different request |
| `stale_epoch` / `stale_run` | no | fence failed; `details.operation_id` / `details.run_id` |
| `cursor_expired` | no | stream epoch changed |
| `worker_closed` | no | target closed |
| `deadline_exceeded` | yes (same key) | supervisor did not answer in time; outcome may still land |
| `persistence_failed` | yes (same key) | durable write failed; nothing dispatched |
| `delivery_unknown` | **no** | dispatch reserved, outcome unknown; also every replay of such an operation; never auto-retried |
| `transport_lost` | yes (same key) | IPC dropped mid-call |
| `internal_error` | no | a server-side bug, such as the final output guard (§3 "Output budget"). It never alters an accepted operation, which a same-key call still replays (§2) |

## 4. IPC: bridge ↔ supervisor

- **Socket.** `$XDG_RUNTIME_DIR/agent-terminal/delegation-<app-pid>.sock`.
  - One per app instance. The directory is `0700` (shared with the approval sockets) and the socket is `0600`; it is
    removed on shutdown.
  - No TCP, and no fallback. Without `$XDG_RUNTIME_DIR`, delegation is unavailable.
- **Frame.** A `u32` big-endian length N, with `1 ≤ N ≤ 1_048_576`, followed by N bytes of UTF-8 JSON (one object).
  - If N is out of range, the connection is closed before any allocation or parse.
  - The decoder buffers at most one frame. The server reads more bytes only when no complete frame is pending, so
    decoded calls never queue up without bound.
- **Transport bounds** (all close the connection on violation; numbers in §9):
  - Unauthenticated connections are counted separately: at most 8 pre-auth, inside the 32 total. Past either cap, a new
    connection is closed at once.
  - Hello must complete within 5 s of accept. Once a frame header is read, its body must arrive within 5 s.
  - Each frame write must complete within 5 s. Each connection's outbound queue holds at most 16 frames or 4 MiB.
    Overflow, or a missed write deadline, closes the connection. Its pending wait calls are released, and mutations
    already accepted carry on and stay visible through `read`.
  - In-flight admission happens when a `call` is decoded. A 5th concurrent call gets an immediate
    `capacity_reached` result and is never queued.
  - These close the connection as protocol violations: a `call` whose `id` equals a pending call's `id` on that
    connection; a second `hello` on an authenticated connection; an unknown frame tag; and any frame before `hello`.
- **Client deadlines** (bridge side): connect, hello and welcome within 2 s of process start, or degraded mode (§1).
- **Reconnect.** After losing an authenticated connection, the bridge fails its pending calls with `transport_lost`.
  It then tries exactly one reconnect with the same hello.
  - The reconnect gets its own **fresh 2 s deadline, counted from the moment the loss is detected**, covering connect,
    hello and welcome.
  - If it is refused or the deadline expires, the bridge is degraded until it exits, and the supervisor marks the
    principal `rebind_required` (§6).
  - While the reconnect is pending, new calls wait for it within their own timeouts.
  - Claude and Codex: a token may be reused **sequentially** within its generation, so the reconnect succeeds unless
    the token was revoked.
  - agy: the token is single-use, so the reconnect is always refused (§7 L4). The principal becomes `rebind_required`
    (§6).
- **Frames.** Each is tagged with `"t"`:

```jsonc
// bridge → app, first frame, sent at bridge process start (before serving tools/list)
{"t":"hello","api":{"min":1,"max":1},"instance":"<32 hex>","cap":"atdc_<43 base64url>",
 "bridge":{"version":"3.6.0","pid":1234}}
// app → bridge
{"t":"welcome","api":1,"connection":"cn_…","principal":{"worker_id":"w_…","root_id":"w_…","driver":"agy",
 "depth":0,"can_spawn":true,"task_mode_ceiling":["ask","plan"]},"authority_epoch":4,
 "limits":{"max_frame":1048576,"max_inflight":4}}
{"t":"refused","error":ToolError}                 // then close
// bridge → app
{"t":"call","id":7,"tool":"cross_agent_spawn","args":{…},"timeout_ms":30000,
 "invocation":{"provider":"claude","tool_use_id":"toolu_…"}            // or null
}
{"t":"cancel","id":7}
// app → bridge
{"t":"result","id":7,"ok":{…tool output…}}  |  {"t":"result","id":7,"err":ToolError}
{"t":"revoked","error":ToolError}                 // principal revoked; then close
```

The `welcome.principal.task_mode_ceiling` is informational. The supervisor recomputes the ceiling on every call.

- **Invocation keys.** The bridge copies only these `_meta` keys and drops all other `_meta`, including Codex
  `x-codex-turn-metadata.workspaces`, which carries the git remote and HEAD:
  - Claude: `{"provider":"claude","tool_use_id": _meta["claudecode/toolUseId"]}`.
  - Codex: `{"provider":"codex","thread_id": _meta.threadId,"call_id": _meta.callId}`.
  - agy: `{"provider":"agy","conversation_id": _meta["antigravity.google/conversation_id"],"step_index": <n from progressToken "<uuid>:<n>">}`.
- **Attestation.** The supervisor keeps, per principal and process generation, a bounded set of **observed native
  calls**: the calls to server `agent_terminal` seen in that worker's own provider stream. Each holds the key, the tool
  and the salted digest of `attestation_arguments` (§2). That helper is the same one applied to the bridge call. It
  includes the idempotency key and the epoch, and it is never the scrubbed canonical request. Sources:
  - Claude: an `assistant` `tool_use` with `id` and `input`.
  - Codex: an `mcpToolCall` item with `id` = `callId`.
  - agy: a `call_mcp_tool` step's `tool_info.parameters`, keyed by `(conversation_id, step_index)`.

  A call is **attested** when its key matches an unconsumed observed call with the same tool and argument digest. The
  observed call is then consumed, so each attests at most one call. The supervisor waits up to 2 s for the observation
  to arrive.
  - **Linking.** A call is linked to a native card only if it is attested. Otherwise it stays unlinked, and that is
    never an error by itself. This replaces "the conversation id matches": Claude keys carry no session, so only an id
    observed in that worker's own generation counts.
  - **Authority.** Attestation never grants authority; it can only deny. For an agy principal, **every** call to any of
    the 8 tools must be attested, including `capabilities`, `read`, `list` and `wait`. An unattested call gets
    `authority_denied` with `details.reason: "unattested"`, and returns no data.
    - So injected calls can neither change nor read delegation state.
    - Each model call to our tools is a `call_mcp_tool` step, so a legitimate session loses nothing.
    - If a native sub-agent's call reaches the root's bridge but its step is not in the root's stream, it is refused.
      That fails closed, and it is stated as a limit, not a bug.
  - For Claude and Codex principals, an unattested call is logged and allowed, until that provider's gate passes
    (G-C5, G-X5: the native call is observable within 2 s of the `tools/call`, with arguments). From then on, all 8
    tools need attestation, flipped in the same reviewed commit as the route table.
  - **Tests (P1):** the same call attests once. Each of these fails to attest:
    - another tool;
    - a changed or added idempotency key;
    - a removed or changed `expected_authority_epoch`;
    - text that differs but scrubs identically.

    These still attest: an omitted default versus an explicit one, a different key order, and an argument object
    supplied as a JSON string.
- **Timeouts.** The bridge's `timeout_ms` is `wait.timeout_ms + 5000` for `wait`, otherwise 30000. On expiry the bridge
  returns `deadline_exceeded`. If the IPC drops, every pending call returns `transport_lost`. Codex runs with
  `tool_timeout_sec = 60`, which is above both.
- **Statelessness.** The bridge holds the connection and its pending call ids, nothing else. Claude respawns a dead
  bridge in the same process (claude.md D7). Codex and agy keep it dead until resume or relaunch (codex.md surprise 7,
  agy.md D7). Reconnect and rebind are defined above and in §6.

## 5. Per-provider launch mapping

### Environment variables

| Name | Secret | Content |
| --- | --- | --- |
| `AGENT_TERMINAL_DELEGATION_ENDPOINT` | no | absolute socket path |
| `AGENT_TERMINAL_DELEGATION_INSTANCE` | no | 128-bit app-instance nonce, 32 hex |
| `AGENT_TERMINAL_DELEGATION_CAP` | **yes** | `atdc_` + base64url(32 random bytes), no padding; one per principal and process generation |
| `AGENT_TERMINAL_DELEGATION_APP_PID` | no | the app's pid, decimal; for the bridge's lineage self-check (§1) |

- Every spawn first strips inherited `AGENT_TERMINAL_DELEGATION_*` and `AGENT_TERMINAL_APPROVAL_SOCKET`, then installs
  its own values.
- `atdc_` is added to `redact` `TOKEN_PREFIXES`. Token comparison runs in constant time.
- **Process generation** means: for Claude, one OS process; for Codex, one `thread/start` or `thread/resume` (each
  spawns a new MCP process); for agy, one OS process.

### Token exposure matrix

The token never appears in argv, in a persistent config file, in an app log, in an `Envelope.raw` or in the store.
What it does reach, per provider:

| Surface | Claude | Codex | agy |
| --- | --- | --- | --- |
| argv | never | never | never |
| Files | one ephemeral overlay file (rules below) | none: it travels in the `thread/start`/`resume` request over the app-server's stdin pipe | none |
| Provider process env | no (only the bridge's server `env`) | no (only the MCP child's config `env`; never `env_vars`, which forwards a variable that already exists in app-server's env, and would need us to put one there) | **yes**, the only channel (§7) |
| Model shell commands | no | no | yes. Spent after the bridge's hello; revoked if unused after the hello window (§7 L4) |
| Other MCP servers and hooks | no (per-server env; children use `--strict-mcp-config`) | no (per-server env) | yes: global and home-plugin servers are user-trusted; workspace ones are withheld by §7 L2 |
| Provider transcripts, logs, caches | **G-C3** | **G-X3** | **G-A3** |

- **G-C3, G-X3, G-A3 are route blockers** for every route in which that provider holds a token. Each run starts a parent
  and a child, and resumes once. After that, the run's exact token must be absent from:
  - the provider's whole home: `CLAUDE_CONFIG_DIR` or `~/.claude`, `CODEX_HOME`, or `~/.gemini`;
  - the workspace and `$TMPDIR`;
  - every file the process opened for writing (strace).

  Codex also runs with the user's effective log settings. A transcript-only search is not enough.
- P1 negative tests cover our own surfaces: `OpenSession` serialisation, `DelegationBinding` Debug, the IPC `Hello`
  Debug, and Codex start and resume config builders that put the token only under `env`.

**Claude overlay file.**
- The directory is `$XDG_RUNTIME_DIR/agent-terminal`. It must be a real directory: `lstat` shows not a symlink, owned by
  our uid, mode `0700`. Otherwise Claude delegation is unavailable. There is no `/tmp` fallback.
- The file is `mcp-<app-pid>-<worker>-<gen>.json`, created `O_WRONLY|O_CREAT|O_EXCL|O_NOFOLLOW|O_CLOEXEC` with mode `0600`.
  `fstat` checks its owner and mode before the write.
- It is unlinked after the generation's observed exit. At startup, the app removes every `mcp-*.json` there whose app pid
  is not a live agent-terminal.
- `$XDG_RUNTIME_DIR` is tmpfs, so the file never reaches a disk.
- If G-C4 proves that Claude reads the overlay only at startup, the file is unlinked after `system/init` instead.

### Mapping table

| | Claude | Codex | agy |
| --- | --- | --- | --- |
| Parent binding | `--mcp-config <overlay file>` (no `--strict-mcp-config` for roots) | `thread/start` **and every** `thread/resume` `config`: `mcp_servers.agent_terminal.{command,args,env,default_tools_approval_mode="approve",tool_timeout_sec=60}` | persistent managed entry in `$HOME/.gemini/config/mcp_config.json` (Settings, plan §8.5–6); token in agy's process env, only if §7 L2 passes |
| Provider gate on our 8 tools | `can_use_tool` with `mcp_server.name=="agent_terminal"` and `source:"dynamic"` → host auto-allows; supervisor is the gate | `approve` mode (calls fail under `never` without it) | hookless: 8 explicit `permissions.allow` rules `mcp(agent_terminal/<tool>)` in `~/.gemini/antigravity-cli/settings.json`, written by Settings with the entry and re-checked at each launch (§7 L2); hook path: app hook recognises server `agent_terminal` and allows (needs the `call_mcp_tool` payload fixture, G-A1) |
| Bridge restart | respawned by Claude in-process (sequential re-hello with the same token) | dead until `thread/resume` (fresh overlay and token) | dead until relaunch (fresh token) |
| Child launch | `--restricted --strict-mcp-config --permission-prompt-tool stdio --permission-mode default --mcp-config <overlay> --tools <list>` (list below); refuse `ask`/`accept_edits` if `--restricted` is unsupported | `thread/start {sandbox: read-only (plan) / workspace-write (ask, accept_edits), approvalPolicy per existing mode_policy}` + overlay + MCP isolation (below) | proven hook in the child's actual worktree **for every task_mode** (hookless agy applies edits in all modes); `--mode` per task_mode; token env only if §7 L2 passes, else the child runs without bridge tools |
| Child read-back (fail closed: stop the child before its first turn, `provider_unavailable`) | `system/init.tools` equals the `--tools` list plus `mcp__agent_terminal__*`, and `mcp_servers` is exactly `[agent_terminal]` | `sandbox`/`approvalPolicy` read back as requested; `mcpServer/startupStatus/updated` names **only** `agent_terminal` for that thread (any other name, including the built-in `codex_apps`, at any time → interrupt and stop) | hook proof for that worktree passes at that start (PR #44: fresh proof per start) |
| Child MCP isolation | `--strict-mcp-config` (proven, claude.md D1) | **G-X1, route blocker.** First mechanism: per-thread `mcp_servers.<name>.enabled=false` for every server name in the child's effective config and every built-in name seen in fixtures (`codex_apps`). Fallback (allowed by user decision 2026-10-15, **children only**, never roots): a private per-child `CODEX_HOME` under the app's data directory (`0700`), holding only a generated `config.toml` plus a symlinked `auth.json` (the codex.md D2 recipe; never copied or read). Its `config.toml` carries only the profile's model/provider settings; the user's servers, profiles and global `AGENTS.md` are absent, and Settings says so. The home is kept for the worker's life (resume needs it) and removed on close. Either way the read-back above decides | app hook denies every `call_mcp_tool` to a server other than `agent_terminal` (G-A1). Ambient servers still start and receive no token (children get none unless L2 passes, and then the same user-trusted rule applies) |
| Child native delegation | excluded: `--restricted` drops `Task`/`Agent` unless named (d6_restricted init `tools`), and the read-back checks it | **G-X2, route blocker:** establish whether Codex 0.161 exposes a native sub-agent or multi-agent tool to the child, and disable it by config with a read-back, or prove there is none | app hook denies `invoke_subagent`, `define_subagent`, `manage_subagents`, `browser_subagent` (tool names from agy fixtures; G-A1) |
| Plan enforcement (child) | tool list without write/exec tools (not Claude plan mode, which switched model and wrote `~/.claude/plans`) | kernel sandbox `read-only` | app hook (plan denies mutating tools); supervisor ceiling on bridge calls |
| `this_run` delivery | **`best_effort`** (user decision 2026-10-15). The receipt is `submitted`, with `target_run_id` = the live run; the line is written only if that run is still live at dispatch. The record then becomes one of: `provider_confirmed`, if the `--replay-user-messages` `isReplay` echo appears in the targeted run's turn; `misdelivered`, with `recipient_run_id` = the actual run, if it appears in a later turn (the turn-end race, claude.md D5); `unknown` if no echo arrives before exit. Correlation: FIFO order of written lines plus the exact scrubbed text; a mismatch makes the delivery `unknown`. By the echo's `uuid` once G-C2 proves an input `uuid` is echoed back. `auto` is the same without a target | `strict`: `turn/steer {expectedTurnId}` → `submitted`; `userMessage` `item/completed` → `provider_confirmed`; `-32600` → `stale_run` | `unsupported` (`unsupported_delivery`) |
| `next_turn` / idle | user line after `result`; respawn uses `--resume` + fresh overlay | `turn/start` on same thread; on refused steer under `auto`, record `queued_next_turn` then `turn/start` after `turn/completed` | write only after a `result` frame, never pre-write (a denied turn plus a queued message produces one merged result); count turns by `user_input` steps/`num_turns`; SIGINT drops queued stdin, so resend from the mailbox |
| Cancel | `control_request interrupt` → run terminal on `aborted_tools` result; process alive → worker `waiting` | `turn/interrupt` → run terminal on `turn/completed interrupted`; release dangling MCP waits | SIGINT → terminal on observed exit |
| Correlation key | `_meta["claudecode/toolUseId"]` = stream `tool_use.id` | `(_meta.threadId, _meta.callId)` = `mcpToolCall` item id | `(conversation_id, step_index)` from `_meta`/`progressToken` = `step_update.step_index` |
| Model provenance | `provider_reported` from `assistant.message.model` **per turn** (may change between turns); init echo is not evidence | `launch_configured` → `provider_accepted` (effective null) after a `completed` turn | `launch_configured` (or `unknown` without `--model`) → `provider_accepted` (effective null) after a successful result |
| Effort provenance | `launch_configured` forever (bogus is silently accepted); validated pre-launch against `supportedEffortLevels` | `launch_configured` → `provider_accepted` (effective null) after a `completed` turn (bogus fails the turn) | `launch_configured` forever (whether it is honoured is unverified) |

Claude child `--tools` lists (constants in `agent_core::delegation::launch`):
- `plan`: `Read,Glob,Grep`.
- `ask` and `accept_edits`: the same plus `Edit,Write,NotebookEdit,Bash`. **Proof gate G-C1 (P5):** a Bash call under
  `--restricted` must raise `can_use_tool`. If the fixture shows otherwise, remove `Bash`; the list is a launch
  constant, not part of the wire contract.
- Never: `Task`/`Agent`, `WebFetch`, `WebSearch`.
- `permission_suggestions` are never applied.

## 6. Authority

- **Mode ceiling.** The ceiling is computed **on every call** from the principal's current effective mode, after
  `Mode::session_safe` (Bypass never propagates):

  | Principal's mode | Allowed child `task_mode` (spawn) and recipients (any parent→child send) |
  | --- | --- |
  | `AcceptEdits` | `accept_edits`, `ask`, `plan` |
  | `Ask` (and `Bypass`→`Ask`) | `ask`, `plan` |
  | `Plan` | `plan` only |

  A send of any kind (`instruction` or `reply`) to a child whose `task_mode` is outside the sender's ceiling is
  `authority_denied`. Every restriction bumps the epoch, and E7 rechecks the epoch at dispatch, so a delivery accepted
  before a restriction is never dispatched after it.
- **Depth.** The root is depth 0 and the maximum is 2. A worker at depth 2 has `can_spawn: false`.
- **Spawning.** Spawn itself needs no human card: inside these limits, the child's edits still ask under the inherited
  Ask. A Plan root cannot spawn writers.
- **Scope of the depth and mode guarantee: supervised delegation.**
  - Depth and ceiling bind every agent started through `cross_agent_spawn`.
  - In a supervised child, the provider's own sub-agent tools are disabled, and the child sees no MCP server but ours.
    The child-launch rows in §5 show how: Claude by `--restricted`, `--tools` and `--strict-mcp-config`; Codex by G-X1
    and G-X2; agy by G-A1. A route whose mechanism is unproven stays blocked.
  - A **root** (the user's own session) keeps its provider's native sub-agents and its own MCP servers. Those run
    under the user's authority. agent-terminal shows them as `origin: native` but does not limit them. This is the
    user's decision of 2026-10-15.
    - A native sub-agent never gets delegation, and it never disturbs the root. If it runs as a separate process
      (unverified for agy), any bridge it starts fails the §1 lineage self-check.
      - For **agy**, that bridge then never says hello.
      - For **Claude and Codex**, the self-check is log-only until P5 fixtures confirm the parent chain (§1). Such a
        bridge may still say hello. The peer check is advisory for them (§7 L3), and Claude and Codex tokens may be
        reused sequentially, so the hello may succeed under the root's principal. Its calls then pass E2/E3 like the
        root's own, and become attestation-gated once G-C5/G-X5 pass.
      - Claude children cannot have native sub-agents at all (§5), and roots are not limited, by decision 1.
    - If such a bridge says hello anyway, the supervisor refuses it with a notice. The token is not consumed, and
      nothing is revoked (§7 L3).
    - Its calls through the root's own bridge are refused for agy unless they are attested in the root's stream (§4).
  - UI text, shown in Settings → Agents and as `capabilities.delegation_scope`'s explanation: "Depth and mode limits
    apply to agents started through agent-terminal. Delegated agents cannot start their own built-in sub-agents. Your
    own sessions keep their built-in sub-agents, which agent-terminal shows but does not limit."
- **Enforcement points.** Every point fails closed.
  - **E1, IPC accept:** the §4 transport bounds: directory and socket permissions, the pre-auth cap, the frame bound and
    the hello deadline. Then the instance nonce and the constant-time token check. One live connection per token: a
    second concurrent hello is refused. For agy, the §7 L3/L4 checks.
  - **E2, every call:** the principal comes from the connection, never from arguments. Tool and argument validation,
    limits, epoch fence, idempotency (§2 order) and attestation (§4).
  - **E3, every call:** the live mode ceiling and depth are applied by the supervisor, because provider plan modes do
    not gate MCP calls (agy.md D6).
  - **E4, spawn:**
    - The profile must be enabled for delegation, and the route must be admitted (`verified`) for this `task_mode` and
      child-bridge variant (§11).
    - Workspace lease: source containment and physical worktree containment, symlink resolution, sensitive files.
    - The child launch mapping above, including its required enforcement (`--restricted`, a proven agy hook, a Codex
      sandbox read-back) and every child read-back row.
  - **E5, child approvals:** `InheritedAuthority` is applied before any Always or session rule. A human card appears in
    the child's scope.
  - **E6, a child's own bridge calls:** handled by the provider gate rows above. Authority is checked again by E2/E3
    under the child's own principal.
  - **E7, dispatch:** epoch, closing and the live ceiling are rechecked on the GIO context, with no yield between the
    check and the write (plan §5 step 3). This covers every execution-driving delivery: spawn, every parent→child send
    and every automatic continuation.
  - **E8, revocation:** below.

### Revocation

Each root tree has one **authority epoch** (`delegation_roots.authority_epoch`). Every revocation event bumps it once
and records its **scope**: `root` (the whole tree) or `subtree(w)`.

On every bump, all of the following happen:
- Every not-yet-reserved operation in the tree is invalidated: failed with `stale_epoch`. Queued sends become
  `cancelled`.
- Pending approvals and automatic-continuation reservations in the scope are invalidated.
- Child→parent mailbox entries are data. They stay readable.

Then, by event:

| Event | Scope | Credentials that die (connections get `revoked`, then close) | Runs and processes |
| --- | --- | --- | --- |
| Root Stop (user) | root | every descendant's token, since its process generation ends; **not** the root's | every descendant run is interrupted and every descendant process is stopped; the root's run is interrupted; root task `stopped` until the user's next turn |
| Root restriction (user narrows the root's mode) | root | the tokens of descendants whose `task_mode` is outside the new ceiling (their processes are stopped); **not** the root's | out-of-ceiling descendants are interrupted and stopped, marked `restricted`, and refuse sends (`authority_denied`); they stay readable. In-ceiling descendants keep running. **The root keeps working: its token stays valid, and E3 applies the narrower ceiling to every later call** |
| Root widening | none (no bump) | none | existing children are never widened; that needs a new spawn |
| Close `w` | `subtree(w)` | `w` and every descendant | all interrupted and stopped; closed permanently |
| Observed exit of a worker's process generation | `subtree(w)` | that generation's token only | descendants keep running and report into the mailbox; the worker's next generation gets a fresh token for the **same** principal |
| App shutdown | all roots | all | all stopped; `revoked` carries `runtime_unavailable` |

Pending calls on a revoked connection return the `revoked` error (`authority_denied` for restriction or close,
`runtime_unavailable` for shutdown). Their operations go on according to their own state.

**Delegation status** (`WorkerSummary.delegation`, stored as `delegation_workers.delegation_status`; the same 6 codes
in the store, on the wire and in P1):

| Code | Set by | Leaves it |
| --- | --- | --- |
| `available` | a successful hello | any other event below |
| `workspace_untrusted` | §7 L2, the post-start recheck, census or second-hello compromise; `paths` lists the offenders | the next launch whose L2 passes |
| `rebind_required` | an agy IPC loss, a failed reconnect, an E1 connection kill, or an unused agy token after the hello window | an automatic rebind (below) |
| `restricted` | a root restriction left the worker outside the new ceiling | **never**: no automatic rebind, and sends are refused; it stays readable and closable, and work goes to a new spawn |
| `bridge_disabled` | an agy child launched without a token (§3 spawn) | never, for that worker |
| `unavailable` | no endpoint, degraded bridge, app shutting down; `reason` says which | the next successful hello |

A **closed** worker has state `closed`. It never rebinds, and its last status is kept for display.

**Rebind (fresh token for a live worker whose credential died).** It applies only to `rebind_required` workers, and to
`available` workers that were stopped by Stop or a process exit and are sent work again. It never applies to
`restricted`, `bridge_disabled` or closed workers. The rebind always happens at an idle point, after a `result` or a
completed turn, never mid-turn:
- **Claude:** relaunch with `--resume <session>` and a fresh overlay (new generation, new token).
- **Codex:** `thread/resume` with a fresh config overlay. The same app-server stays up, and a new MCP process starts.
- **agy:** relaunch with `--conversation <id>`. §7 L2 runs again first, and the new process gets a fresh single-use
  token.

Children rebind automatically before their next dispatched turn. A root rebinds automatically before the next turn the
user starts. Until then its bridge is degraded, and the thread banner says "Delegation disconnected; it reconnects on
your next message".

## 7. agy capability-leak mitigation (frozen)

**Problem.** agy passes its full environment to every MCP server and hook it starts. That includes servers a repository
declares in `$W/.agents/mcp_config.json` or `$W/.agents/plugins/*/mcp_config.json`, with no trust prompt (agy.md D1). It
also includes the model's shell commands. No per-launch override exists (agy.md D2), so the token can only travel
through agy's environment.

| Option | Verdict |
| --- | --- |
| L1: per-principal bearer only | rejected: a repo server harvests it |
| L2: withhold delegation unless the launch's config sources are known-clean | **adopted: the primary and only preventive control** |
| L3: lineage self-check (§1), peer check at hello, plus child census | **adopted: detection, defence in depth** |
| L4: single-use token per agy process generation, plus hello window | **adopted: limits reuse, defence in depth** |
| L5: inherited fd / socketpair channel | not viable: Go `os/exec` passes only stdio and `ExtraFiles`, so the fd never reaches the server |
| L6: no agy parent | contradicts the user's decision (2026-10-15) |
| Attestation (§4) | **adopted, mandatory for agy principals on all 8 tools**: limits what a misused token can do or read |

**L2: the launch must be clean.** It runs immediately before every agy (re)launch, root or child, on a worker thread.
Its inputs are the **final** `SpawnSpec` (argv after the adapter, the environment after stripping and the profile's env
file) and the physical working directory. It withholds delegation if any of these holds:

1. **argv redirection.** The final argv still contains a configuration-directory flag: `gemini_dir` or `app_data_dir`,
   with one or two dashes, `-` and `_` treated alike, inline `=` or the next argument (the PR #44 classification). The
   adapter already drops them for every launch, so this is defence in depth against a future regression.
2. **env redirection.** The environment sets any of `CONFIG_REDIRECT_VARS` (`ANTIGRAVITY_APP_DATA_DIR`,
   `JETSKI_APP_DATA_DIR`, `CASCADE_GLOBAL_CONFIG_OVERRIDE`), even to blank, or **any** name starting `CASCADE_`
   (covering `CASCADE_MCP_SERVER_INIT` and `CASCADE_ENABLE_MCP_TOOLS`, whose effect is unverified).
3. **Home mismatch.** The environment's `HOME`, canonicalised, is not the home whose managed entry and allow rules
   Settings verified.
4. **Allow rules (hookless only).** `~/.gemini/antigravity-cli/settings.json` cannot be parsed, or its
   `permissions.allow` lacks any of the 8 exact `mcp(agent_terminal/<tool>)` strings. Without them agy silently denies
   the tools, so the token would only be exposure.
5. **Workspace sources.** The searched directories are:
   - inside a git repository (a successful `agent_kit::git::discover`): the physical cwd and its ancestors up to the
     toplevel, i.e. the worktree root;
   - in a **non-git** tree: the physical cwd and **every ancestor up to `/`**, because how far agy searches there is
     unproven;
   - if discovery fails or times out: the launch is withheld.

   In each searched directory `d`, it withholds if:
   - `d/hooks/hooks.json` exists;
   - `d/.agents` exists and a **recursive scan** of it finds, at any depth, an entry with one of the
     **`WORKSPACE_SOURCE_NAMES`**: `mcp_config.json`, `hooks.json`, `sidecars.json`, `sidecars`, `plugin.json`,
     `plugins.json` or `plugins.txt`.
     - Each of these names is a file agy opened or probed in the workspace (agy strace,
       `tests/fixtures/real/delegation/agy/d2-config-probes.txt`).
     - `sidecars` may start processes. A plugin manifest or plugin list may declare servers, hooks or sidecars; what
       those contain is unverified, so each one is treated as a source.
     - Text sources (rules, skills, README) are not sources.
     - The scan follows symlinked directories, canonicalised, and detects cycles by `(dev, ino)`. It is limited to
       depth 8 and 4096 entries.
   - It also withholds on any unreadable or unlistable entry, a dangling link, an entry whose type cannot be read, or a
     limit exceeded.

When L2 withholds:
- no `AGENT_TERMINAL_DELEGATION_*` variables are installed;
- the worker's delegation status is recorded as `workspace_untrusted`, with the reason and the offending relative paths
  (never values);
- Settings and the thread banner say that delegation is unavailable in this workspace, and why.

For a child, L2 runs on its created worktree, whose files come from the base commit (agy.md D6). L2 runs again at every
relaunch, since the agent may have written such a file during the session. **Post-start recheck:** L2 also runs again
when the generation's hello arrives, and before its first call is admitted. Any change revokes the generation:
agy is stopped, and the status becomes `workspace_untrusted`.

**L4: single use.**
- Each agy process generation gets a fresh token. The first accepted hello consumes it, and any later hello with it gets
  `authentication_failed`.
- The bridge sends hello at process start. agy starts servers eagerly (`agy models` started them) and never restarts one
  inside a process (agy.md D7).
- **Hello window.** A token not consumed within 30 s of agy's spawn is revoked. The generation becomes
  `rebind_required`.
- **A hello that fails L3 is refused and consumes nothing.** The peer may not be a direct child of this agy, or not a
  bridge (below). Typical causes are a nested agent's process or a stray copy. The token stays valid for the legitimate
  bridge, and nothing is revoked. The supervisor logs the peer's pid and its argv (first 256 B, scrubbed) at warning
  level, and adds a notice to the thread: "a nested process tried to use this thread's delegation and was refused".
  Notices are **coalesced per peer pid**:
  - one notice per pid per process generation, with a refusal count updated in place;
  - at most one new notice per thread per 60 s, with later distinct pids folded into the next one;
  - log lines are rate-limited the same way, with a periodic suppressed-count line.
  This keeps native sub-agents of a root from killing the root.
- **Second hello that passes L3.**
  - From the **same** peer pid after its connection closed: a lost IPC. It is refused, and the generation becomes
    `rebind_required` (§6).
  - From a **different** pid that is itself a direct child of this agy running the bridge: a compromise signal. Two
    bridge processes under one agy means a workspace entry launched one. The generation is revoked, agy is stopped,
    and the status becomes `workspace_untrusted`, with the reason "a second bridge under this thread presented its
    delegation capability". The log records both pids and argv.
- **Correction to revision 1:** a shell command that reads the variable after the hello finds it spent, but it does not
  need the token. See the residual risk below.

**L3: peer and census.**
- At hello, take the peer pid from `SO_PEERCRED` (GIO `Socket::credentials`). The peer must satisfy both:
  - its `/proc/<pid>/stat` ppid equals the agy pid the app spawned for this principal. `stat` is parsed after the
    **last** `)`, because `comm` may contain spaces and parentheses: the ppid is the second field after it.
  - its `/proc/<pid>/exe` stats to the same `(dev, ino)` as the managed bridge command;
  - its `/proc/<pid>/cmdline` is a **bridge**: the NUL-separated argv after argv[0] is exactly `--cross-agent-mcp` (a trailing NUL is optional; an empty trailing field is ignored).
    The executable alone is not enough. The app's own approval hook is also a direct child of agy with the same
    `(dev, ino)`, because `bash -c '… exec agent-terminal --approval-hook'` keeps the pid. Its argv is
    `--approval-hook`, so it is never counted as a bridge.
- **Census** at hello and before the first call: exactly one live process may have ppid = agy pid, exe = the bridge's
  `(dev, ino)` and a bridge argv, and it must be the connected peer. Approval-hook processes and every other argv are
  ignored. Two bridges is the compromise case of a second hello. A process that vanishes between the scan and the read
  of its `cmdline` is skipped, since an exited process holds no connection.
- For Claude and Codex, the peer check is advisory only: record it and log any mismatch, until P5/P8 fixtures prove the
  parent relation.

**What L3 and L4 do not do.** A repository entry that L2 missed (a race, below) can launch the genuine
`agent-terminal --cross-agent-mcp`. That process has the right exe and the right parent. If it says hello first, it
passes L3 and consumes the token. L3, L4 and the census only **detect** this: the legitimate bridge's hello then becomes
a second hello from a different pid. They cannot prevent it. Attestation **limits** it: on an agy principal, every
unattested call is refused, reads included. So the impostor can submit only a call that agy's own model made to
`agent_terminal`, with exactly those arguments, including its idempotency key and epoch fence (§2 attestation
arguments). It then receives that call's result, which the model would have received anyway.

**Residual risk (stated in Settings, plainly):**
- **L2 is the only control that keeps a hostile server from receiving the token.**
- **Race.** L2 checks the files, then agy reads them. Something that writes a `.agents` MCP or hook file in that
  interval, typically milliseconds to a few seconds, gets the token. A repository cannot do this by itself: the writer
  must be a process already running as the user. The post-start recheck, the second-hello signal and the census detect
  most such cases afterwards, and attestation limits the damage. Nothing closes the window entirely, because agy has no
  launch mechanism that pins the configuration it loads.
- **Same-UID code.** Any process running as the user can:
  - write tool calls into a live bridge's stdin (`/proc/<pid>/fd/0`);
  - write prompts into agy's own stdin;
  - `ptrace` or `LD_PRELOAD` either of them.

  For agy, attestation stops injected bridge calls, reads included, that the model did not make. It cannot stop
  injected prompts.
  agent-terminal does not defend against code that already runs as the user; such code can drive the user's agents
  directly. This holds for every provider.
- **User-trusted sources.** MCP servers and hooks the user installed globally or in home plugins receive the token,
  like any hook.
- The existing `AGENT_TERMINAL_APPROVAL_SOCKET` reaches the same repo servers today. The impact is low: a forged query
  only raises an approval card.

## 8. Store amendments (to plan §5)

1. **Single instance is the real guard; v4 is strictly additive.** An idle v3 `StoreWriter` holds no SQLite lock.
   - v4 only creates new tables and indexes, and adds rows to `meta`.
   - It does not ALTER existing tables, add NOT NULL columns, change CHECK constraints, or add triggers on v3 tables.
   - Hidden child threads are excluded with `NOT EXISTS (SELECT 1 FROM delegation_workers w WHERE w.backing_thread_id = threads.id)`,
     not with a new column.
2. **Preflight never writes before the lock.** The order is:
   1. A missing database file means a fresh install, handled by today's `Store::open`.
   2. Otherwise, `flock(<db>.upgrade-lock)`, then open with `READ_WRITE` and no `CREATE`, and read `sqlite_master` with
      no write. Do not run `CREATE TABLE IF NOT EXISTS schema_version`, and do not switch to WAL.
   3. `foreign_keys=OFF`, then `BEGIN IMMEDIATE`, then recheck the version.
   4. Back up from a **separate** read-only connection with one bounded `Backup::step(-1)` that must return `Done`.
      Never use `run_to_completion`. Switch the destination to `journal_mode=DELETE`, fsync it, then verify:
      `integrity_check`, v3, counts, `0600` in `0700`.
   5. Publish with `hard_link` (no clobber).
   6. Migrate in the same reserved transaction and record `meta('pre_v4_backup')` there.
   7. A failure before commit leaves a valid orphan backup, which is retained.
3. **rusqlite** gains `features = ["bundled","backup"]` in `crates/agent-kit/Cargo.toml` (coordinator).
4. **`durable_call`** switches `synchronous` only outside a transaction, after committing the open batch. It reads the
   value back after each switch, and an error at either switch refuses dispatch.
5. **`delegation_operations`:** `UNIQUE(principal_worker_id, kind, idempotency_key)`. Columns:
   - `authority_epoch`: the epoch it was accepted under;
   - `canonical_request`: the scrubbed, syntax-normalised JSON (§2);
   - `resolution_json`: the target worker, the bound runs and the base commit, fixed at acceptance;
   - `receipt_json`: the original success receipt, written once at acceptance and never updated;
   - `dispatch_state`: a `DispatchState`, updated as facts arrive;
   - `error_json`: the scrubbed `ToolError` of a `failed` operation.

   There is no fingerprint hash. Replay reads `receipt_json` and `dispatch_state` separately (§2 table).
6. **`delegation_messages`** records:
   - `target_run_id`: frozen at acceptance;
   - `delivery_state`: current, including `misdelivered`;
   - `recipient_run_id`: the run in which consumption was observed, set once;
   - `presented_at`: the first page that returned it to its recipient, for ACK eligibility.
7. **`delegation_workers`** records `delegation_status` with the 6 codes of §6 (`available`, `workspace_untrusted`,
   `rebind_required`, `restricted`, `bridge_disabled`, `unavailable`), plus `delegation_reason` (scrubbed, ≤256 B) and
   `delegation_paths` (JSON array of relative paths, ≤16). They are the same enum as the wire and P1.
8. **Never stored:** tokens, the instance nonce, overlay file contents, environment snapshots, the Codex
   `x-codex-turn-metadata`, and the argument digests of observed calls (memory only).

## 9. Limits (v1 defaults; enforced in `agent_core::delegation::limits`)

| Limit | Value |
| --- | --- |
| Active runs | 4 per root, 8 app-wide |
| Live supervised processes | 8 app-wide (an idle process still counts; only an observed exit frees a slot) |
| Depth | 2 |
| Pending starts | 32 per root, 64 app-wide |
| IPC connections | ≤ 32 app-wide, of which ≤ 8 unauthenticated |
| IPC frames | frame ≤ 1 MiB; one buffered inbound frame per connection; ≤ 4 in-flight calls per connection |
| IPC deadlines | hello ≤ 5 s after accept; frame body ≤ 5 s after its header; each write ≤ 5 s |
| IPC outbound queue | ≤ 16 frames and ≤ 4 MiB per connection |
| Bridge | stdio message ≤ 1 MiB; welcome deadline 2 s from process start; one reconnect, with a fresh 2 s deadline from the detected loss |
| Output budget | every tool: `transport_cost(O)` = `len(json)` + `len(escaped json text)` ≤ 768 KiB (`OUTPUT_BUDGET`); JSON-RPC request id ≤ 256 B encoded; `WorkerSummary` ≤ 48 KiB serialised; receipts ≤ 48 KiB (≤ 8 warnings); `ToolError.details` ≤ 64 KiB; read `ack` ≤ 64 KiB; capabilities field caps 32 profiles × 64 models × 8 efforts, then truncated to the budget (§3) |
| agy hello window | 30 s from spawn |
| Attestation | ≤ 256 observed calls per principal generation (oldest dropped first); wait ≤ 2 s |
| Waits | ≤ 64 open waits app-wide, ≤ 8 per principal; timeout default 10 s, max 30 s |
| Task / message / name | 64 KiB / 16 KiB / 80 chars |
| Read/wait page | default 100 units / 64 KiB; max 500 / 256 KiB; snapshot always included and at least one unit when available (§3) |
| `wait.workers` / ACK batch / list page | 32 / 100 / 100 |
| Run deadline | default 15 min (profile setting, max 120 min) |
| Automatic root continuation | 1 reserved at a time, 3 per active root task |
| L2 scan | depth 8, 4096 entries per `.agents` tree |
| Sanitiser | 64 KiB per stream, 4 MiB aggregate, 1024 streams (`redact::stream::Limits::default`) |

## 10. Publication boundary (P2, merged)

- **Key.** Child text passes through `redact::stream::StreamSanitiser` (not generic) before both persistence and the
  sink. The key is the concrete `redact::stream::StreamKey { worker: String, generation: u64, item: String, stream: String }`:
  - `worker` = `WorkerId::as_str()`;
  - `generation` = `ProcessGeneration.0`;
  - `item` = `Envelope.item`;
  - `stream` = `StreamKind` as snake_case.

  Keys are compared by equality. Fields of any length are accepted; the sanitiser stores a bounded encoding of each
  (`MAX_KEY_FIELD_BYTES`).
- **Event mapping:**
  - `ContentDelta`: `push`.
  - `ContentSnapshot`: `replace_snapshot`; the result has `Published.replace == true`, and the consumer replaces the
    item's displayed and stored text with `Published.text`.
  - `ItemCompleted`: `finish`.
  - Interruption or process exit: `cancel` for every open key of that generation.
- **Overflow.** When `Published.overflowed` is set, `WITHHELD_NOTICE` is already in the text. `withheld_bytes` is
  recorded in the worker's activity. A refused or sealed stream is backpressure: pause that worker's intake and surface
  an error. `saturated()` is an error; start a new sanitiser for new work.
- **Approvals and attestation.** Authority decisions and attestation digests use the original trusted inputs, never the
  published text.

## 11. Provider-pair route status (v1)

Every pair below is in v1 scope. At runtime each reports `unverified`, and `spawn` refuses it with `route_unverified`,
until its P5/P8 evidence bundle (plan §10) is recorded and the route table in `agent_core::delegation::route` is flipped
in a reviewed commit.

**Route admission** is decided per `(parent driver, child driver, task_mode, child bridge)`, never per pair alone:
- A route's blockers are the parent group plus the child group, with mode-conditional blockers counted only for the
  modes they name.
- An agy child has two variants: **with bridge**, where it receives a token, and **without bridge**, where it gets no
  token, `bridge_disabled` and no bridge tools.
- `capabilities.profiles[].modes` reports the admission per `task_mode`. The supervisor picks the agy child variant at
  spawn (§3): with bridge if that variant is admitted and L2 passes, else without bridge if that variant is admitted,
  else `route_unverified`.

Blocker groups:
- **Claude parent:** G-C3 (token persistence).
- **Claude child:** G-C3 in every mode; G-C1 (Bash under `--restricted`) only for `ask` and `accept_edits`.
- **Codex parent:** G-X3, plus the adapter change to resend the overlay on every resume.
- **Codex child:** **G-X1** (ambient MCP isolation, read-back), **G-X2** (native delegation), G-X3, the overlay resend
  on resume.
- **agy parent:**
  - the P9 managed entry and allow rules, plus the agent-sync entry (coordinator);
  - **PR #44 merged** (`fix/agy-workspace-plugin-hooks`: workspace plugin hooks refuse the proof, `--gemini_dir` and
    `--app_data_dir` dropped, fresh proof per start);
  - L2 as specified in §7, including the non-git walk to `/`, the recursive scan with all `WORKSPACE_SOURCE_NAMES`, and
    the argv/env/home checks;
  - the §1 lineage self-check, L3 with the argv-aware census, and L4;
  - **G-A5** (attestation: a `call_mcp_tool` step with its arguments is observable within 2 s of the `tools/call`);
  - G-A3.
- **agy child, without bridge:** PR #44 merged, a proven hook in the child's worktree, and **G-A1**: the
  `call_mcp_tool` payload fixture, with the hook denying other servers and the sub-agent tools. The child reports by
  its final result only.
- **agy child, with bridge:** all of the above, plus every control its own credential needs. These are the same
  controls an agy parent needs:
  - the managed entry and allow rules;
  - L2 on the worktree;
  - the §1 lineage self-check, L3 with the census, and L4;
  - **G-A5** (attestation);
  - G-A3.

| Parent ↓ / child → | Claude | agy | Codex |
| --- | --- | --- | --- |
| **Claude** (overlay, D1–D3 proven) | P8 control. Claude parent + Claude child (G-C1 for writer modes only) | P8. Claude parent + agy child (either variant) | **P5 first route.** Claude parent + Codex child. **G-X1 is the first P5 proof gate**, then G-X2, G-X3, G-C3 |
| **Codex** (per-thread config, D1–D3 proven) | P8. Codex parent + Claude child | P8. Codex parent + agy child | P8 control. Codex parent + Codex child |
| **agy** (managed global entry; user decision) | P8. agy parent + Claude child | P8 control. agy parent + agy child | P8. agy parent + Codex child |

If G-X1 fails by both mechanisms (`enabled=false`, then the private per-child `CODEX_HOME` allowed by the user),
Codex children are `unsupported` in v1. The first route then becomes Claude→Claude with `plan` children, which needs
only G-C3. Route admission (above) expresses that without blocking Claude writer children's later admission.

Shared open items (UNVERIFIED, advisory, not blocking a route unless named above):
- `--fallback-model`;
- the Codex Plan `collaborationMode`;
- the agy admin allow-list (`/etc/antigravity/admin_settings.json`);
- whether agy honours `--effort` and the base model id;
- G-C2 (Claude echoes an input `uuid`) and G-C4 (Claude reads the overlay only at startup);
- G-C5 and G-X5 (attestation for Claude and Codex), which become mandatory when proven.

## P4 amendments (2026-10-15, coordinator and user decisions)

These four change the text above. The design is `docs/delegation/p4-supervisor.md` (revision 2). Everything else stands.

**A1. Claude child read-back by an inert bootstrap turn** (replaces "stop the child before its first turn" in the §5
child read-back row, for Claude).
- **Why.** Claude emits `system/init` only after the first user frame
  (`tests/fixtures/real/delegation/claude/d6_restricted/frames.ndjson`, lines 3–4). The `initialize` response carries
  no tools or MCP servers.
- **Bootstrap.** Every Claude child process generation therefore first receives a fixed, harmless bootstrap message,
  which tells it to reply `OK` and use no tool:
  - every `can_use_tool` request in that turn is denied;
  - any `tool_use` in that turn is a **latched violation**: it triggers an interrupt, and the bootstrap can no longer
    pass.
- **Read-back.** `system/init` `tools` and `mcp_servers` are read back as §5 requires. The model is taken from the
  turn's `assistant.message.model`; init's model is diagnostic only.
- **Outcomes.** The task is sent, after the E7 recheck, **only if all** of these hold:
  - the bootstrap turn completed successfully, not interrupted and not failed;
  - no violation was latched;
  - init was received and accepted;
  - the model was recognised.

  Otherwise the child is stopped and the task is never sent:
  - a tool use, an interruption, a failed completion, or a missing or rejected init gives `route_unverified`;
  - an unrecognised model gives `provider_unavailable`.

  Nothing moves from a failed or violated bootstrap to the task.
- **Gates.** G-C1, G-C3 and G-C5 still apply.
- **Cost.** One small turn per Claude child generation, rebinds included. It is shown in the child's transcript as a
  labelled startup check.

**A2. Codex rebind by restart** (replaces "`thread/resume` with a fresh config overlay. The same app-server stays up"
in §6 Rebind).
- A Codex worker rebinds by stopping its app-server, then starting a new one that resumes the thread with
  `thread/resume` and a fresh config and token. This is a new process generation.
- The new generation starts only after the old generation's exit is **observed**. After 5 s it is killed, and its
  exit is still awaited.
- The restart loses Codex's session grants and pending permission requests: those cards expire and are asked again,
  and the thread says so.
- Fail closed: a restart the supervisor did not prepare gets no token, and the worker becomes `rebind_required`.

**A3. Closing a root's page stops its descendants** (a §6 revocation row).
- Closing a root thread's page, or a window holding it, is a **Root Stop**, and every descendant process is stopped.
- The window waits for the observed exits of every descendant process generation. A single tab close does not wait,
  but no process slot or overlay is released before the observed exit.
- Moving a root to another window is a close followed by a reopen. The root principal (its root and root-worker
  rows) is kept.
- The reopened root's first token is issued only after the previous root generation's exit is observed. Two root
  generations never hold live credentials at once.
- Stopped children stay readable and can be sent work again, which is a rebind.

**A4. Remembered rules in children** (refines E5).
- Always rules and session rules apply in a supervised child, and its cards may offer to keep one, only when all of
  these hold:
  - the call is a file edit or read;
  - every canonical target is inside the child's own worktree lease, or the shared root for a read, under the same
    containment and sensitive-file checks as Accept edits;
  - for an edit, the child's `task_mode` is a writing mode that is still inside the parent principal's live ceiling.
- Shell and MCP rules never apply in a child.
- `InheritedAuthority` is still judged first, before every rule, Accept edits and Bypass path. Unknown tools are never
  auto-allowed, and they are denied in `plan`.
