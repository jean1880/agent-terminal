# Agent Terminal: Developer Guidelines

Architectural mandates, standards, and workflows for this codebase.

## 🏗️ Architectural Mandates

- **Standalone Philosophy**: a single, standalone binary. All assets (SVG, icons)
  are embedded via `include_str!`/`include_bytes!`. Do NOT introduce external
  runtime dependencies.
- **Native GTK4/Libadwaita**: a native Rust binary. Minimum system libraries are
  set by the Cargo version features, currently GTK 4.10 (`v4_10`, for
  `FileDialog`), libadwaita 1.5 (`v1_5`, for `AlertDialog`/`PreferencesDialog`)
  and VTE 0.72 (`v0_72`, for `text_range_format`). Raising any of them is a
  deliberate decision, not a side effect.
- **No redundant GTK crates**: `glib`, `gio` and `gdk4` are reached through the
  `gtk4` re-exports (`gtk4::glib`, …). Do not add them as direct dependencies —
  that only creates a second place for versions to drift.
- **Chat-first (3.0)**: the primary surface is a chat thread (`src/chat/`) that
  drives Claude or agy through a structured adapter (`agent-core`) over a gio
  process transport (`src/agent_proc.rs`). Every new-session action opens a
  thread. Terminal pages (2.x) remain for any profile and for CLIs with no
  adapter.
- **PTY Bridge**: terminal pages and a thread's drawer use `vte4`, running the
  configured CLI or the user's `$SHELL`.
- **PTY Isolation**: sessions inherit the user's interactive environment (`-ic`)
  and full environment variables so tooling (nvm, aliases) stays available.
  `TERM`/`COLORTERM` are forced so the CLI renders its full TUI.
- **`exec`, and nothing before it**: the startup command is `exec <cli>` so the
  CLI owns the controlling terminal. Never emit banner or `clear` output into the
  TTY beforehand — it disrupts the CLI's initial terminal handshake. Anything
  resembling a startup script must contribute *environment*, merged into the
  spawn environment, never bytes written to the terminal.
- **Threads over a hidden tab view**: the window is an `adw::OverlaySplitView`
  whose sidebar lists the store's threads. Its content is an `adw::TabView` with
  no tab bar: one page per open thread or terminal. Each page is tracked in an
  explicit `TabState` registry (`src/window/imp.rs`); a thread page has
  `chat: Some(ChatTab)` and its `terminal` is the drawer's shell. Never look a
  page up by walking the widget tree. A page's directory is fixed for its
  lifetime. A thread is built (view replayed from the store, then its
  `ChatSession` started with `view.sink()`) only when it is first shown.
- **Module layout**: a Cargo workspace. The root package is the GTK app, so
  `cargo build --release` and `cargo deb` at the root still produce the one
  shipped binary; `crates/*` are path libraries with no GTK.
  - `crates/agent-core` (pure: no GTK, no process or file I/O): `redact`
    (secret masking applied to every hand-off brief, every injected handoff
    and every persisted event), `event` (the canonical provider-neutral event
    stream), `caps` (per-agent capability matrix), `adapter` (the sans-I/O
    adapter contract) and the per-agent adapters and pure policies built on it.
  - `crates/agent-kit` (GTK-free; blocking process and file I/O): `git`
    (git plumbing for turn checkpoints and diffs: shells out, but its parsers
    and filters are pure), `diff` (diff bases, numstat, line classification,
    truncation), `restore` (undo: pin, restore, contained deletion, check),
    `worktree` (worktree locations, branch checks, create and clean-only
    remove), `handoff` (hand-off briefs, session readers, quota detection),
    `sessions` (`SessionFormat`, session IDs, transcript lookup and listing),
    `paths` (`~` expansion) and `exec` (child processes with a timeout).
  - `agent-kit`'s `store` is the SQLite thread store (threads, provider
    threads, scrubbed events, app meta such as the open-thread list).
  - The app (`src/`): `config.rs` (persisted settings + migration, re-exporting
    `SessionFormat`; per-agent defaults live on the 2.x profiles), `theme.rs`
    (colour schemes, infallible `RGBA::new`), `utils.rs` (detection and
    `SystemProbe::locate`, startup command, path resolution, indicators; it
    re-exports the kit helpers the window uses), `window/imp.rs` (GTK UI),
    `window/imp/threads.rs` (sidebar, thread pages, drawer, re-homed 2.x
    features), `window/imp/agents_prefs.rs` (Settings → Agents),
    `window/sidebar_model.rs` (pure sidebar logic), `window/diff_panel.rs` (the
    diff panel's widgets), `main.rs` (app setup + logging, and the `agent_kit`
    imports that keep `crate::git::…` paths).
  - Chat: `chat/session.rs` (one thread's backend: adapter, process, store,
    switching, handoff), `chat/view/` (the GTK view; it only talks to
    `ChatBackend`), `agent_proc.rs` (gio process transport),
    `approval_server.rs` / `approval_hook.rs` (agy's approval socket and the
    `--approval-hook` client), `hook_config.rs` (is the hook installed in
    `~/.gemini/config/hooks.json`), `model_catalog.rs`, `account_status.rs`,
    `claude_probe.rs` (both agents' models, usage and account).
  - Anything that needs `config.rs`, GTK or the command cache stays in the
    app; a crate never depends on the app.
- **Undo is itself undoable, and only touches what it pinned**: a restore
  always pins the current working tree first (even if unchanged), refuses to
  run if the tree moved since that pin, never touches HEAD or the index, never
  deletes outside the toplevel or through a symlinked directory, and leaves
  files checkpoints skip alone. `restore::tests` proves each of these.
- **Live GUI checks go through the preview MCP's `preview_app`**, never
  hand-run `gtk4-broadwayd`/`dbus-run-session` shells. It isolates the D-Bus
  session and XDG homes, and cleans up after itself.
- **Worktrees are never removed by force**: removal is offered only for a
  clean worktree that no open tab is in, runs `git worktree remove` without
  `--force`, and never deletes the branch. Names reaching git are refused if
  they start with `-`, and paths follow `--`.
- **Checkpoints never touch the user's git state**: snapshots go through a
  private index file under the git dir, and are recorded only as refs under
  `refs/agent-terminal/`, created with an empty old value so none is
  overwritten. The user's index, HEAD, working tree and stash are left
  exactly as they were; `agent-kit`'s `git::tests::repo` proves it, including with a split
  index and in a linked worktree. Every git call strips inherited
  `GIT_DIR`-style variables and sets `GIT_OPTIONAL_LOCKS=0`. Untracked files
  whose names look like secrets are never captured.
- **Hand-off briefs are the terminal's, not the CLI's**: a CLI out of quota
  cannot summarize itself, so briefs are built from disk. Every brief goes
  through `agent_core::redact::redact` and is written `0600` in a `0700` directory outside
  any project. Never add a brief source that skips either.
- **No feature flags for deployment specifics**: the crate has none. The
  Ansible-drift indicator used to sit behind `homelab-drift`; it is now one
  possible entry in the config-declared `indicators` list, which honours the
  standalone philosophy more strictly than a `#[cfg]` did — anyone gets the same
  capability, and the shipped binary carries no homelab knowledge at all.

## 🔒 Security & Robustness

- **agy never runs with `--dangerously-skip-permissions` unless the hook is
  proven**: the flag is passed only with an approval socket bound after
  `hook_config::check_installed` accepted the hooks file (`ApprovalHandle::
  bind_checked`, given the verdict of the off-main-thread `check_hook`; a
  verdict not yet in is an `Err`). Without it, agy is forced to `--mode plan`. The adapter strips
  the flag from profile arguments. The session's canary restarts agy read-only
  if a tool step arrives without a hook query. Never add a path that sets the
  flag any other way.
- **Every persisted or injected text is redacted**: the store scrubs every
  event (`scrub_envelope`) and title, and every handoff (cross-agent switch,
  fork, compact-by-handoff, brief) goes through `agent_core::redact` and the
  handoff budget. A new store write or injected prompt must take the same path.
- **Agent processes get a cleaned environment**: `clear_env` and any inherited
  approval-socket variables are removed (`SpawnSpec::unset`), then the profile's
  env file, then the approval socket.
- **Localhost bound**: interacts only with the local shell; exposes no ports.
- **Never block the main thread**: anything that shells out — CLI detection above
  all, which may run `$SHELL -ic` and source the user's rc — goes through
  `gio::spawn_blocking`. A frozen window cannot even repaint its own spinner.
- **Graceful error handling**: avoid `.unwrap()`/`.expect()` in runtime paths.
  Surface failures to the user rather than dying.
- **A dead session is not a reason to destroy UI**: a non-zero exit keeps its tab,
  its scrollback and its error message. Only a clean exit closes a tab.
- **Reference integrity**: GTK signal handlers use weak references
  (`glib::clone!` with `#[weak]`) to avoid reference cycles.
- **Config writes are atomic**: write to a temp file, `sync_all`, then rename.
  Never write over a live config in place.
- **Observability**: logging goes to the systemd journal (identifier
  `agent-terminal`) and to stderr when attached to a terminal, with a panic hook.
  Keep meaningful coverage on spawn, detection, config and tab lifecycle paths.

## 🧪 Testing Strategy

- **Logic separation**: keep pure logic in `agent-core`, and GTK-free git and
  session logic in `agent-kit` (`git`, `diff`, `restore`, `worktree`,
  `handoff`, `sessions`); the app's `src/utils.rs` and `src/config.rs` hold the
  rest. All of it is decoupled from GTK so it is unit-testable without a
  display. `agent-kit`'s repository tests run real git in temp repos with a
  hermetic config (CI has no git identity) and skip themselves when git is
  absent. `window/imp.rs` is covered by a construction smoke test plus tests
  for any pure helpers in it.
- **Guard hand-maintained arrays**: `CliClient::ALL`, `ThemeChoice::ALL` and
  `DiffBase::ALL` drive dropdowns by index in both directions. All have exhaustive-match
  tests so adding a variant fails to compile until the array is updated. Any new
  such array needs the same treatment.
- **Update tests alongside** any change to detection, startup-command,
  path-resolution, config-serialization or migration logic.

## 🛠️ Development Workflow

1. **Environment**: `make deps`.
2. **Build & run**: `cargo build`; `make start-local`.
3. **Running a local build**: GApplication is single-instance, keyed on the app
   ID. If the packaged build is already running, launching your build hands the
   activation to *that* process and yours exits immediately — the journal then
   shows a plausible startup that has nothing to do with your code. Always use
   `dbus-run-session -- ./target/debug/agent-terminal`. Do not kill the running
   instance to work around this; it is someone's live session.
4. **Linting**: `cargo fmt --all` and `cargo clippy --workspace --all-targets -- -D warnings`.
   CI enforces fmt, clippy, and tests under Xvfb.
5. **Installation**: do **NOT** run `make install` on developer/daily-driver
   machines. Deployment is the `debian-maintainer` apt pipeline
   (`apt.nuvek.ca`), which builds every pushed commit. A manual `make install`
   shadows the apt-delivered package in PATH, so the machine silently keeps
   running a stale build. If you need it for isolated testing, run
   `make uninstall` immediately afterwards.

## 📦 Packaging

Packaging is `cargo-deb` driven by `[package.metadata.deb]`, both locally
(`make package`) and in the apt pipeline. Runtime dependencies come from `$auto`,
which derives them from the built ELF — do **not** hand-list them alongside it;
that previously produced duplicate entries with weaker bounds and, before the
switch, omitted `libadwaita-1-0` entirely from every shipped package.

The pipeline step in `debian-maintainer/config.yaml` passes `-o .` because the
orchestrator collects artifacts with a non-recursive scan of the workspace root.

## 🤖 AI Contribution Rules

- Always prioritize the **Standalone Philosophy**.
- Terminal colours are user-selectable (`ThemeChoice` in `config.rs`, palettes in
  `theme.rs`); add new schemes there and update the guard test. The window chrome
  follows the brand CSS in `main.rs` (background `#181425`, foreground `#c8c8ff`).
- If you modify startup/detection logic, update and run the tests in `utils.rs`.
- Deployment-specific behaviour belongs in config, not in the binary. Anything
  that reads a path or runs a command particular to one machine should be an
  `indicators` entry rather than new Rust.
- An indicator-style check has three states, never two. "Could not read the
  source" must have its own icon and never collapse into "healthy" — that
  collapse is exactly what made the old drift button report a permanently green
  shield for a file that had not existed in months.
- Settings apply on change, not on dialog close. Rows that can hold an invalid
  value must validate inline and refuse to persist rather than silently falling
  back at spawn time.
