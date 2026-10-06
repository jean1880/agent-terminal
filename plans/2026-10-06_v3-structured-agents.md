# agent-terminal 3.0.0: structured agent sessions (Claude, Agy, later Codex)

> **Scope decisions (user, 2026-10-06):**
> - This is a **major version bump of agent-terminal** (2.1.0 → 3.0.0), not a new app.
> - Gemini CLI is out of scope. Gemini models are reached only through agy.
> - `libgtksourceview-5-dev` 5.18.0 is installed (verified with `pkg-config`).

**Status:** ACTIVE (Phase 0 done). **Owner:** Argus. **Date:** 2026-10-06.

**Branch and releases (user, 2026-10-06):**
- All 3.0 work happens on **`feat/v3-chat-first`**, an exception to the direct-to-master default.
- Each phase lands there.
- master keeps shipping 2.x fixes until 3.0.0 merges.
- The branch ships **beta builds** through debian-maintainer on a separate apt suite, `beta`. That needs a
  beta-channel feature in debian-maintainer itself:
  `~/scripts/plans/ACTIVE_2026-10-06_debian-maintainer-beta-channel.md`.
  - Beta versions look like `1:3.0.0~beta.<UTC committer time>+g<sha>-<build>+debmaintainer`. The
    timestamp stays monotonic across rebases. `dpkg` verifies they sort above 2.1.0 and below 3.0.0.
  - Suite `beta` carries `NotAutomatic` + `ButAutomaticUpgrades`, so apt gives it priority 100 against
    stable's 500. Opt in per package with `apt install agent-terminal/beta`.
  - Beta builds need `libgtksourceview-5-dev` in the task dependencies from Phase 3 on.
  - The Cargo.toml version on this branch becomes `3.0.0` in Phase 1, so betas are numbered against 3.0.0.

## Goal

Rebuild agent-terminal around a native **chat thread** that drives coding agents headless over their
structured protocols. Chat is the default and the focus of the whole design. A direct terminal stays
supported as a drawer and as an optional Terminal thread. It must:
- own the transcript itself;
- switch model or provider mid-thread;
- hand a session from one agent to another;
- offer typeahead for `/commands`, `@files` and `$skills`.

It mirrors the **approach** of T3 Code (pingdotgg/t3code, MIT) without its code size or its web wrapper.

**Not a goal:** porting T3 Code. Its orchestration layer is ~90k lines of Effect-TS (event sourcing, PR
watchers, relay, mobile). We take five ideas and leave the rest:

1. **One adapter per agent** behind one trait. Each adapter normalizes the agent's native stream into a
   small canonical event set.
2. **The app owns the thread.** A provider-neutral `AppThread` holds N `ProviderThread`s, one of them
   active. Native session ids are refs graded `strong|weak|none`, never the source of truth.
3. **A switch classifier.** `decide_transition(current, target)` returns `reuse | switch_model_in_session |
   restart_and_resume | create_with_handoff | reject`
   (t3 `ProviderSessionTransitionPolicy.ts`, 96 lines; port it verbatim).
4. **Budgeted handoff.** Inject the app transcript into the new provider thread, capped at 16k tokens:
   - add the latest user message, then the latest assistant message, then the first user message, then
     fill newest-first;
   - give the agent a `thread_read` MCP tool so it can page in what was omitted
     (t3 `ContextHandoffBudget.ts`, 283 lines; port it).
5. **A capability matrix** per adapter (t3 `orchestrationV2.ts:193-336`). The UI greys out what an agent
   cannot do instead of failing.

Research notes (with file:line refs): `/tmp/claude-1000/-home-jdesroches/c59b6ebc-…/scratchpad/findings-*.md`.
These are session-scoped, and the essentials are copied below.

## Verified facts (2026-10-06)

| Fact | Evidence |
|---|---|
| T3 Code is MIT ("Copyright (c) 2026 T3 Tools Inc.") | `t3code/LICENSE` |
| claude 2.1.291 installed; agy installed; **codex NOT installed** | `claude --version`, `agy --help`, `which` |
| The Agent SDK spawns `claude --output-format stream-json --verbose --input-format stream-json [--permission-prompt-tool stdio] [--model M] [--permission-mode P] [--mcp-config JSON] [--thinking …] [--effort …]` | `@anthropic-ai/claude-agent-sdk@0.3.291` `sdk.mjs` |
| Claude control requests over stdin/stdout include `initialize`, `interrupt`, `can_use_tool`, `set_model`, `set_permission_mode`, `mcp_status`, `mcp_toggle`, `mcp_reconnect`, `get_settings`, `update_settings`, `get_context_usage`, `list_models`, `file_suggestions`, `get_usage`, `rewind_files`, `rename_session`, `stop_task`, `reload_skills` | SDK `sdk.d.ts:3861-5113` |
| Claude pushes `system/commands_changed` with the full slash-command list (replace the cache, don't merge) | `sdk.d.ts:3728` |
| agy supports `-p --input-format stream-json --output-format stream-json`, `--conversation <id>`, `--model`, `--mode plan\|accept-edits`, `--disable-slash-commands` | `agy --help` |
| GTK 4.22.4, libadwaita 1.9.1 and GtkSourceView 5.18.0 installed. `sourceview5` 0.11.2 (MIT) pairs with gtk4 0.11. | `pkg-config`, `cargo info` |
| agent-terminal 2.1.0 uses no async runtime: gio plus `tracing-journald` (identifier `agent-terminal`) | `Cargo.toml` |
| T3 does not parse `~/.claude/projects`. It uses SDK helpers; we parse the jsonl ourselves (agent-terminal already does) | claude findings §8 |

**UNVERIFIED (checked in Phase 0):**
- [ ] the exact JSON shape of each control request/response (read `sdk.d.ts` per subtype);
- [x] agy's stream-json has its own schema (event/step_update/result). Live approvals work through a PreToolUse hook. See Phase 0 results.
- [ ] Claude `file_suggestions` returned `[]` (warm-up or query form). Re-probe in Phase 2.
- [ ] agy hook `timeout` ceiling: how long a hook may wait for the user. Probe in Phase 6. The hook enforces its own fail-closed deadline regardless.
- [x] agy speaks ACP: **no**. The local CLI has no ACP flag; T3 uses Google's separate `agy_acp_server.par` bundle.
## Phase 0 results (probed 2026-10-06, claude 2.1.291, isolated HOME sandbox)

Evidence is kept **outside the repo** in `~/.local/state/agent-terminal/phase0-2026-10-06/` (mode 0700):
- `rec/*.ndjson` recordings, which contain the account email and local paths but no tokens;
- `scenarios/`;
- the probe driver source (`src/`, `Cargo.toml`);
- `hook-probe.sh`;
- T3 research notes (`findings-*.md`).

Recordings are scrubbed and hand-reviewed before any of them land in `tests/fixtures/`. The sandbox
(copied login files plus a scratch repo) was deleted after the probes.

**Claude: all core assumptions verified.**

| Question | Answer (evidence: rec/claude-{a,b,c}.ndjson) |
|---|---|
| Argv | `claude --output-format stream-json --verbose --input-format stream-json --permission-prompt-tool stdio --include-partial-messages --model M` works with no `-p`. Session flags use `=`: `--resume=<id>`, `--session-id=<id>`, `--resume-session-at=<uuid>`. |
| Handshake | `control_request{subtype:initialize}` gets a reply in ~0.3–0.7 s. The reply carries account, agents, models, commands, output styles and `session_state`. |
| Approval | `control_request{subtype:can_use_tool, tool_name, input, tool_use_id}` → reply `control_response{response:{subtype:success, request_id, response:{behavior:allow, updatedInput, toolUseID}}}`. The edit landed (`git diff`: calc.py +3). |
| Streaming | `stream_event` deltas: `thinking_delta`, `signature_delta`, `input_json_delta` (tool args stream in) and `text_delta`. A complete `assistant` snapshot arrives per content block. |
| Per-turn init | `system/init` is **re-emitted at the start of every turn**, so it is the source of truth for the current model. |
| Typeahead source | `init.slash_commands` holds bare names (53). **`system/commands_changed` holds `{name, description, argumentHint, builtin}`** (66) and arrives about 70 ms later, so build the menu from it. `init.terminal_slash_commands` = `doctor, color, focus, reload-plugins` (hide these). `init.skills` lists skills. |
| Native panels | `mcp_status`, `get_settings` (`applied/effective/sources`), `list_models` (displayName, resolvedModel, supportedEffortLevels, supportsFastMode), `get_context_usage` (maxTokens, autoCompactThreshold, categories) all answer in **under 2 ms before any turn**. `/mcp`, `/config`, `/model` and the context gauge can load instantly. |
| `file_suggestions` | Returned `[]` for "calc" with calc.py present. **Open:** retry after the first turn (index warm-up?) or check the query form, else fall back to our own walk. |
| `set_model` | **Works in-session with no restart.** It emits a user frame `<local-command-stdout>Set model to …`, the next `init.model` is `claude-sonnet-5-5`, and the reply came from Sonnet. `switch_model_in_session` is real, so T3's restart-on-switch is unnecessary for Claude. |
| Local commands (`/cost`) | **Correction to the review:** there is no `local_command_output`. The CLI sends an `assistant` frame with `message.model:"<synthetic>"`, then `result{num_turns:0}`. Render it as a system note. The turn does close. |
| `/compact` | `system/compact_boundary{trigger, pre_tokens:21478, post_tokens:2082, preserved_segment}`, then a synthetic summary `user` frame (`isSynthetic:true`, render as a divider, not a bubble), then `<local-command-stdout>Compacted</local-command-stdout>`, then `result`. A stale synthetic assistant frame was replayed, so dedup by uuid. |
| Interrupt | `control_request{subtype:interrupt}` acks in 1 ms (`{still_queued:[]}`), then a final partial `assistant`, then `result{subtype:"error_during_execution", is_error:true, terminal_reason:"aborted_streaming"}`. **Map `aborted_streaming` → Interrupted, not Error.** |
| Rate limits | `rate_limit_event` arrives each turn, which feeds the quota and "Continue in" logic. |

**agy (stream-json): works with the user's Google login session (requirement, user 2026-10-06).**

| Question | Answer (evidence: rec/agy-{c,d,e}*.ndjson, `agy changelog`) |
|---|---|
| Argv | `agy --input-format stream-json --output-format stream-json [--mode accept-edits\|plan] [--model M] [--conversation <id>]`. Auth comes from `~/.gemini/oauth_creds.json` (the login session), and a sandboxed `HOME` works. |
| Input | **`{"event":"user","message":{"role":"user","content":"…"}}`**, one turn per line. Other event names warn and are ignored. A frame that does not decode kills the process. |
| Output | `init{conversation_id, cwd, permission_mode, tools}` → `step_update{step_index, step_type: user_input\|agent_response\|tool\|…, state: ACTIVE\|DONE, text_delta?, tool_name?, tool_info{name, parameters, output?}, duration_seconds?, usage?}` → `result{status: SUCCESS\|ERROR, response, num_turns, usage{input,output,thinking,cache_read,total}, error?, denied_actions?}` |
| Tool cards | Structured. In-workspace edits (`replace_file_content`) **ran without asking** in the default `request-review` mode (calc.py +6). |
| Approvals | **VERIFIED live approvals via a PreToolUse hook** (rec/agy-g-hook-skip.ndjson, rec/hook-calls.ndjson).<br>• Headless mode alone soft-denies `run_command`, and a hook `allow` does **not** override that (rec/agy-f-hook).<br>• So agy runs with **`--dangerously-skip-permissions`**, and **our hook is the gate**. `{"decision":"allow"}` → the command ran and its output returned. `{"decision":"deny","reason":R}` → step `state:"ERROR"`, `tool_info.error.message:"tool call denied by pre-tool hook: R"`, and the agent continues.<br>• The hook gets the parent env (our `AGENT_TERMINAL_APPROVAL_SOCKET` reached it) and a payload `{toolCall{name,args}, conversationId, modelName, stepIdx, transcriptPath, workspacePaths, artifactDirectoryPath}`.<br>• The user's own global guard hooks still run alongside ours, since they load from the real HOME. |
| Hook install | One **permanent** entry: `agent-terminal --approval-hook`, matcher `run_command\|write_to_file\|replace_file_content\|call_mcp_tool\|…`.<br>• It is a **no-op when `AGENT_TERMINAL_APPROVAL_SOCKET` is unset** (exit 0 with no stdout = allow, verified 2026-09-26). Interactive agy and other callers are unaffected.<br>• For this user the entry goes through `~/git/agent-config/sync` with agent-sync, never a hand edit of hooks.json. For others, the app offers to add it on first agy use, with consent and an atomic write.<br>• **Fail-closed:** if the socket is set but unreachable, or the user does not answer within the hook's own deadline (set below the configured `timeout`), the hook returns deny with a reason.<br>• `<workspace>/.agents/hooks.json` is the per-project alternative, but it would write into user repos, so it is not used. |
| Read-only commands | `/model /usage /effort /credits /config /skills /help /hooks /permissions /changelog` return an **error inside a stream-json session, and the process exits** (rec/agy-d). Run each as its own `agy -p /<cmd> --output-format json`: no turn, no quota (changelog). This backs the native panels. **The adapter must intercept these and never forward them.** `agy models` is plain TSV (`id\tdisplay`) and has no `--output-format`. agy also offers `claude-*-5-5-*` and `gpt-oss-120b` on Google quota. |
| Model switch | **VERIFIED** `restart_and_resume`: a new process with `--conversation <id> --model gemini-3.1-pro-low` kept the same conversation id, `init.model` showed the new model, and the history was remembered (rec/agy-i-resume). Restart cost is about 2 s. |
| Interrupt | **VERIFIED:** SIGINT gives `result{error:"interrupted", response:<partial>}` and **the process exits** (rec/agy-h-interrupt). Interrupt = SIGINT, then the next turn respawns with `--conversation <id>`. |

## Gemini route: DECIDED, the agy CLI (user, 2026-10-06)

**Decision:** drive Gemini through the `agy` CLI in stream-json mode, with live approvals through its
PreToolUse hook, as verified above.

Constraints that decided it:
1. **It must use the Google login session** (user requirement). Subscription quota is reachable only
   through Google's own clients. An API key bills separately.
2. **No deconstructing agy, and no replaying its OAuth token** against Google's private backend (user
   agreed). In Feb 2026 Google suspended paying Antigravity subscribers who did this through third-party
   tools (OpenClaw, OpenCode, custom CLI scripts), under the ToS clause "in connection with products not
   provided by us". Some lost Gmail and Workspace too, and access was restored later. Driving Google's own
   process stays within its supported interfaces.

Rejected alternatives, kept for the record:

| Route | What it gives | Auth / billing | Rust |
|---|---|---|---|
| **agy CLI (stream-json) — CHOSEN** | agy's agent loop and tools; schema now known; approvals via hook | the Google login agy already holds | own codec (about 3 event kinds in, 3 out) |
| Google ACP bundle (`agy_acp_server.par`, T3's route) | agy's loop over documented ACP with approvals | Google login | `agent-client-protocol` crate; 926 MB bundle, ~1 GB unpack per launch |
| **Gemini Developer API (REST `streamGenerateContent`, SSE)** | **the model only. We run the agent loop.** | `GEMINI_API_KEY` (AI Studio: free tier and paid per token). **Not** the Google One/AI subscription. No key is configured today (`env-inspect`). | No official Rust SDK: Google ships Python, TS, Go, Java, .NET and Kotlin only. It is one REST endpoint, so a thin client in-tree. |
| Vertex AI | the model only | GCP project + billing | official `googleapis/google-cloud-rust` `aiplatform` |

Do **not** reuse agy's OAuth token against Google's internal Code Assist endpoint. It is not a public API.

A **native agent loop** (our own tools plus a model API: Gemini API key, or any OpenAI-compatible endpoint
such as a LiteLLM gateway) stays a possible **post-3.0** adapter. It would make model switching free,
because we would own the context. It is out of 3.0 scope, and it would not use the login session.

Claude stays on the `claude` CLI adapter: subscription billing, and its own loop and tools.

## Architecture

agent-terminal 3.0.0 still ships **one standalone binary** (AGENTS.md "Standalone Philosophy"). Internally it
becomes a Cargo workspace, so that the protocol logic is GTK-free and testable:

```
agent-terminal/                 # workspace root; the bin package stays `agent-terminal`, version 3.0.0
  crates/
    agent-core/                 # NO GTK, NO I/O. Pure, unit-tested.
      event.rs                  #   canonical Event enum (below)
      caps.rs                   #   Capabilities struct
      transition.rs             #   decide_transition (port of t3 ProviderSessionTransitionPolicy)
      handoff_budget.rs         #   select_history / handoff_budget / render (port of t3 ContextHandoffBudget)
      commands.rs               #   command registry + ranking (port of t3 composerSlashCommandSearch)
      redact.rs                 #   MOVED from src/handoff.rs: one scrubber for briefs, handoffs, raw frames
      claude.rs, agy.rs         #   adapters as pure state machines: feed(line) -> Vec<Event>,
                                #   encode(Command) -> line. No processes, so replay tests are trivial.
    agent-kit/                  # MOVED from src/: handoff (session readers, briefs), git (checkpoints),
                                # restore, diff, worktree. Shells out to git; no GTK.
  src/                          # the GTK app (bin)
    window/…                    #   REBUILT shell: thread sidebar + content; 2.x terminal code reused for drawer/Terminal threads
    chat/                       #   NEW chat thread (the primary surface): transcript, composer + completion, approvals, panels
    agent_proc.rs               #   NEW gio::Subprocess transport: async line reads on the main loop
    store.rs                    #   NEW rusqlite (bundled) event log + projections, under $XDG_STATE_HOME
  tests/fixtures/               # recorded, scrubbed ndjson (ours, plus t3 testkit fixtures with the MIT notice)
```

**No tokio.** On the minimal-fix ladder, the platform rung holds. Each agent process is a `gio::Subprocess`
with piped stdio:
- stdout is read with `gio::DataInputStream::read_line_future` inside `glib::spawn_future_local`;
- writes go through the async output stream;
- each line goes through the adapter's pure `feed()` and the resulting events go straight to the UI.

There are no threads and no channels. This matches the existing "never block the main thread / gio"
mandate and adds no runtime.

**Chat-first (user, 2026-10-06).** The 3.0 design is built around the chat flow. The terminal is
supported, not designed for.
- **The unit is a thread, not a tab.**
  - The window's primary navigation is a thread sidebar grouped by project folder.
  - Every new-session action opens a Chat thread: `Ctrl+Shift+T`, `+`, "New in Folder…", "New in
    Worktree", Resume and Continue In.
  - The agent and model are thread properties shown in the header, and can be switched mid-thread.
  - They are not a choice of profile at creation.
- **The terminal is secondary.**
  - It is a drawer under the chat (`Ctrl+\``): a VTE shell in the thread's directory, for running things
    yourself.
  - A raw CLI session (2.x behaviour) is still possible as a **Terminal thread** in the sidebar, from
    "New Terminal Thread" or for a profile with no adapter.
  - 2.x terminal code is kept and works. It gets no new features in 3.0 beyond being reachable this way.
- **Every 2.x feature is re-homed onto the thread:**
  - diff panel, checkpoints, undo and worktrees operate on the thread's folder;
  - attention marks, bells and notifications go on the sidebar row;
  - quota detection becomes the structured `RateLimited` event, which offers "Continue in <agent>" as an
    in-thread handoff;
  - session restore reopens threads from the store.
- **Config migration.** 2.x `profiles` become agent definitions (command, args, env_file, dir). The
  `resume_args`/`session_store` keys still serve Terminal threads and the Resume browser. No setting
  makes Terminal the default; that is the deliberate 3.0 break, and the upgrade notes call it out.

**New minimum system libraries** (a deliberate decision, per AGENTS.md): add GtkSourceView 5.12 via
`sourceview5` features `v5_12` and `gtk_v4_10`. Ubuntu 24.04 ships 5.12, so 24.04 support holds.
`$auto` in cargo-deb picks up `libgtksourceview-5-0`; do not hand-list it.

**Docs to update in 3.0:** AGENTS.md "Module layout", "PTY Bridge" (drop `gemini`) and "Logic separation";
README features; the upgrade notes ("Upgrading from 2.x").

### Canonical event set (the small core, from t3 `providerRuntime.ts`)

```rust
struct Envelope { id, thread: AppThreadId, provider_thread: ProviderThreadId, turn: Option<TurnId>,
                  item: Option<ItemId>, request: Option<RequestId>, at: DateTime, raw: Option<RawFrame>, ev: Event }
enum Event {
  SessionStarted { native_id: Option<String>, resume: Option<serde_json::Value> },
  SessionExited  { recoverable: bool, reason: Option<String> },
  CommandsChanged(Vec<AgentCommand>),            // typeahead source
  TurnStarted   { model: String, effort: Option<String> },
  TurnCompleted { state: TurnState, usage: Option<TurnUsage>, cost_usd: Option<f64>, error: Option<String> },
  ItemStarted / ItemUpdated / ItemCompleted { kind: ItemKind, status, title, detail, data, parent_tool_use: Option<String> },
  ContentDelta  { stream: StreamKind /* assistant|reasoning|plan|command_output */, delta: String },
  PlanUpdated   { steps: Vec<(String, StepStatus)> },
  RequestOpened { kind: RequestKind, options: Vec<Decision>, args: serde_json::Value, response: ResponseCapability },
  RequestResolved { decision: Decision },
  UserInputRequested { questions: Vec<Question> },
  UsageUpdated  { used: u64, max: Option<u64>, auto_compact_at: Option<u64> },   // context gauge
  Compacted     { before: u64, after: Option<u64>, trigger: Manual|Auto },
  RateLimited   { resets_at: Option<DateTime> },  // drives the "Continue in X" offer
  LocalCommandOutput { text: String },            // system/local_command_output: command handled by the CLI, NO TurnCompleted follows
  Unknown,                                        // #[serde(other)] catch-all; frame kept in `raw`, stream never aborts
  Error         { class: ErrorClass, message: String },
}
```

`raw` keeps the native frame for a debug pane and for re-mapping after an adapter fix.

### Adapter trait

An adapter is split in two:
- a **pure, sans-I/O state machine** in `agent-core`;
- the shared **gio transport** in `src/agent_proc.rs`.

The transport owns the process and correlates control responses by request id. The adapter only
translates in both directions.

```rust
trait Adapter {                                    // agent-core, no I/O
  fn driver(&self) -> DriverKind;
  fn capabilities(&self) -> &Capabilities;
  fn argv(&self, o: &OpenSession) -> Vec<String>;  // e.g. claude --input-format stream-json … --resume <id>
  fn plan_selection(&self, cur: &ModelSelection, tgt: &ModelSelection) -> SelectionPlan; // apply_on_next_turn|restart|handoff|reject
  fn encode(&mut self, c: Command) -> Result<Vec<String>>;  // Command: Prompt|Interrupt|Respond|SetModel|Control(..)|InjectHistory
  fn feed(&mut self, line: &str) -> Vec<Envelope>;          // one stdout line -> zero or more canonical events
  fn on_exit(&mut self, status: ExitStatus) -> Vec<Envelope>; // closes open items as interrupted, expires approvals
}
```

`encode` returns `Err(Unsupported)` where a capability is missing (for example `SetModel` on agy). The
caller then follows `decide_transition`: restart and resume, or hand off. Replay tests feed fixture lines
into `feed` and compare the result against golden event sequences, with no process or display involved.

### Claude adapter: the mapping we need (from claude findings)

| Area | How it works |
|---|---|
| Spawn | argv as in Verified facts, with `--permission-prompt-tool stdio` and `--include-partial-messages` if verified. Use `--session-id <uuid>` on a new thread, `--resume <id>` (optionally `--resume-session-at <uuid>`) to continue. Env passes through and may use `CLAUDE_CONFIG_DIR`. |
| Frames | `system/init` gives session_id, model, tools, mcp_servers and the slash command list. `stream_event` carries partials; T3 streams only thinking from these. `assistant` is the complete snapshot, one per content block, deduped by message uuid. `user` with `tool_result` completes the item. `system/compact_boundary` maps to Compacted. `system/api_retry`, `task_*` map to the subagent tree. `rate_limit_event` maps to RateLimited. `result` maps to TurnCompleted. |
| Approvals | `can_use_tool` control request becomes RequestOpened. Reply with allow/deny, optionally `updatedPermissions` (session scope) and `interrupt:true` on cancel. `AskUserQuestion` maps to UserInputRequested, answered via `updatedInput.answers`. |
| Model switch | T3 restarts with `--resume` on every model, effort or policy change and refuses to while background tasks run. We try the `set_model` control request first (apply on next turn) and fall back to a restart. |
| Interrupt | Send the `interrupt` control request. Force-finalize after 10 s. Close any unfinished tool items as interrupted. |
| Traps | A `result` with `subtype:"success"` can still be a failure (`is_error`, `api_error_status` 401/429/529). Frames with `parent_tool_use_id` arrive before the Task item registers, so buffer them. The prompt uuid echo separates prompt turns from wake turns. |

### Agy / ACP / Codex adapters

Filled from the codex+acp research and Phase 0 probes. See the § "Adapter notes: agy, ACP, Codex" appendix.

## Typeahead and commands (the user's explicit ask)

T3's design, ported. A `GtkSourceView 5` composer with `GtkSourceCompletionProvider`s, one per trigger.
Its popover, keyboard navigation and fuzzy filtering are native:

| Trigger | Provider | Source | Applies |
|---|---|---|---|
| `/` at prompt start | **Built-in** | the app's command registry | locally, never sent |
| `/` at prompt start | **Agent** | `system/init` `slash_commands` + `commands_changed` (Claude); `available_commands_update` (ACP); probe for agy | sent as text; only valid at the start of a message (t3 `slashCommandItemsForPromptPosition`) |
| `$` anywhere | **Skill** | Claude: skills in init; agy: `~/.gemini` skills | inserted as a mention |
| `@` anywhere | **File** | Claude `file_suggestions` control request; others: `ignore`-crate walk with fuzzy match | inserted as a path |

Ranking (t3 `composerSlashCommandSearch.ts`):
- the name is scored exact 0, prefix 2, word boundary 4 (at `- _ /`), substring 6, fuzzy 100;
- a description match scores +20 on the same scale;
- ties sort built-in, then agent, then skill.

**Built-in commands are native panels backed by adapter controls**, so `/mcp` and `/config` work even
though the TUIs that own them are not running:

| Command | Does | Claude backing | Others |
|---|---|---|---|
| `/model` | model + effort picker (AdwDialog) | `list_models`, `set_model` | restart + `--model` |
| `/mcp` | server list with status, toggle, reconnect | `mcp_status`, `mcp_toggle`, `mcp_reconnect` | `agy mcp list` / `enable` / `disable` |
| `/config` | settings view and editor | `get_settings`, `update_settings` | read-only view of the settings file |
| `/compact` (alias `/compress`) | compact the context | agent's own `/compact` | agy's command (probed in Phase 0; likely `/compress`); else handoff-to-self |
| `/handoff <agent>` | switch provider with a budgeted handoff | n/a | n/a |
| `/fork` | branch the thread from this turn | `--resume --fork-session` / `resume-session-at` | handoff into a new thread |
| `/mode plan\|default` | interaction mode | `set_permission_mode` | `--mode` |
| `/usage` `/context` | quota and context gauge | `get_usage`, `get_context_usage` | from usage events |
| `/rewind` | undo the last turn's files | `rewind_files`, or app git checkpoint | app git checkpoint |
| `/clear` `/new` | new app thread | n/a | n/a |

An alias table per adapter (`/compress` → `/compact` for Claude) means muscle memory works across agents.

Rules (from review, verified in `sdk.d.ts`):
- **Agent commands are sent as plain user text.** Prompt text is processed as a slash command (`sdk.d.ts:69`).
  `/compact` produces a `compact_boundary`.
- **Locally handled commands have no `result`.** They reply with `system/local_command_output` (`sdk.d.ts:5276`)
  and bypass the model loop. The turn state machine must close the turn on that frame, or the UI stays
  "running" forever.
- **Hide terminal-bound commands.** Init carries `terminal_slash_commands` (`sdk.d.ts:5945`). Drop them from
  typeahead: `/exit`, `/statusline` and the like make no sense without the TUI.
- **Built-ins shadow agent commands.** A built-in with the same name as an agent command wins, and the list
  shows one entry. `/mcp`, `/config` and `/model` sent as text would try to open a TUI that is not there.
- **Debounce `@` suggestions.** `file_suggestions` is a round-trip to the CLI. Debounce it (~120 ms) and
  cancel stale requests.

## UI (GTK4 + libadwaita 1.9, "make it pretty")

- **Shell (chat-first):**
  - `AdwApplicationWindow` with an `AdwOverlaySplitView`. It replaces 2.x's `adw::TabView` tab bar as the
    primary navigation.
  - **Sidebar:** threads grouped by project folder and searchable. Each row shows an agent accent dot,
    title, relative time and a state badge (running spinner, needs approval, unread, rate-limited).
    "New thread" sits at the top.
  - **Content:** an `AdwToolbarView`. Its header bar holds the thread title, an agent and model chip
    (`AdwSplitButton` that opens the `/model` picker, including cross-agent switching), a context gauge,
    and diff and terminal toggles.
  - The sidebar collapses at narrow widths (`AdwBreakpoint`).
- **Terminal drawer:** a vertical `GtkPaned` holding a `GtkRevealer` with a VTE shell in the thread's
  folder, opened with `Ctrl+\``.
  - Not `AdwBottomSheet`: that needs libadwaita 1.6, and the floor stays at 1.5.
  - A Terminal thread is the same drawer filling the whole content area.
- **Transcript:** a `GtkListView` over a `gio::ListStore` of item objects, so long threads are
  virtualized:
  - Markdown via `pulldown-cmark` into `GtkTextView` tags, with code blocks in `GtkSourceView` and syntax
    highlighting.
  - Tool calls are collapsible cards built as a **custom widget**, not `AdwExpanderRow`, which belongs in
    preferences lists. Each card has a status icon, title and output. Its expanded/collapsed state lives in
    the model item, so it survives row recycling.
  - **Highest-risk choice.** `GtkListView` with variable-height, streaming markdown rows has known
    problems: row heights change while streaming, building a TextView per bind is slow, and stick-to-bottom
    scrolling is hard. Phase 3 starts with a spike (5k mixed items, streaming into the last row,
    expand/collapse). **Fallback:** a `GtkBox` in a `GtkScrolledWindow` that renders only the last N items.
    Subagent cards nest by `parent_tool_use`.
  - Reasoning shows as a dimmed, collapsed "Thinking…" row.
  - Each provider-switch boundary is an inline banner with the agent's accent colour.
- **Approvals:** an inline card on the item with Allow / Allow for session / Deny buttons, not a modal.
  An `AdwToast` appears if the window is not focused.
- **Plan/todos:** a pinned `GtkRevealer` panel above the composer.
- **Diff:** an `AdwOverlaySplitView` secondary pane with `GtkSourceView` and diff highlighting (reuse
  agent-terminal's diff/checkpoint logic).
- **Composer:** `GtkSourceView` plus the completion providers above. Enter sends; Shift+Enter adds a
  newline. Esc interrupts while a turn is running.
- **Style:**
  - Build on the existing brand chrome (AGENTS.md: background `#181425`, foreground `#c8c8ff`), extended
    into an embedded `style.css` with cards, bubbles and badges.
  - One accent colour per agent (t3 `accentColor`).
  - The terminal themes (`ThemeChoice`) also tint code blocks.
  - No custom theme engine.
  - Every surface gets a `preview_app` screenshot review in Phases 3, 4 and 6.

## Cross-cutting decisions (from plan review, 2026-10-06)

- **Secrets.** Handoff text, stored `raw` frames and committed fixtures can all carry secrets. Scrub every
  one of them with **one** redactor:
  - Move `src/handoff.rs` `redact()`/`mask()` (pem, url-credential, bearer, flag-value; `****last4`) into
    `agent-core::redact`.
  - The AGENTS.md rule "every brief goes through `handoff::redact`" extends to handoffs and stored raw
    frames.
  - Take no `nuvek-core`/`homelab` dependency: the Standalone Philosophy forbids it.
  - Test: a planted `ghp_` token is masked both in a rendered handoff and in the stored raw frame.
- **Workspace split first.** Phase 1 moves the pure and git modules into `agent-core`/`agent-kit`. No
  behaviour changes, so the existing tests must pass unchanged. Rollback is reverting the split commit.
  - Chat-thread checkpoints use `refs/agent-terminal/chat/<thread>/…`, a sub-namespace kept apart from
    tab checkpoints.
- **Logging.** The existing `tracing` plus `tracing-journald` setup with identifier `agent-terminal`. Add
  spans for adapter spawn, exit and protocol errors. Never log prompt or frame bodies, which may hold
  secrets.
- **Gate.** The existing CI (fmt, `clippy --all-targets -D warnings`, tests under Xvfb) becomes
  `--workspace`. The rust-reviewer agent runs after every Rust phase. Gitleaks pre-commit covers
  fixtures.
- **Tests never touch live state.**
  - Record fixtures with `CLAUDE_CONFIG_DIR` and agy's `GEMINI_HOME` pointed at tempdirs.
  - Scrub recorded fixtures and review them by hand before committing.
  - Store tests use a tempdir database.
  - "Real turn" checks are manual smoke checks, not `cargo test`.
- **Licence.** Rust ports of t3 logic are derivative works. Keep T3's MIT notice in `THIRD_PARTY.md` and in
  the headers of the ported files.
- **Interrupted turns.** On restart or interrupt mid-turn, close every open item, including nested `Task`
  subagent trees, as `interrupted`. Mark pending approvals `expired`.
- **`claude-codes`.** Pin it with `=x.y.z`, types feature only. Re-check it against `claude --version` on
  every bump.

## Phases (each ends in a runnable check)

| # | Deliverable | Done when |
|---|---|---|
| 0 | ✅ **DONE 2026-10-06** (except committing fixtures). Wire-format probes in an isolated-HOME sandbox: claude (turn, approval, controls, local command, `/compact`, `set_model`, interrupt) and agy (input schema, tool steps, hook approvals, read-only commands, SIGINT, resume with model switch). Agy decision recorded. Recordings are in the session scratchpad. They get scrubbed and hand-reviewed as Phase 2/6 fixtures. | ✅ results table above; remaining opens listed under UNVERIFIED |
| 1 | **Workspace split** (no behaviour change: pure and git modules into `agent-core`/`agent-kit`, CI to `--workspace`), then `agent-core` additions: events, caps, ports of `decide_transition`, `select_history`/`handoff_budget` and command ranking, each with t3's cases as unit tests | every pre-split test passes unchanged; new tests cover every transition branch and budget edge |
| 2 | Claude adapter (pure `feed`/`encode`) + replay golden tests + `agent_proc.rs` gio transport + a hidden `agent-terminal --chat-probe <prompt>` smoke mode | replay tests green; manual: a real turn with one approval round-trips |
| 3 | **Spike first** (ListView vs box fallback; GtkSourceView 5 completion with `/` at prompt start and `$`/`@` anywhere), then the chat-first shell: thread sidebar replacing the tab bar, transcript, composer, inline approvals, `/model`, terminal drawer; 2.x features re-homed (diff, checkpoints, worktrees, attention) | spike verdicts recorded; 5k-item thread scrolls without panic; a real turn in the UI via `preview_app`; every 2.x feature reachable |
| 4 | Typeahead (built-in, agent, skill, file) + native panels `/mcp` `/config` `/compact` `/usage` | popover shows deduped, ranked items, with terminal commands hidden; `/mcp` toggles a server; `/compact` closes its turn |
| 5 | **Store** (SQLite event log, scrubbed raw) + threads persist and the sidebar reopens them after restart (replaces 2.x session restore); 2.x config migration (profiles → agents) with migration tests | kill and relaunch → thread resumes; planted-secret test passes |
| 6 | Agy adapter (stream-json + `--dangerously-skip-permissions` + `agent-terminal --approval-hook` over a unix socket; hook entry added via agent-config/agent-sync; read-only commands as separate `agy -p /cmd` calls); `/handoff` and Chat-tab "Continue In" as a structured, budgeted, scrubbed handoff; `/fork`, `/rewind` via `agent-kit` checkpoints/restore | a Claude thread continues in agy with history, and back; rewind is undoable |
| 7 | **3.0.0 release**: AGENTS.md/README/upgrade notes; debian-maintainer → CI → apt (BUILD-RELEASE; `$auto` picks up `libgtksourceview-5-0`); tag v3.0.0 + GitHub Release with notes | `apt install agent-terminal` gives 3.0.0; release has notes; CI badge green |
| later | Codex adapter (needs `codex` installed) | — |

## Risks

- **The Claude control protocol is not a documented public API.** It follows the SDK version. Mitigations:
  pin to the SDK's `sdk.d.ts` for the installed CLI version; keep `raw` frames; golden replay tests; a
  version advisory when `claude --version` changes.
- **Approvals are live-only.** A `can_use_tool` callback dies with the process, so model responses carry
  `ResponseCapability` and the UI marks stale requests expired.
- **Agy's stream-json input is undocumented.** It was found by probing (`{"event":"user",…}`), and agy
  ships about weekly. Mitigations: golden replay tests from recorded fixtures; a version advisory when
  `agy --version` changes; treat unknown `step_type`s as generic cards.
- **The agy approval path runs with `--dangerously-skip-permissions`.** Safety rests on our hook. The hook
  must fail closed: socket set but unreachable, no answer before its deadline, or a parse error all mean
  deny. Test each case. The user's own guard hooks still apply. Without the hook installed, the adapter
  must **refuse** to start agy with skip-permissions and fall back to `--mode plan` (read-only) with a
  banner.
- **Account safety.** Only drive Google's own `agy` process. Never call Google endpoints with agy's
  token (see the Gemini route decision).
- **Scope creep toward T3 parity.** Out of scope: PR watching, remote relay, mobile, scheduled tasks.

## Gaps to install (AUTONOMY: report, offer)

- ~~`libgtksourceview-5-dev`~~: installed 2026-10-06 (5.18.0). Add it to `make deps` in Phase 3.
- `codex` CLI is optional and needed only for the later Codex adapter.

## Is TypeScript required? No. Every protocol is a subprocess speaking NDJSON

T3 uses TS SDKs only because T3 is a TS app. Each SDK spawns the agent binary and exchanges
newline-delimited JSON over stdio, so Rust can do the same directly.

| Agent | Wire | Rust option (verified 2026-10-06) | Decision |
|---|---|---|---|
| Claude | `claude` stream-json in/out plus `control_request`/`control_response` frames | **`claude-codes`** 2.1.293, Apache-2.0, by meawoppl/rust-code-agent-sdks (28★, pushed today, 72 versions whose version numbers track the CLI: 2.1.293 vs our 2.1.291). Serde models plus a tokio client. | Use it for **types only** (`default-features=false, features=["types"]`, which pulls in no tokio), pinned and reviewed. Write our own gio process/control loop so approvals, interrupt and set_model are under our control. If it lags or breaks, vendor the types. |
| Claude (avoid) | n/a | `claude-agent-sdk` 0.1.1 claims repository `anthropics/claude-agent-sdk-rust`, which **404s**. Its crates.io owner is an individual, not Anthropic. | **Do not use.** Misleading provenance. |
| Antigravity | the local `agy` has **no ACP flag**, only `-p --input-format/--output-format stream-json`. T3 instead downloads Google's separate `agy_acp_server.par`: a 926 MB PyInstaller bundle that unpacks ~1 GB per launch, with its own OAuth and a private `GEMINI_HOME`. | Our own small serde codec for the local `agy` stream-json (no 1 GB bundle, reuses the existing login). | **Decided:** agy stream-json + hook approvals. No ACP crate needed. |
| Codex | `codex app-server` NDJSON (JSON-RPC shape **without** a `"jsonrpc"` field) | Upstream `openai/codex` `codex-rs/app-server-protocol` (Rust, with a `schema/` dir), used as a git dependency. Not crates.io `codex-app-server-protocol` 0.63, which is a third-party fork (namastexlabs). | Later phase. Codex is not installed. |

Net: **no TypeScript or Node in the runtime.** The whole stack is Rust plus the agent binaries themselves.
(Whatever agy and Claude are written in, they are just processes to us.)

## Appendix: Adapter notes: agy, ACP, Codex (from the codex+acp research)

**ACP (fallback only: the Antigravity `agy_acp_server` bundle)**
- Lifecycle: `initialize{protocolVersion}` → (`authenticate`) → `session/new{cwd,mcpServers}` | `session/resume` (preferred over `session/load`, which replays every tool call) → `session/prompt{sessionId,prompt:[ContentBlock]}` → stop.
- Cancel: the `session/cancel` notification. For Antigravity, wait for the prompt to return `stopReason:"cancelled"`, then kill on timeout.
- Model: `session/set_config_option{configId:"model"}`. Mode: Antigravity `yolo|auto_edit|default`.
- `session/update` kinds:
  - agent_message_chunk → ContentDelta(assistant)
  - agent_thought_chunk → ContentDelta(reasoning)
  - tool_call / tool_call_update → Item* keyed by toolCallId
  - plan → PlanUpdated
  - usage_update → UsageUpdated
  - **available_commands_update → CommandsChanged (typeahead)**
  - current_mode_update; compaction_*
- Tool kind mapping: execute → command, edit/delete/move → file_change, search/fetch → web_search, else dynamic.
- Assistant text is segmented at each tool call.
- Agent→client requests we must serve:
  - `session/request_permission`: reply `{outcome:{outcome:"selected",optionId}}` or `cancelled`. Antigravity questions arrive here with toolCallId prefix `interaction_`.
  - `fs/read_text_file` and `fs/write_text_file`, confined to allowed roots.
  - `terminal/*`: advertise `terminal:false` at first.
- No rollback. Fork works at the head only. Rewind and fork use the app's portable-context fallback.

**Codex (later)**
- Spawn `codex app-server`, then `initialize` and the `initialized` notification.
- Session calls: `thread/start{cwd,model,config}`, `turn/start{threadId,input,model,effort,approvalPolicy,sandboxPolicy,approvalsReviewer,summary:"detailed"}`, `turn/interrupt`, `turn/steer`.
- Approvals are server requests whose response is withheld until the user answers: `item/commandExecution/requestApproval`, `item/fileChange/requestApproval`.
- Model switching is per turn.
- Native `thread/fork{lastTurnId}` and `thread/revert{beforeTurnId}`.
- History injection uses `thread/inject_items` with a developer message.
- Send `approvalsReviewer` on every turn, or the previous reviewer stays in effect.
