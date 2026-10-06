# T3 Code feature inventory (research input for the comparison write-up)

**Source:** a read-only review of T3 Code `pingdotgg/t3code` at server version **0.0.45**, cloned 2026-10-06.
- Licence: MIT, "Copyright (c) 2026 T3 Tools Inc."
- Nothing from the clone was executed.
- Paths are relative to the T3 repo root.
- "Doc-claim" means the item comes from T3's docs only.

## 1. Architecture and stack

- **Monorepo:** pnpm 11 + Vite+, Node ^24.13 (server: Node ≥22.16). TypeScript throughout with
  Effect-TS.
- **apps/server:** the `t3` CLI plus an HTTP/WS RPC server. It owns provider processes, terminals, git
  and files; clients are thin.
- **apps/web:** React 19, TanStack Router, a Tiptap composer, Tailwind 4, zustand, mermaid and
  @pierre/diffs. Hosted at app.t3.codes.
- **apps/desktop:** Electron 44 (electron-updater, keyring, Clerk, ffi-rs, playwright-core), bundling
  the server.
- **apps/mobile:** Expo/React Native for iOS and Android.
- **Packages:** contracts, client-runtime, shared, effect-acp, effect-codex-app-server (largely
  generated), ssh, tailscale.
- **native/:** a Rust resource monitor and libghostty-vt.
- **infra/relay:** Cloudflare Workers, Postgres and Durable Objects (Alchemy).
- **Persistence:** SQLite (node:sqlite) with migrations.

## 2. Providers and how each is driven

| Provider | How T3 drives it | Notes |
|---|---|---|
| Codex | `codex app-server` JSON-RPC | managed home, ChatGPT OAuth, multi-account, native rollback, /goal |
| Claude | `@anthropic-ai/claude-agent-sdk` | usage limits and reset credits |
| Cursor | `@cursor/sdk` | |
| Grok Build | ACP | |
| OpenCode 1.x/2.x | SDK; one server per thread or per instance | |
| Pi | RPC mode with an injected MCP extension | |
| Antigravity | Google's official ACP bundle, downloaded and managed by T3 | 4 sign-in methods |
| ACP Registry | generic ACP agents | SHA-256-verified installs |

Provider *instances* allow several accounts per driver. Providers self-update, and a model manifest
supports custom models.

## 3. Threads, projects and event sourcing

- **Model:** AppThread > Run > ExecutionNode tree > ProviderThread. App ids are primary; provider ids
  are references.
- **The event log is the source of truth.** Orchestrator → EventSink commits events, projections,
  command receipts and an effect outbox in one transaction. EffectWorker runs side effects,
  ProjectionStore projects, and ThreadStream streams snapshot+cursor. A V1 importer migrates old data.
- **Projects** are optional (scratch folders), with a `t3.json` project file and project scripts.
- **Thread organisation:** pin/reorder, snooze, settle and auto-settle (inactivity or PR merge),
  archive, regenerated titles, search, and multi-model fan-out with Shift-click.

## 4. Model/provider switching and context handoff

- **Switching is first class:** ProviderSwitchService, SelectionTransition, ContextHandoffService and
  Delivery.
- **Strategies:** `delta_since_target_last_seen | full_thread_summary | checkpoint_summary |
  manual_context`.
- **Budget:** 16k tokens by default (clamped 1,024–64,000), a 64 KB ceiling, 1 byte = 1 token, a 128k
  fallback window, reserve max(16k, 25 %), and image/attachment allowances. Omitted history is
  available through the `t3_thread_read` MCP tool. Codex gets native historical messages.
- **Also:** fork plus merge-back with lineage, `/compact`, and restart continuation.

## 5. Approvals, runtime modes and sandbox

- **Modes:** Supervised, Auto-accept edits, Auto (provider auto-review), and Full access (the
  default). Per-project overrides.
- **No T3-native sandbox:** enforcement is the provider's, via a pass-through `sandboxPolicy`.

## 6. Plans, todos and questions

- Plan and todo graph items, `/plan`, and persisted async questions (`user_input_request`) with
  attachments.
- `/goal` for Codex and Claude.

## 7. Subagents and background tasks

- A subagent projection with an Agents panel; stop cascades to children.
- Orchestrator MCP `delegate_task`/`task_status`/`task_cancel` creates T3 child threads on any
  provider.
- A background-work policy and a host power monitor.

## 8. Checkpoints, rollback, fork and diff

- Hidden git-ref checkpoints, capture/rollback/restore safety, and "Edit from here".
- **Diff and review UI:** DiffPanel with @pierre/diffs, ReviewService, review comments as composer
  chips, and per-revision viewed-state for PR files.

## 9. Git, worktrees, PRs and source control

- **Forges:** GitHub, GitLab, Forgejo/Gitea, Bitbucket and Azure DevOps.
- **PR features:** multi-PR links and stacks, PR watch/sync reactors, and template detection.
- **Worktrees:** a worktrees directory with setup scripts.
- **Repos:** create, clone and publish.

## 10. Terminals

- A node-pty manager rendered with libghostty WASM, ACP client terminals, agent shell streams, and
  terminal excerpts as composer chips.

## 11. Composer

- **Editor:** a Tiptap composer with `/` commands, `$` skills, `@` files and `#` PR mentions, plus
  terminal, review and preview chips.
- **Attachments:** up to 100 files (images 10 MiB; 80 MiB total), HEIC conversion.
- **Follow-ups:** queue or steer, with queue editing.
- **Extras:** prompt stash, ArrowUp recall, citations, voice input (iPhone), and a mobile share sheet
  with an offline queue.

## 12. Usage, quota and auth

- **Usage:** transcript readers for six providers (a cost/pricing dashboard), subscription-limit
  widgets, and "Limited" threads with resume-at-reset or auto-resume.
- **Auth:** provider auth flows (Codex ChatGPT, Antigravity Google, ACP sign-in), with Clerk for T3
  Connect.

## 13. MCP

- An HTTP MCP endpoint (`t3-code`) with about 70 tools: orchestrator, thread
  read/send/fork/merge-back, worktree, PR, project, preview/browser automation, devices, attachments
  and scheduling.
- Injected per provider.

## 14. Other

- **Notifications:** desktop, plus mobile APNs/FCM and iOS Live Activities through the relay.
- **Scheduled tasks:** interval, fixed time and webhook, with the relay holding webhooks up to 24 h.
- **Navigation and settings:** a command palette, cross-environment search, JSON keybindings with
  when-clauses, and per-environment/project settings.
- **Themes:** custom themes, plus VS Code and Open VSX theme import.
- **Extras:** an in-app browser preview with element picking, SnapShot screen capture, HTML replies,
  OpenTelemetry, and telemetry.

## 15. Remote and mobile

- **Remote access:** T3 Connect (Clerk, managed tunnels, DPoP), LAN/Tailscale pairing, SSH
  environments, WSL, and multi-route failover.
- **Service and apps:** a `t3 service install` background service, and iOS/Android apps that need a
  reachable host.

## 16. Testing

- **Replay-backed integration tests:** a real orchestrator, adapters and stores; only transport,
  clock and FS are substituted.
- **Per provider:** testkits plus live tests, and `record:*-replay` scripts.
- **Also:** perf tests, a desktop smoke test, knip and oxlint.

## 17. Packaging and release

- **Channels:** stable, nightly (every 30 min) and preview.
- **Desktop:** DMG (macOS arm64/x64), AppImage and .deb (Linux x64/arm64), NSIS (Windows x64/arm64).
- **CLI:** Node SEA archives, npm packages, and curl|sh installers.
- **Other distribution:** AUR, Homebrew and winget; the web app on Vercel.

## 18. Code size (non-test .ts/.tsx)

| Part | Lines |
|---|---|
| apps/server | 258,296 (orchestration-v2 102k, provider 40k, pullRequest 21k) |
| apps/web | 240,744 |
| apps/mobile | 110,798 |
| apps/desktop | 41,360 |
| packages | 171,011 |
| **Total** | **≈ 822,600 non-test lines** |

Test lines total ≈ 534k (server tests alone 274k), and infra/relay adds 15k. The checkout is 475 MB.

## 19. Evidenced limitations and costs

- **Antigravity runtime:** about 926 MB plus 130 MB on linux-x64; "several GB free disk". It cannot
  rewind, plan mode is unavailable, and there are no custom models.
- **Electron desktop:** native modules run in child processes; there are Wayland shortcut quirks; the
  product is still labelled "(Alpha)".
- **Runtime:** Node 22.16+/24 from source. Providers must be installed and authenticated separately
  (except the managed ACP ones).
- **Remote and mobile depend on hosted infrastructure:** a Cloudflare relay plus Clerk. The Antigravity
  API key is stored in plain text in environment settings (doc-claim).
- **Compatibility burden:** persisted event schemas must stay decodable across versions, and the V1
  importer is kept.
- **Uneven provider capabilities:** handoffs do not copy reasoning or tool state, and token budgets
  are estimates.
- **Maturity:** v0.0.45, "very very early", contributions mostly closed.
