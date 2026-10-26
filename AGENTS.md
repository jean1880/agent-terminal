# Agent Terminal: Developer Guidelines

Architectural mandates, standards, and workflows for this codebase.

## 🏗️ Architectural Mandates

- **Standalone Philosophy**: a single, standalone binary. All assets (SVG, icons)
  are embedded via `include_str!`/`include_bytes!`. Do NOT introduce external
  runtime dependencies.
- **Bundled icons**: All icons are bundled (`assets/icons`, GResource); never rely on the
  system icon theme for a name; the icon test (`tests/icons.rs`) guards it. See the rendering
  note in `THIRD_PARTY.md` before editing an SVG there.
- **Native GTK4/Libadwaita**: a native Rust binary. Minimum system libraries are
  set by the Cargo version features, currently GTK 4.10 (`v4_10`, for
  `FileDialog`), libadwaita 1.5 (`v1_5`, for `AlertDialog`/`PreferencesDialog`)
  and VTE 0.72 (`v0_72`, for `text_range_format`). Raising any of them is a
  deliberate decision, not a side effect.
- **No redundant GTK crates**: `glib`, `gio` and `gdk4` are reached through the
  `gtk4` re-exports (`gtk4::glib`, …). Do not add them as direct dependencies —
  that only creates a second place for versions to drift.
- **Chat-first (3.0)**: the primary surface is a chat thread (`src/chat/`) that
  drives Claude, agy or Codex through a structured adapter (`agent-core`) over a gio
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
    adapter contract), `git_command` (a `git`/`gh` command line read as its operation for the
    approval card: a reading aid, never a gate; the card still shows the command),
    `agent_command` (a shell command that launches an agent CLI, `cross-agent`, `claude -p`,
    `codex exec` or `agy -p`, read from a command line or a shell tool's input, so it can be shown
    as a sub-agent; a reading aid too, though a delegated child's authority refuses what it
    recognises) and the per-agent adapters and pure policies built on it.
  - `crates/agent-kit` (GTK-free; blocking process and file I/O): `git`
    (git plumbing for turn checkpoints and diffs: shells out, but its parsers
    and filters are pure), `diff` (diff bases, numstat, line classification,
    truncation), `restore` (undo: pin, restore, contained deletion, check),
    `worktree` (worktree locations, branch checks, create and clean-only
    remove), `github` (a branch's pull-request state, the compare URL, the unforced push; it also gives
    a thread's subtitle its branch, shown only for a repository on GitHub),
    `handoff` (hand-off briefs, session readers, quota detection),
    `sessions` (`SessionFormat`, session IDs, transcript lookup and listing),
    `paths` (`~` expansion) and `exec` (child processes with a timeout).
  - `agent-kit`'s `store` is the SQLite thread store (threads, provider
    threads, scrubbed events, app meta such as the open-thread list).
    `store_writer` is the one thread that writes it: the app's main thread
    queues every write there through a `StoreHandle` (never writing in place on
    a file store), reads that need their own writes wait on its barrier (and
    are not made at all if it does not answer: never read stale rows), its queue
    is bounded, and failures (refused writes included) are counted and shown.
    Split-secret heals use the in-memory tails of `heal_tail` only when they can
    vouch for every row of the item, else the SQL scan; redaction must never be
    weaker than the scan's.
  - The app (`src/`): `config.rs` (persisted settings + migration, re-exporting
    `SessionFormat`; per-agent defaults live on the 2.x profiles), `theme.rs`
    (colour schemes, infallible `RGBA::new`), `utils.rs` (detection and
    `SystemProbe::locate`, startup command, path resolution, indicators; it
    re-exports the kit helpers the window uses), `window/imp.rs` (GTK UI),
    `window/imp/threads.rs` (sidebar, thread pages, drawer, re-homed 2.x
    features; the sidebar draws from a thread list loaded off the main thread
    and every store read or write on the UI path goes through `store_job`),
    `window/imp/thread_menu.rs` (the sidebar's context menu and its actions;
    the menu itself is data in `window/sidebar_model.rs`), `window/imp/closing.rs` (closing a
    window waits for its agents' real exits with the window up, never by blocking at quit; past
    150 ms it shows the closing screen, drawn by `window/imp/pack_animation.rs`),
    `window/imp/agents_prefs.rs` (Settings → Agents),
    `window/sidebar_model.rs` (pure sidebar logic), `window/diff_panel.rs` (the
    diff panel's widgets and its "Open a PR" bar; it pops out into its own window, keeping its
    place, hidden, as its identity, and docks back when that window closes or its page leaves),
    `main.rs` (app setup + logging, and the `agent_kit`
    imports that keep `crate::git::…` paths).
  - Chat: `chat/session.rs` (one thread's backend: adapter, process, store,
    switching, handoff), `chat/view/` (the GTK view; it only talks to
    `ChatBackend`), `agent_proc.rs` (gio process transport),
    `approval_server.rs` / `approval_hook.rs` (agy's approval socket and the
    `--approval-hook` client), `hook_config.rs` (is the hook installed in
    `~/.gemini/config/hooks.json`), `model_catalog.rs`, `account_status.rs`,
    `claude_probe.rs` / `codex_probe.rs` (prompt-less probes of Claude and
    `codex app-server`: models, usage and account), `probe.rs` (probe targets,
    coalescing, disconnectable listeners). Every probe and side process runs in
    the environment its agent's threads get (`AgentEnv`: profile env file,
    `clear_env`), so the indicator shows the account a thread would use.
  - Anything that needs `config.rs`, GTK or the command cache stays in the
    app; a crate never depends on the app.
- **Diffs and the external diff tool**: `agent-kit`'s `editdiff` (pure line differ and the
  Claude/agy/Codex edit readers), `filediff` (pre-turn baseline per turn, repo containment, private
  temp files under `$XDG_RUNTIME_DIR/agent-terminal/diff`) and `difftool` (config shape, presets,
  per-element placeholder substitution; never a shell). The app's `diff_tool.rs` launches it
  detached off the main thread. A file-change card compares the working file against the state
  recorded when its turn started (`window/imp/diffs.rs`), and falls back to the agent's own edit.
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
- **Uninstall removes everything the app put there, and only that**: `agent-terminal
  --uninstall` (also `make uninstall` and Settings → General) asks whether to remove the
  configuration and data (default yes); `agent_kit::uninstall` decides what that is. Its folders
  are removed only when real, owned by the user and named as the app's. Another tool's file is
  only reverted through what the app recorded (the agy registration journal, the Codex export
  header), never edited otherwise. In repositories only `refs/agent-terminal/*` and
  `<git dir>/agent-terminal` go, never a worktree or branch. It never runs while an instance
  does (the CLI refuses; Settings waits for `app.run()` to return). A package's maintainer
  scripts never touch a home folder: they only say where the data stays. A new place the app
  writes must be added to `uninstall::candidate_folders` or `plan`, with a test.
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
  verdict not yet in is an `Err`). Without it, the mode picker drives agy's own `--mode`
  flag (Plan, Accept edits) and Ask is unavailable, because headless agy cannot ask. Never
  describe hookless agy as read-only: verified live, it refuses shell commands without the flag
  but applies file edits in every mode, Plan included. The adapter strips the flag from
  profile arguments, and with it `--gemini_dir`/`--app_data_dir` (which move every hook file
  away from the `$HOME/.gemini` the proof reads). Any workspace `.agents/hooks.json` or
  `.agents/plugins/<p>/hooks.json`, in the thread directory or an ancestor up to the repository
  root, refuses the proof. Outside a repository (or under a toplevel that is not its ancestor)
  every ancestor up to `/` is checked, the same directories as the delegation guard's
  (`agy_guard::l2_directories`). A cached verdict only binds the socket and drives the UI: every agy
  process the session starts with the flag first waits for a fresh `check_target`, run off the
  main thread right before that spawn (`ChatSession`'s `start_process`; writes are held until it
  is in), and a failed check starts that process hookless. The session's canary restarts agy
  without the flag if a tool step arrives without a hook query. **Residual risk (not closed):** a
  hook file created, or a plugin symlink retargeted, after that fresh check and before agy reads
  its hooks (or while it runs) is not seen by the proof. The canary bounds it: such a hook only
  matters if it displaces the app's hook, and the first watched tool step (command, edit, MCP,
  subagent, web) with no hook query restarts agy without the flag after `CANARY_GRACE` (8 s),
  so that one step can run unasked. Never add a path that sets the flag any other way.
- **Dangerous permissive mode (`Mode::Bypass`) is confirmed, session-only and never a
  default**: picking it (dropdown or `/mode`) always shows the warning first, and Cancel changes
  nothing. No process starts in it (`ChatSession::with_env` applies `Mode::session_safe`), it is
  never stored as the thread's choice (`ModeChosen` keeps the last safe mode), a fork or handoff
  gets the safe mode, a switch to another agent ends it, and Settings never offers it. Natively:
  Claude `bypassPermissions` (entered by a resumed restart, since a live `setMode` is a no-op),
  Codex `never` + `dangerFullAccess`, agy the hook allowing everything; hookless agy refuses it.
  It never propagates to a delegated worker (its task mode stays inside the root's safe mode), but
  while the root is in it, the root's view answers each live worker approval that offers Allow
  (`allow_worker_requests`): once, keeping no rule, through the shelf's own route, so the worker's
  inherited authority is re-checked at the answer. Questions and forms still wait for the user.
  It stays last in the picker, below its divider. The session tests
  `dangerous_permissive_mode_*` prove these.
- **Every persisted or injected text is redacted**: the store scrubs every
  event (`scrub_envelope`) and title, and every handoff (cross-agent switch,
  fork, compact-by-handoff, brief) goes through `agent_core::redact` and the
  handoff budget. A new store write or injected prompt must take the same path.
- **A root hears its sub-agents without polling, and never after a stop**: a child's finished
  run, failure or message to its root is posted in the root's thread as the app's notice and
  starts a turn with it (`delegation/wake.rs`, decided in `agent_core::delegation::wake`):
  redacted before it is cut to `WAKE_BUDGET`, coalesced, held while the root is busy, never sent
  again once a `cross_agent_wait`/`read` page showed it, a notice only after
  `MAX_CONSECUTIVE_WAKES` wakes without the user, and dropped once the user stops the root (until
  the user speaks) or its thread closes. A root in dangerous permissive mode is never woken
  (`ParentState::Unattended`): the news is kept, the thread is told once per new item that it
  waits, nothing retries it on a timer, and it goes, framed the same way, with the user's next
  message, a live one mid-turn included (never a slash command, never beside a handoff), or as
  an ordinary wake once the mode is left. It is spent, and its notice posted, only once that
  message reaches a process (for Codex, only once its thread is up: never into the adapter's
  own queue); a message that never does gives it back, exactly once, unless a stop or close
  discarded it meanwhile.
- **A parent's declared limit on a child is refused, never asked**: `cross_agent_spawn`'s optional
  `capabilities` (`edit`, `exec`, `network`) is all a child may do beyond reading. It only narrows
  the task mode (a plan child can hold `network` only, refused otherwise), a child of a restricted
  worker never holds more than it (none declared inherits it), and it is kept in the launch
  record (an unreadable one restores as none). A call outside it, or an unrecognised tool, is
  denied by `ChildAuthority::judge`, so neither the user's rules nor dangerous permissive mode can
  approve it. With nothing declared the task mode decides, and the user's shell and edit rules
  reach a writing child within it (`rule_may_apply`); a plan child has no network unless declared.
  `authority::tests::a_declared_limit_is_refused_and_no_rule_reaches_past_it` proves these.
- **A folder grant is session-only and never wider than its card**: "Allow for session" on an
  edit keeps the file's folder (`always_allow::session_folder`): inside the workspace, or at
  least two levels inside home outside it; never `/`, home or above, and nothing when home is
  unknown. Sensitive files inside it (git, hooks, agent config, instructions) still ask. The
  answer keeps the folder only if it is the one the card showed (`decide_as_shown`), and
  "Always allow" keeps one file. `always_allow::tests::session_edit_keeps_its_folder_and_nothing_more`
  proves each of these.
- **An idle thread out of sight is suspended, never ended**: after `idle_suspend_minutes`
  (default 15, 0 never; Settings → Behaviour) unseen and unheard from, the window's minute sweep
  (`suspend_idle_threads`) asks `ChatSession::suspend_if_idle`, which stops the agent process
  only when nothing would be lost (`Inner::suspend_blocker`: no turn, queue, approval, permission
  card, model change, handoff, rebind, Codex session grants, unresumable Codex thread, no
  conversation yet, Bypass, delegated worker, active provider workers; the window adds background
  work, delegated runs, a delegated sub-agent's open question and
  pending sends). A delegation root suspends only with its hooks' consent
  (`DelegationHooks::suspending`): a planned rotation (`rotate_before_stop`), so nothing is
  revoked, and the rebind around its resume posts no notice; it also refuses while sub-agents are
  alive. "A conversation" means a resumed start or a turn completed after a prompt (Claude's
  prompt-less init turn names no transcript). The stop is silent: no exit reaches the view or the
  store. A mode or same-agent model picked while suspended is kept for the resume (never Bypass);
  other controls say, unstored, that the agent is paused. The next prompt resumes the same native
  session (`ensure_alive`).
- **Agent processes get a cleaned environment**: `clear_env` and any inherited
  approval-socket variables are removed (`SpawnSpec::unset`), then the profile's
  env file, then the approval socket.
- **Localhost bound**: interacts only with the local shell; exposes no ports. The one outbound
  connection is the update check (`agent_kit::update`): a single HTTPS GET of the repository's
  latest GitHub release, off the main thread, with a timeout. It runs once per process a few
  seconds after startup unless Settings → "Check for Updates at Startup" is off
  (`check_updates_on_startup`), and when the user picks "Check for Updates…". Tests never reach
  it. Never add any other request. Two user actions reach GitHub without the app making a
  request: the diff panel's "Open a PR" hands GitHub's compare URL (built from the remote,
  credentials dropped: `agent_kit::github`) to the browser, and "Push & Open PR" runs one
  `git push` (never forced, after a confirmation that shows the command) through the user's own
  git and credentials. Neither runs unasked.
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

## 🧭 Navigating with Graphify

A Graphify knowledge graph of the repo (code symbols from the AST, docs from a pass on the
NAS Ollama) lives in the git-ignored `graphify-out/`. Reach for it before a broad grep:
the `graphify` MCP tools, or the CLI from the repo root:

- `graphify explain "crates/agent-core/src/redact.rs::redact()"`: a symbol's callers and
  callees with file and line. The strongest tool; start here.
- `graphify path "ChatSession" "crates_agent_kit_src_store_store"`: how two symbols connect.
- `graphify affected "<symbol>"`: what depends on a symbol, before you change it.
- `graphify query "<question>"`: keyword-seeded BFS; broad and noisy, so prefer the above.
- `graphify-out/GRAPH_REPORT.md`: the communities and hub nodes at a glance.

Short names are often ambiguous (`Store` matches four nodes); the CLI exits 3 and lists the
candidates, so retry with `<path>::<symbol>` or the node id it prints. Edges tagged
`[INFERRED]` are guesses; `[EXTRACTED]` ones come from the AST.

The graph is a snapshot: confirm what it says in the source before editing. Refresh it with
`/home/jdesroches/scripts/graphify-build.sh /home/jdesroches/git/agent-terminal` (add
`--code-only` for an AST-only pass with no LLM).

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
5. **Testing**: `make test` while working; `make check` is what CI and a release run (lint,
   the suite, the `#[ignore]`d display tests one at a time); `make coverage` holds
   `COVERAGE_FLOOR`. Never run a test script by hand. Mutation testing (`cargo mutants`) runs
   in CI only (`mutants.yml`), never on a developer machine: its mutants can loop and allocate
   without bound, and one took a workstation down. Details: `tests/qa/README.md`.
6. **Installation**: do **NOT** run `make install` on developer/daily-driver
   machines. Deployment is the `debian-maintainer` apt pipeline
   (`apt.nuvek.ca`), which builds every pushed commit. A manual `make install`
   shadows the apt-delivered package in PATH, so the machine silently keeps
   running a stale build. If you need it for isolated testing, run
   `make uninstall UNINSTALL_ARGS=--keep-data` immediately afterwards: the data is per user,
   shared with the packaged app, and without a terminal the uninstall refuses to guess.

## 📦 Packaging

Packaging is `cargo-deb` driven by `[package.metadata.deb]`, both locally
(`make package`) and in the apt pipeline. Runtime dependencies come from `$auto`,
which derives them from the built ELF — do **not** hand-list them alongside it;
that previously produced duplicate entries with weaker bounds and, before the
switch, omitted `libadwaita-1-0` entirely from every shipped package.

The pipeline step in `debian-maintainer/config.yaml` passes `-o .` because the
orchestrator collects artifacts with a non-recursive scan of the workspace root.

**Releases** (`.github/workflows/release.yml`): a `v*` tag builds the `.deb` (Ubuntu 24.04,
cargo-deb), the `.rpm` (Fedora 42, cargo-generate-rpm, `[package.metadata.generate-rpm]`) and the
Arch package (`packaging/arch/PKGBUILD`, makepkg), each by `packaging/build-<kind>.sh` in a clean
container of its distribution, and publishes them with `SHA256SUMS` as a GitHub Release. The tag
must equal `v` + the Cargo.toml version, and `docs/releases/<tag>.md` must exist (the release
notes): the workflow refuses otherwise. Run it by hand (workflow_dispatch) for a dry run that
builds everything and publishes nothing. The same scripts build locally:
`docker run --rm -v "$PWD":/src:ro -v /tmp/out:/out -e OUT=/out fedora:42 bash /src/packaging/build-rpm.sh`.
Keep the three asset lists (deb, rpm metadata, PKGBUILD) in step.

## 🤖 AI Contribution Rules

- Plans, reports, reviews, council notes, QA evidence and investigation write-ups never go in
  this repo; they live outside it. `docs/` holds only release notes, screenshots and user
  documentation. Commit messages, PR descriptions and code comments never cite plans, councils,
  Hive topics or finding numbers: state the fact itself. `make lint` enforces the `docs/`
  allowlist.
- Always prioritize the **Standalone Philosophy**.
- Colours are user-selectable (`ThemeChoice` in `config.rs`, palettes in
  `theme.rs`, app-wide names in `palette.rs`); add new schemes there and update the
  guard test. The default is the app's own palette (background `#181425`,
  foreground `#c8c8ff`). `ThemeChoice::System` brings no colours: `palette.rs`
  aliases every `@at_*` name to the desktop theme's named colours, leaves
  libadwaita's own alone, does not force dark, and reads values back (for VTE,
  diffs, and Pango markup parsed from then on) when the desktop's style changes.
  Never give it a literal colour.
- If you modify startup/detection logic, update and run the tests in `utils.rs`.
- Deployment-specific behaviour belongs in config, not in the binary. Anything
  that reads a path or runs a command particular to one machine should be an
  `indicators` entry rather than new Rust.
- An indicator-style check has three states, never two. "Could not read the
  source" must have its own icon and never collapse into "healthy" — that
  collapse is exactly what made the old drift button report a permanently green
  shield for a file that had not existed in months.
- Settings apply as they are made, never on dialog close: a switch or dropdown at
  once, a text entry when committed (Enter, its apply button, or focus leaving it),
  and never with a half-typed value. Rows that can hold an invalid
  value must validate inline and refuse to persist rather than silently falling
  back at spawn time.
