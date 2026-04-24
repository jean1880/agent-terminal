# Gemini Terminal: Developer Guidelines

Welcome to the Gemini Terminal codebase. This document outlines the architectural mandates, security standards, and development workflows for this project.

## 🏗️ Architectural Mandates

- **Standalone Philosophy**: This application must remain a single, standalone binary. Do NOT introduce dependencies on external shell scripts in `~/scripts/` or elsewhere.
- **GTK4 Subclassing**: All UI windows and complex widgets must be implemented using the proper GObject subclassing pattern (`glib::wrapper!` and `ObjectSubclass`). Avoid monolithic procedural UI code.
- **VTE Integration**: Terminal interactions are handled via `vte4`. Ensure that terminal spawning is handled asynchronously to prevent UI blocking.

## 🔒 Security & Robustness

- **Dynamic Resolution**: Never hardcode absolute paths to the user's home directory. Always use environment variables (`$HOME`, `$SHELL`) or `std::env` to resolve paths at runtime.
- **Panic Prevention**: Avoid `.unwrap()` and `.expect()` in production code paths. Use `unwrap_or` with sensible defaults or handle `Result`/`Option` types gracefully with UI feedback if necessary.
- **Environment Isolation**: The terminal should inherit the user's interactive environment (`-ic`) to ensure tools like `nvm`, `rbenv`, or custom aliases work out of the box.

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
- When adding UI elements, verify they match the "Gemini Theme" (Background: `#181425`, Foreground: `#c8c8ff`).
- If you modify the startup logic, you MUST update and run the corresponding unit tests.
