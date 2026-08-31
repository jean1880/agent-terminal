# Agent Terminal 🚀

A standalone GTK4 terminal application written in Rust, purpose-built for driving
an AI coding CLI (Claude, Antigravity/`agy`, or Gemini) in a focused, tabbed
window.

> **Renamed in 2.0.0.** This was `antigravity-terminal` (and, before that,
> `gemini-terminal`). The binary, package and config directory are now
> `agent-terminal`; existing settings are adopted automatically on first run —
> see [Upgrading](#upgrading-from-1x-).

## Features ✨

- **Native GTK4 & VTE4**: high-performance terminal rendering with a Libadwaita
  header bar.
- **Tabbed sessions**: multiple terminals in one window.
  - `Ctrl + Shift + T` (or the header **+**) opens a new tab rooted in the current
    tab's directory. The tab bar auto-hides when only one tab is open.
  - **New Tab in Folder…** opens a folder picker and roots a new tab there —
    useful because a running CLI session cannot re-root itself.
- **Sessions survive a crash**: if the CLI exits non-zero the tab stays open with
  its scrollback intact and a bar explaining what happened, offering **Restart**
  and **Close Tab**. A clean exit still closes the tab as you would expect.
- **Selectable colour themes**: Antigravity (default), Dracula, Nord, Gruvbox
  Dark, Solarized Dark, One Dark, and Monokai — applied live to every open tab.
- **Configurable CLI client**: auto-detect (prefers `claude`), or pin `Claude`,
  `Agy`, or `Gemini`. Detection runs off the UI thread and is cached for the life
  of the process.
- **Settings**: starting directory (validated as you type), scrollback lines,
  font scale, CLI client, and theme — applied immediately and persisted to
  `~/.config/agent-terminal/config.json`.
- **Shortcuts**:
  | Keys | Action |
  |---|---|
  | `Ctrl + Shift + T` | New tab |
  | `Ctrl + Shift + R` | Restart the current session |
  | `Ctrl + Shift + C` / `V` | Copy / paste |
  | `Ctrl + Plus` / `Minus` | Zoom (persisted) |
  | `Ctrl + 0` | Reset zoom |
  | `Ctrl + Left-Click` | Open a hovered hyperlink |
- **Observability**: logs to the systemd journal, with a panic hook, so a
  desktop-launched failure is diagnosable after the fact:
  ```bash
  journalctl --user -t agent-terminal -b
  ```
  When run from a terminal, logs also print to stderr. Level defaults to `info`
  and is overridable via `RUST_LOG`.
- **Standalone identity**: treated as a unique application by your window manager
  (won't group with standard terminals). All assets are embedded in the binary.
- **Sixel support**: inline image rendering.

## Upgrading from 1.x ⬆️

The apt package is renamed, so `apt upgrade` pulls in `agent-terminal` and
removes `antigravity-terminal` via a transitional package.

On first launch, settings at `~/.config/antigravity-terminal/config.json` are
copied to `~/.config/agent-terminal/config.json`. **The old file is deliberately
left in place** so that reinstalling 1.x still finds its configuration.

## Prerequisites 🛠️

- **Rust & Cargo** (1.92+)
- **GTK 4.10+** (`libgtk-4-dev`)
- **VTE 2.91 GTK4** (`libvte-2.91-gtk4-dev`)
- **libadwaita 1.5+** (`libadwaita-1-dev`)

## Quick Start ⚡

```bash
make deps          # install system dependencies
make start-local   # build (debug) and run
```

> **Running a local build while the packaged one is open?** GTK's single-instance
> handling will hand your launch to the already-running process, so your build
> never actually runs. Use its own bus:
> ```bash
> dbus-run-session -- ./target/debug/agent-terminal
> ```

## Advanced Usage 🔧

### Cargo feature flags
The homelab Ansible-drift indicator is behind the default-on `homelab-drift`
feature. For a generic terminal without it:
```bash
cargo build --release --no-default-features
```

### Generating a Debian package (.deb)
```bash
make package
```
*Requires `cargo-deb`; the Makefile installs it if missing.* Runtime dependencies
are derived from the built binary via `$auto`, so they cannot drift from what it
actually links.

### Development workflow
- **Build**: `cargo build` · **Run**: `make start-local`
- **Lint**: `cargo fmt` and `cargo clippy --all-targets -- -D warnings`
  (CI also lints `--no-default-features`)
- **Test**: `cargo test`

## Project Structure 📁

- `src/main.rs` — entry point, application setup, logging, global CSS, accelerators.
- `src/window/imp.rs` — window implementation: tabs, spawning, input, settings.
- `src/window/mod.rs` — the `AgentTerminalWindow` GObject wrapper.
- `src/config.rs` — persisted settings and the 1.x migration, with tests.
- `src/theme.rs` — terminal colour schemes.
- `src/utils.rs` — pure logic: CLI detection, startup command, path resolution.

## Contributing 🤝

1. Branch: `git checkout -b feature/cool-new-thing`.
2. Keep it green: `cargo fmt`, both clippy configurations, and `cargo test`.
3. Commit using Conventional Commits.

## License 📄

MIT — see [LICENSE](LICENSE).
