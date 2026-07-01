# Antigravity Terminal: Developer Guidelines

Welcome to the Antigravity Terminal codebase. This document outlines the architectural mandates, security standards, and development workflows for this project.

## 🏗️ Architectural Mandates

- **Standalone Philosophy**: This application must remain a single, standalone binary. All assets (SVG, icons) are embedded via `include_str!` or `include_bytes!`. Do NOT introduce external runtime dependencies.
- **Native GTK4/Libadwaita**: The application is a native Rust binary using the Libadwaita framework for a modern, adaptive GNOME experience.
- **PTY Bridge**: Core terminal interactions are handled via `vte4`, bridging the UI with the configurable AI CLI (`claude` by default; also `agy`/`gemini`, selectable in Settings).
- **PTY Isolation**: Terminal interactions inherit the user interactive environment (`-ic`) and full system environment variables to maintain tool accessibility (nvm, aliases, etc.).
- **Tabbed sessions**: The window hosts an `adw::TabView`. Each tab is tracked in an explicit `TabState { page, terminal, dir }` registry (`src/window/imp.rs`) — never look a terminal up by walking the widget tree. A tab's launch directory is fixed for its lifetime (a running CLI cannot re-root itself), which is why "new tab in same directory" reuses it.
- **Module layout**: keep concerns separated — `config.rs` (persisted settings: `TerminalConfig`, `CliClient`, `ThemeChoice`), `theme.rs` (color schemes, built via infallible `RGBA::new`), `utils.rs` (pure logic: detection, startup command, path resolution), `window/imp.rs` (GTK UI), `main.rs` (app setup + logging).
- **Feature flags**: homelab-specific integrations must sit behind a Cargo feature. The Ansible-drift indicator is behind the default-on `homelab-drift` feature; `--no-default-features` yields a generic terminal.

## 🔒 Security & Robustness

- **Localhost Bound**: The application interacts exclusively with the local shell and does not expose network ports.
- **Graceful Error Handling**: Avoid `.unwrap()` and `.expect()` in critical paths. Use proper `Result` handling and provide clear error messages to the user if the shell or binary cannot be located.
- **Reference Integrity**: All GTK signal handlers MUST use weak references (`glib::clone!`) to prevent memory leaks and circular reference cycles.
- **Observability**: Logging is initialized in `main.rs` — it writes to the systemd journal (identifier `antigravity-terminal`, filter with `journalctl --user -t antigravity-terminal -b`) and, when attached to a terminal, to stderr. A panic hook routes crashes through `tracing` so a live failure is diagnosable. Keep meaningful `info!/warn!/error!` coverage on spawn, detection, config, and tab lifecycle paths.

## 🧪 Testing Strategy

- **Logic Separation**: Keep "pure" logic (command generation, path resolution, config load/save) in `src/utils.rs` and `src/config.rs`, decoupled from GTK types so it is unit-testable. GTK/UI code in `window/imp.rs` is covered only by a construction smoke test.
- **Unit Testing**: Add or update tests alongside any change to detection, startup-command, path-resolution, or config-serialization logic (e.g. the config round-trip/malformed-file tests). Run `cargo test` before submitting any PR.

## 🛠️ Development Workflow

1. **Environment Check**: Verify dependencies with `make deps` (`libgtk-4-dev`, `libvte-2.91-gtk4-dev`, `libadwaita-1-dev`).
2. **Build & Run**: `cargo build` for development, `make start-local` to build and run, `make build` for release.
3. **Linting**: Always run `cargo fmt` and `cargo clippy --all-targets -- -D warnings`. Also lint the generic build: `cargo clippy --no-default-features --all-targets -- -D warnings`. CI (`.github/workflows/ci.yml`) enforces fmt, both clippy configs, and tests (under Xvfb).
4. **Installation**: Do **NOT** run `make install` on developer/daily-driver machines. Deployment is handled by the `debian-maintainer` apt pipeline (`apt.nuvek.ca`), which builds every pushed commit and ships the `antigravity-terminal` package to `/usr/bin`. A manual `make install` copies the binary to `~/.local/bin` and writes a local `.desktop`, both of which **shadow the apt-delivered package in PATH** — so the machine silently keeps running a stale build even after the pipeline ships an update. If you ever need it for one-off, isolated deployment-lifecycle testing, run `make uninstall` immediately afterward.

## 🤖 AI Contribution Rules

- Always prioritize the **Standalone Philosophy**.
- Terminal colors are user-selectable (`ThemeChoice` in `config.rs`, palettes in `theme.rs`); add new schemes there. The window chrome (header bar, dialogs) still follows the Antigravity brand CSS in `main.rs` (Background: `#181425`, Foreground: `#c8c8ff`) — keep new UI chrome consistent with it.
- If you modify the startup/detection logic, you MUST update and run the corresponding unit tests in `utils.rs`.
- Put homelab-specific behavior behind the `homelab-drift` feature (or a new feature); the default `--no-default-features` build must stay generic and compile.
