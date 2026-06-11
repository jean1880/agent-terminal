# Antigravity Terminal: Developer Guidelines

Welcome to the Antigravity Terminal codebase. This document outlines the architectural mandates, security standards, and development workflows for this project.

## 🏗️ Architectural Mandates

- **Standalone Philosophy**: This application must remain a single, standalone binary. All assets (SVG, icons) are embedded via `include_str!` or `include_bytes!`. Do NOT introduce external runtime dependencies.
- **Native GTK4/Libadwaita**: The application is a native Rust binary using the Libadwaita framework for a modern, adaptive GNOME experience.
- **PTY Bridge**: Core terminal interactions are handled via `vte4`, bridging the UI with the local Antigravity CLI.
- **PTY Isolation**: Terminal interactions inherit the user interactive environment (`-ic`) and full system environment variables to maintain tool accessibility (nvm, aliases, etc.).

## 🔒 Security & Robustness

- **Localhost Bound**: The application interacts exclusively with the local shell and does not expose network ports.
- **Graceful Error Handling**: Avoid `.unwrap()` and `.expect()` in critical paths. Use proper `Result` handling and provide clear error messages to the user if the shell or binary cannot be located.
- **Reference Integrity**: All GTK signal handlers MUST use weak references (`glib::clone!`) to prevent memory leaks and circular reference cycles.

## 🧪 Testing Strategy

- **Logic Separation**: Keep "pure" logic (command generation, path resolution, config parsing) in independent functions decoupled from GTK types.
- **Unit Testing**: Maintain 100% coverage for environment-handling logic in `src/window/imp.rs`. Run `cargo test` before submitting any PR.

## 🛠️ Development Workflow

1. **Environment Check**: Verify dependencies with `make deps`.
2. **Build**: Use `cargo build` for development and `make build` for release.
3. **Linting**: Always run `cargo clippy -- -D warnings` and `cargo fmt`.
4. **Installation**: Use `make install` to test the full deployment lifecycle (desktop entries, icons, etc.).

## 🤖 AI Contribution Rules

- Always prioritize the **Standalone Philosophy**.
- When adding UI elements, verify they match the "Antigravity Theme" (Background: `#181425`, Foreground: `#c8c8ff`).
- If you modify the startup logic, you MUST update and run the corresponding unit tests.
