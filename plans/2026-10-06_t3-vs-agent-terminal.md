# T3 Code versus agent-terminal 3.0: a comparison

**Date:** 2026-10-06. **Status:** reference write-up (requested by the user).

**Sources**
- T3 side: `plans/2026-10-06_t3-feature-inventory.md`, our read-only review of `pingdotgg/t3code` at server
  v0.0.45. The T3 GitHub README was **not** fetched; every T3 claim below comes from that inventory.
  "Not verified" marks anything the inventory does not establish.
- Our side: `README.md`, `AGENTS.md`, `THIRD_PARTY.md`, `plans/2026-10-06_v3-structured-agents.md`,
  `plans/2026-10-06_v3-progress.md`, and the code on `feat/v3-chat-first` (about 80 commits ahead of
  `master`, from `9e77b8fa` to the smoke-test fixes after `81c668e0`).
- Tests: 597 pass on the branch head after the smoke-test fixes (agent-core 175, agent-kit 130, app 287,
  icons 5), with fmt and clippy `-D warnings` clean.

## 1. Summary

- **T3 Code** is a TypeScript, event-sourced agent workbench: a Node server owns provider processes, terminals,
  git and files, and thin clients (React web, Electron, Expo mobile) attach to it. It drives eight provider
  families and adds PR automation, scheduled tasks, remote access and a hosted relay. About 823k non-test lines;
  v0.0.45, desktop still labelled Alpha.
- **agent-terminal 3.0** is a native Rust, GTK4/libadwaita desktop app, about 52k lines of Rust in one
  standalone binary. It drives Claude, agy and Codex headless through structured adapters, owns the thread
  itself, and keeps a real terminal drawer.
- We ported five ideas from T3 (adapter per agent, app-owned thread, transition classifier, budgeted handoff,
  capability matrix) and left the platform (relay, mobile, PR watchers, scheduling) behind.
- **Verdict:** T3 is far broader and has web, mobile and remote reach, more providers and forge integration, but
  carries a large runtime and hosted dependencies. Ours is narrower and local-only, but is small, native, has no
  runtime beyond the agent CLIs, and in a few places (redaction by default, verified agy approval gating,
  external diff tool, retired-model porting) goes further than the T3 features we can establish. Closing the
  gaps in section 6 is about workflow depth (fork, queue, attachments), not architecture.

## 2. Architecture side by side

| Aspect | T3 Code | agent-terminal 3.0 |
|---|---|---|
| Stack | pnpm monorepo, TypeScript with Effect-TS, Node >=22.16/24; React 19 web, Electron 44 desktop, Expo mobile; Rust only for a resource monitor | Cargo workspace, Rust; GTK 4.10+, libadwaita 1.5+, VTE 0.72; no async runtime (gio only) (`AGENTS.md`, "Native GTK4/Libadwaita") |
| Process model | `t3` server (HTTP/WS RPC) owns agents, terminals, git, files; clients are thin | One process; the GTK app owns everything. No server, no ports ("Localhost bound", `AGENTS.md`) |
| Message interception | Per provider: `codex app-server` JSON-RPC, Claude Agent SDK, Cursor SDK, ACP (Grok, Antigravity, registry), OpenCode SDK, Pi RPC | Per agent, a pure sans-I/O adapter: `feed(line) -> Vec<Envelope>`, `encode(Command) -> lines` (`crates/agent-core/src/{claude,agy,codex}.rs`, `adapter.rs`); gio process transport in `src/agent_proc.rs` |
| Normalisation | Canonical provider runtime events | Canonical `Envelope`/`Event` set (`crates/agent-core/src/event.rs`), raw frame kept, `Unknown` catch-all so the stream never aborts |
| Thread ownership | AppThread > Run > ExecutionNode tree > ProviderThread; app ids primary | `threads` plus N `provider_threads`, one active; native ids are references (`crates/agent-kit/src/store.rs`) |
| Source of truth | Event log; orchestrator commits events, projections, receipts and an outbox in one transaction; V1 importer | SQLite event log, scrubbed on write (`store.rs:157 scrub_envelope`), schema v3 with stepwise migrations (`store.rs:32`). No outbox or projection layer: the view replays events |
| Transport | HTTP/WS RPC between server and clients; relay for remote | In-process; agent subprocesses over stdio NDJSON. agy approvals over a unix socket (`src/approval_server.rs`) |
| Persistence | SQLite (node:sqlite) | SQLite (rusqlite) at `$XDG_STATE_HOME/agent-terminal/threads.db`; config in `config.json` (atomic writes) |
| Packaging | Stable, nightly (30 min) and preview channels; DMG, AppImage, .deb, NSIS; Node SEA CLI, npm, AUR, Homebrew, winget; web on Vercel | One .deb via `cargo-deb` with `$auto` dependencies, built by the `debian-maintainer` apt pipeline; a `beta` apt suite for the 3.0 branch (`plans/2026-10-06_v3-structured-agents.md`, "Branch and releases") |

## 3. Feature matrix

"n/v" = not verified by the T3 inventory.

| Feature | T3 Code | agent-terminal 3.0 | Notes |
|---|---|---|---|
| Agents supported | Codex, Claude, Cursor, Grok Build, OpenCode, Pi, Antigravity, generic ACP registry; multiple accounts per driver | Claude, agy (Antigravity CLI), Codex; any other CLI as a terminal thread | Gemini is reached only through agy. Codex adapter is built from upstream protocol source, not yet exercised live (section 7) |
| Model switching in-thread | First class (ProviderSwitchService); Claude restarts with resume | Same-agent switch: Claude applies `set_model` on the next turn with no restart; agy restarts and resumes; effort change forces restart (`transition.rs:70 plan_selection`) | `set_model` verified live in Phase 0 |
| Cross-agent handoff / "continue in" | Handoff service; fork plus merge-back with lineage | Switch agent inside a thread with a budgeted, redacted summary; a divider marks it; `Continue in` on rate limit; fork carries history (`README.md`, "One model picker") | No merge-back or lineage. Verified live: agy to Claude carried 9 messages |
| Handoff budgeting and redaction | 16k default (1,024 to 64,000), 64 KB ceiling, 128k fallback window, reserve max(16k, 25 %), strategies `delta_since_target_last_seen`, `full_thread_summary`, `checkpoint_summary`, `manual_context`; omitted history readable via `t3_thread_read` MCP tool. Redaction: n/v | Ported budget with the same constants (`handoff_budget.rs:21-33`, `:121 handoff_budget`, `:191 select_history`); every handoff, brief and stored event goes through `redact.rs:65` | Ours has one strategy (budgeted selection) and no `thread_read` tool, so omitted history is simply gone (noted in the module header, `handoff_budget.rs:12-14`). Mandatory redaction is ours (`AGENTS.md`, "Every persisted or injected text is redacted") |
| Slash-command typeahead | `/` commands, `$` skills, `@` files, `#` PR mentions in a Tiptap composer | `/`, `@`, `$` in a GtkSourceView composer (`src/chat/view/typeahead.rs`, `composer.rs`); registry and ranking in `crates/agent-core/src/commands.rs` | Ranking ported from T3. No `#` PR mentions. Terminal-only commands are hidden |
| Approvals UI | Supervised, Auto-accept edits, Auto, Full access (default); per-project overrides | Inline card with Allow, Allow for session, Deny, with the edit's diff; modes Ask, Accept edits, Plan (`adapter.rs:113 Mode`) | No Full access or provider auto-review mode, and no per-project override (n/v whether we want one). Agy gating is verified, see below |
| Plan / accept-edits modes per agent | `/plan`, plan and todo items, runtime modes; Antigravity has no plan mode | Claude `set_permission_mode`; Codex `approvalPolicy` plus `sandboxPolicy` (`codex.rs:1125 mode_policy`); agy `--mode plan\|accept-edits` (`agy.rs:412`) | **agy Plan is not read-only** without the hook (verified live; section 7). Plan panel above the composer |
| Usage / quota and account indicators | Transcript readers for six providers with a cost dashboard, subscription-limit widgets, "Limited" threads with resume-at-reset or auto-resume | Per-turn `QuotaUpdated` for each agent, usage popover with account, shared sidebar footer (`crates/agent-core/src/quota.rs`, `src/account_status.rs`, `src/chat/view/usage.rs`); rate limit offers "Continue in" | No cost dashboard and no auto-resume at reset. Codex usage is not exposed (`caps.rs:100`) |
| Model catalogue and retired-model porting | Model manifest with custom models; retired-model handling: n/v | Live catalogue per agent, cached, one searchable picker (`crates/agent-core/src/catalog.rs`, `src/model_catalog.rs`); a retired model gets a suggested replacement of the same family, effort kept (`catalog.rs:314 suggest_replacement`, `:356 model_notice`; banner `session.rs:60`) | Only a fresh list can call a model retired, never the cache (`catalog.rs` test `only_a_fresh_list_can_call_a_model_retired`). No custom-model entry |
| Dynamic agent availability | Providers must be installed and authenticated; managed ACP installs and self-update | Registry-driven detection and a Ready state; a thread never spawns an agent that is not Ready (`src/availability.rs:57 classify`, `:95 unavailable_banner`; commit `134957dd`) | We never install or update agents |
| Diff viewer | DiffPanel (@pierre/diffs), ReviewService, review comments as composer chips, per-revision viewed state | Per-file diff cards against the pre-turn baseline with the agent's own edit as fallback (`crates/agent-kit/src/filediff.rs`, `editdiff.rs`, `src/window/imp/diffs.rs`); repo diff panel on `Ctrl+Shift+D` (`src/window/diff_panel.rs`) | No review comments and no viewed state |
| External diff tool | n/v | Argv template with presets and placeholders, never a shell, run detached off the main thread (`crates/agent-kit/src/difftool.rs`, `src/diff_tool.rs`) | |
| Checkpoints / undo | Hidden git-ref checkpoints, rollback, "Edit from here"; Codex native rollback | Turn checkpoints in hidden refs, restore that pins the current tree first so undo is undoable (`crates/agent-kit/src/git.rs`, `restore.rs`); `/rewind` | No "Edit from here" (n/v for exact T3 semantics) |
| Worktrees | Worktrees directory with setup scripts; repo create, clone, publish | Create branch plus worktree, clean-only removal, never `--force` (`crates/agent-kit/src/worktree.rs`) | No setup scripts, no repo create/clone/publish |
| Terminal | node-pty with libghostty WASM, agent shell streams, terminal excerpts as composer chips | VTE drawer per thread (`` Ctrl+` ``) and full Terminal threads for any CLI | No terminal-to-composer chips |
| Multi-window / remote / web access | Web app, Electron, T3 Connect (Clerk, tunnels), LAN/Tailscale pairing, SSH, WSL, multi-route failover | Multiple windows of one GTK app; no remote, no web (`AGENTS.md`: "Localhost bound ... exposes no ports") | Deliberate. See section 4 |
| Mobile | iOS and Android (Expo), push notifications, share sheet, voice input (iPhone) | None | Deliberate |
| Subagents / background tasks | Agents panel, orchestrator MCP `delegate_task` across providers, scheduled tasks | Subagent cards nested by parent tool use (design in `v3-structured-agents.md`); no cross-provider delegation or scheduler | The nesting rendering was designed; live exercise of it is n/v |
| Git forges and PRs | GitHub, GitLab, Forgejo/Gitea, Bitbucket, Azure DevOps; PR stacks, watch/sync, templates | None | Out of scope |
| MCP | HTTP MCP endpoint with about 70 tools, injected per provider | `/mcp` panel for Claude only (`caps.rs:51`); we expose no MCP server | |
| Composer extras | Up to 100 attachments, HEIC, queue or steer follow-ups, prompt stash, ArrowUp recall, citations, voice | Typeahead, Enter to send, Esc to interrupt. Attachments, queueing, stash: not implemented (not found in the sources read) | |
| Theming / icons | Custom themes, VS Code and Open VSX import | Seven terminal colour schemes; one dark chrome palette with per-agent accents; icons bundled as a GResource with `at-*` names (`THIRD_PARTY.md`, `tests/icons.rs`); dark-only | |
| Secret handling | Antigravity API key stored in plain text in environment settings (T3 doc-claim) | Redaction on every persisted and injected text; briefs 0600 in a 0700 directory; agent env cleaned (`AGENTS.md`, "Security & Robustness") | Pattern-based, so an unrecognised secret format can pass; the brief says so (`README.md`) |
| Tests | Replay-backed integration tests with real orchestrator and adapters, per-provider testkits, live tests, perf and desktop smoke tests, knip, oxlint; about 534k test lines | Replay fixtures (`crates/agent-core/tests/fixtures/*.ndjson`) fed through the pure adapters, git tests on real temp repos, store and restore tests, GTK construction smoke test, CI fmt + clippy + tests under Xvfb. Roughly 600 `#[test]` functions by a text count | Live provider tests are manual smoke checks, not `cargo test` (`v3-structured-agents.md`, "Tests never touch live state") |

## 4. What we ported from T3, and where we diverged

### Ported (MIT notice in `THIRD_PARTY.md`)

| T3 idea | Our file | Change |
|---|---|---|
| Session transition policy | `crates/agent-core/src/transition.rs:94 decide_transition`, `:70 plan_selection` | Instance and continuation checks collapse into a driver comparison (module header, lines 8-10) |
| Context-handoff budget | `crates/agent-core/src/handoff_budget.rs` | No wire-format cost, no thread/run ids, no `thread_read` pointer; redaction added |
| Slash-command ranking | `crates/agent-core/src/commands.rs` | Same scoring scale; built-ins shadow agent commands |
| Canonical event stream | `crates/agent-core/src/event.rs` | Smaller set, with `Unknown` fallback |
| Capability matrix | `crates/agent-core/src/caps.rs:45,64,85` | Fourteen booleans, values from verified probes, the Codex row from upstream source |
| One adapter per agent | `crates/agent-core/src/{claude,agy,codex}.rs` | Pure state machines, replayable without a process |
| Approval modes | `codex.rs:1125 mode_policy` (notes "T3's `approval-required` row, `auto-accept-edits` row") | Three modes, not four |

### Deliberate divergences

| Divergence | Reason |
|---|---|
| No TypeScript, Node or Electron; one native binary | Standalone Philosophy (`AGENTS.md`); T3's own cost is 475 MB of checkout and a Node runtime |
| Drive `claude` over its stream-json control protocol, not the Agent SDK | Same wire the SDK uses; no Node dependency; `set_model` works in session, so T3's restart on every switch is unnecessary for Claude (Phase 0 results) |
| agy through its local stream-json CLI plus our PreToolUse hook, not Google's ACP bundle | The bundle is about 926 MB and unpacks about 1 GB per launch; the local CLI reuses the existing Google login; no replaying agy's OAuth token (account-ban risk, user-agreed) |
| Approval gating is fail-closed and proven before `--dangerously-skip-permissions` is used | agy runs tools unprompted otherwise (`src/approval_server.rs:521 bind_checked`, `src/hook_config.rs:79 check_installed`; a canary restarts agy without the flag if a tool arrives with no hook query) |
| Redaction is mandatory on every handoff and stored event | A handoff replays old command output, where credentials hide |
| No remote, relay, mobile, PR watchers or scheduler | Out of scope in the plan ("Scope creep toward T3 parity"); each needs hosted infrastructure or a server |
| Windowed `GtkBox` transcript instead of `GtkListView` | `ListView::scroll_to` needs GTK 4.12 (floor is 4.10) and recycling rebuilds markdown widgets (`src/chat/view/transcript.rs`, spike verdict in the progress log) |
| SQLite event log without an outbox or projection layer | A single local process needs no effect worker; view replays from the store |
| 2.x terminal kept as a drawer and as Terminal threads | Any CLI without an adapter still works; chat is the default, with no setting to restore terminal tabs |
| External diff tool and retired-model porting | Our additions; neither is established in the T3 inventory |

## 5. Pros and cons

### T3 Code

| Pros | Cons |
|---|---|
| Eight provider families plus a generic ACP registry with SHA-256-verified installs; several accounts per driver | About 823k non-test lines; the orchestration layer alone is about 102k. Large to audit or fork |
| Web, desktop and mobile clients; remote access through T3 Connect, Tailscale, SSH and WSL | Remote and mobile depend on a hosted Cloudflare relay and Clerk |
| PR and forge integration across five forges; scheduled tasks; orchestrator MCP with about 70 tools | v0.0.45, desktop labelled Alpha, contributions mostly closed |
| Event-sourced core with replayable state and a V1 importer; extensive replay-backed tests | Persisted event schemas must stay decodable across versions: a permanent compatibility burden |
| Rich composer (attachments, queue or steer, stash, voice) and review comments | Antigravity path needs about 926 MB plus 130 MB, "several GB free disk"; no rewind, no plan mode, no custom models there |
| Managed provider updates, cross-platform installers, three release channels | Needs Node 22.16+/24 from source; providers must be installed and authenticated separately; Wayland shortcut quirks on Electron |
| Cost and usage dashboard across six providers | Handoffs do not copy reasoning or tool state; token budgets are estimates; Antigravity API key in plain text (doc-claim) |

### agent-terminal 3.0

| Pros | Cons |
|---|---|
| Small and native: about 52k lines of Rust, one .deb, no Node or Electron, no hosted dependency, no ports | Three agents only (Claude, agy, Codex); anything else is a terminal thread |
| Pure adapters replayed from recorded fixtures: protocol changes are testable without a process or display | Rests on undocumented control protocols (Claude control requests, agy stream-json input) that change with CLI versions; mitigated by pinned fixtures, not eliminated |
| Mandatory redaction, fail-closed approval hook, canary, atomic config writes, undoable restore, never-force worktree removal | Linux and GTK only; no web, mobile or remote access |
| Real terminal drawer and Terminal threads keep 2.x workflows | No PR, forge, scheduling or cross-provider delegation features |
| Native panels and typeahead backed by agent controls; live model catalogue with retired-model porting | Handoff keeps one strategy and no `thread_read` tool, so omitted history is lost to the receiving agent |
| External diff tool, per-turn baseline diffs and checkpoint refs that never touch the user's git state | agy cannot ask before acting without the user installing a hook in `~/.gemini/config/hooks.json` |
| Shipped via the existing apt pipeline with a beta suite | Single developer; the Codex adapter is unproven live; composer lacks attachments and queueing |

## 6. Gaps worth closing next (ours), ranked

1. **Exercise Codex live.** The adapter and its capability row come from upstream source and synthetic fixtures
   (`caps.rs:83-85`); one real login and turn turns an assumption into a verified fact.
2. **Agy hook onboarding.** Without the hook agy applies edits in every mode; a one-click, consented install
   (with the existing atomic write) removes the biggest safety caveat in the app.
3. **Handoff `thread_read` equivalent.** Omitted history is currently unrecoverable; a small MCP or file the
   receiving agent can page from addresses T3's main handoff advantage.
4. **Fork and merge-back with lineage.** We have fork-with-history but no way to bring results back to the
   parent thread.
5. **Attachments and follow-up queue in the composer.** The most visible day-to-day gap against T3's composer.
6. **Auto-resume after a rate limit resets.** `QuotaUpdated` already carries reset times (`quota.rs`); a
   "resume when available" action is small by comparison with its value.
7. **Review comments on diffs.** A diff card already exists; sending a line comment back as a composer chip
   closes the review loop.
8. **Per-project mode defaults and a Full access mode.** Matches T3's runtime modes; low cost, but only if wanted.
9. **Usage history and cost view.** We show live windows only; a stored history is feasible from the event log.
10. **Custom model entry.** Useful for agents whose lists lag; low priority.

Remote access, mobile, PR watching and a scheduler stay deliberately out of scope; each carries hosted
infrastructure or a server we chose not to run.

## 7. Known caveats from the live smoke test (2026-10-06, `plans/2026-10-06_v3-progress.md`)

- **Setup:** release build through `preview_app`, real Claude and agy, scratch repository. Worked live: usage
  bars, the full model picker with effort, a Claude edit behind an approval card with its diff, an agy
  read-only turn, and an agy to Claude handoff carrying 9 messages.
- **Approval card fix.** The card stuck at "Allow..." because Claude and Codex never acknowledge an answer;
  `respond_approval` now resolves the card once the adapter accepted and wrote it.
- **Other fixes from the same run:** switching agents no longer shows "Turn failed: exited unexpectedly"; an idle
  agy no longer shows "Working..." forever (it prints `init` on spawn); fresh installs no longer get a
  standalone Gemini profile.
- **agy plan mode is not read-only headless.** Verified with a control, in both `-p` and stream-json: agy writes
  its plan, then applies the edits itself while its reply says it only planned. Shell commands are denied in
  every mode without `--dangerously-skip-permissions`; edits are not gated in any mode. Only the approval hook
  gives agy a real ask-before-edit. The earlier "forced plan" fallback was false safety and was removed; the mode
  dropdown now sets agy's own flags and the thread says so (`README.md`, "agy's approval hook").
- **Codex not exercised live.** The user is not logged in to Codex, and `codex` was not installed during Phase 0.
  Its adapter, catalogue and probes are covered by unit tests and synthetic fixtures only.
- **Unexplained, not reproduced:** one "Allow for session" decision on the first run, most likely stray Broadway
  input. Unverified.
- **Not covered by the smoke test:** launching the external diff tool (the approval card's diff and the
  edit's "View diff" were exercised), subagent nesting under real load, and the beta apt channel deploy
  (gated on user approval).
