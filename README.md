# Antigravity Terminal 🚀

A robust, standalone GTK4 terminal application written in Rust, purpose-built for
driving an AI coding CLI (Claude, Antigravity/`agy`, or Gemini) in a focused,
tabbed window.

## Features ✨

- **Native GTK4 & VTE4**: High-performance, GPU-accelerated terminal rendering
  with a Libadwaita header bar.
- **Tabbed sessions**: Multiple terminals in one window.
  - `Ctrl + Shift + T` (or the header **+** button) opens a new tab rooted in the
    current tab's directory. The tab bar auto-hides when only one tab is open.
  - **New Tab in Folder…** (header button and right-click menu) opens a folder
    picker and roots a new tab there — handy since a running CLI session cannot
    re-root itself.
- **Selectable color themes**: Antigravity (default), Dracula, Nord, Gruvbox
  Dark, Solarized Dark, One Dark, and Monokai — chosen from **Settings** and
  applied live to every open tab.
- **Configurable CLI client**: Auto-detect (prefers `claude`), or pin `Claude`,
  `Agy`, or `Gemini`. Detection is cached so opening tabs never stalls the UI.
- **Settings**: startup script path, starting directory, scrollback lines, font
  scale, CLI client, and theme — persisted to
  `~/.config/antigravity-terminal/config.json`.
- **Interactive shortcuts**:
  - `Ctrl + Shift + C` / `V`: Copy and Paste.
  - `Ctrl + Shift + T`: New tab.
  - `Ctrl + Plus` / `Minus`: Dynamic text zoom (persisted).
  - `Ctrl + 0`: Reset zoom.
  - `Ctrl + Left-Click`: Open a hovered hyperlink.
- **Observability**: Logs to the systemd journal (with a panic hook), so a
  desktop-launched failure is diagnosable after the fact:
  ```bash
  journalctl --user -t antigravity-terminal -b
  ```
  When run from a terminal, logs also print to stderr. Level defaults to `info`
  and is overridable via `RUST_LOG`.
- **Standalone identity**: Treated as a unique application by your window manager
  (won't group with standard terminals). All assets are embedded in the binary.
- **Sixel support**: High-quality inline image rendering.

## Prerequisites 🛠️

- **Rust & Cargo** (1.70+)
- **GTK 4 Development Files** (`libgtk-4-dev`)
- **VTE 2.91 GTK4 Development Files** (`libvte-2.91-gtk4-dev`)
- **Libadwaita Development Files** (`libadwaita-1-dev`)

## Quick Start ⚡

### 1. Install dependencies
```bash
make deps
```

### 2. Run locally
```bash
make start-local   # builds (debug) and runs the app
```

### 3. Build and install locally
Builds the release binary, installs it to `~/.local/bin`, and updates your
desktop menu entry:
```bash
make install
```
> Note: on machines fed by the `debian-maintainer` apt pipeline, prefer the
> packaged build — a local `make install` shadows it in `PATH`. See `AGENTS.md`.

## Advanced Usage 🔧

### Cargo feature flags
The homelab Ansible-drift health indicator is behind the default-on
`homelab-drift` feature. For a generic terminal without the homelab integration:
```bash
cargo build --release --no-default-features
```

### Generating a Debian package (.deb)
```bash
make package
```
*Requires `cargo-deb`; the Makefile will install it if missing.*

### Development workflow
- **Build**: `cargo build`
- **Run**: `cargo run` or `make start-local`
- **Format / lint**: `cargo fmt` and `cargo clippy --all-targets -- -D warnings`
  (CI also lints `--no-default-features`)
- **Test**: `cargo test`
- **Clean**: `make clean`

## Project Structure 📁

- `src/main.rs`: Entry point, application setup, logging (journald + panic hook),
  global CSS, and accelerators.
- `src/window/imp.rs`: Window implementation — tabbed UI, terminal spawning,
  input controllers, and settings.
- `src/window/mod.rs`: `AntigravityWindow` GObject wrapper.
- `src/config.rs`: Persisted settings (`TerminalConfig`, `CliClient`,
  `ThemeChoice`) with load/save and tests.
- `src/theme.rs`: Terminal color schemes.
- `src/utils.rs`: Pure logic — CLI detection, startup command, directory
  resolution — unit tested.
- `Cargo.toml`: Dependencies, features, and `.deb` packaging metadata.
- `Makefile`: Convenience wrappers for common tasks.

## Contributing 🤝

1. **Create a feature branch**: `git checkout -b feature/cool-new-thing`.
2. **Keep it green**: `cargo fmt`, `cargo clippy --all-targets -- -D warnings`,
   and `cargo test` must all pass (both feature configurations).
3. **Commit** using Conventional Commits.

## License 📄

This project is licensed under the MIT License - see the [LICENSE](LICENSE) file for details.
