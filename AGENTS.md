# Agent Terminal: Developer Guidelines

Architectural mandates, standards, and workflows for this codebase.

## 🏗️ Architectural Mandates

- **Standalone Philosophy**: a single, standalone binary. All assets (SVG, icons)
  are embedded via `include_str!`/`include_bytes!`. Do NOT introduce external
  runtime dependencies.
- **Native GTK4/Libadwaita**: a native Rust binary. Minimum system libraries are
  set by the Cargo version features, currently GTK 4.10 (`v4_10`, for
  `FileDialog`) and libadwaita 1.5 (`v1_5`, for `AlertDialog`/`PreferencesDialog`).
  Raising either is a deliberate decision, not a side effect.
- **No redundant GTK crates**: `glib`, `gio` and `gdk4` are reached through the
  `gtk4` re-exports (`gtk4::glib`, …). Do not add them as direct dependencies —
  that only creates a second place for versions to drift.
- **PTY Bridge**: terminal interaction is via `vte4`, bridging the UI with the
  configurable AI CLI (`claude` by default; also `agy`/`gemini`).
- **PTY Isolation**: sessions inherit the user's interactive environment (`-ic`)
  and full environment variables so tooling (nvm, aliases) stays available.
  `TERM`/`COLORTERM` are forced so the CLI renders its full TUI.
- **`exec`, and nothing before it**: the startup command is `exec <cli>` so the
  CLI owns the controlling terminal. Never emit banner or `clear` output into the
  TTY beforehand — it disrupts the CLI's initial terminal handshake. Anything
  resembling a startup script must contribute *environment*, merged into the
  spawn environment, never bytes written to the terminal.
- **Tabbed sessions**: the window hosts an `adw::TabView`. Each tab is tracked in
  an explicit `TabState` registry (`src/window/imp.rs`) — never look a terminal up
  by walking the widget tree. A tab's launch directory is fixed for its lifetime
  (a running CLI cannot re-root itself).
- **Module layout**: `config.rs` (persisted settings + migration), `theme.rs`
  (colour schemes, infallible `RGBA::new`), `utils.rs` (pure logic: detection,
  startup command, path resolution), `window/imp.rs` (GTK UI), `main.rs` (app
  setup + logging).
- **No feature flags for deployment specifics**: the crate has none. The
  Ansible-drift indicator used to sit behind `homelab-drift`; it is now one
  possible entry in the config-declared `indicators` list, which honours the
  standalone philosophy more strictly than a `#[cfg]` did — anyone gets the same
  capability, and the shipped binary carries no homelab knowledge at all.

## 🔒 Security & Robustness

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

- **Logic separation**: keep pure logic in `src/utils.rs` and `src/config.rs`,
  decoupled from GTK so it is unit-testable without a display. `window/imp.rs` is
  covered by a construction smoke test plus tests for any pure helpers in it.
- **Guard hand-maintained arrays**: `CliClient::ALL` and `ThemeChoice::ALL` drive
  the settings dropdowns by index in both directions. Both have exhaustive-match
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
4. **Linting**: `cargo fmt` and `cargo clippy --all-targets -- -D warnings`.
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
