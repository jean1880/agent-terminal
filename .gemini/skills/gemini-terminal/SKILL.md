# Skill: Gemini Terminal Architect

Specialized guidance for maintaining and extending the `gemini-terminal` application.

## 🎯 Expertise
- **GTK4/Rust (glib-rs)**: Expert in the GObject subclassing pattern and signal handling.
- **VTE4 (Terminal Emulation)**: Specialized in virtual terminal execution, PTY management, and Sixel graphics.
- **Linux Desktop Integration**: Deep knowledge of XDG specifications, `.desktop` file standards, and Debian packaging.

## 🛠️ Specialized Workflows

### Creating a New UI Component
When asked to add a new widget or window:
1.  Define the private state in an `imp` module using `ObjectSubclass`.
2.  Implement `ObjectImpl`, `WidgetImpl`, and the specific widget's trait (e.g., `WindowImpl`).
3.  Register the type in the parent module using `glib::wrapper!`.
4.  Ensure all styles are applied via `CssProvider` using the project's color palette.

### Modifying Terminal Behavior
When modifying how the terminal launches or interacts with the shell:
1.  Locate `get_startup_command` in `src/window/imp.rs`.
2.  Modify the logic and immediately update the unit tests in the `tests` module.
3.  Verify the fix with `cargo test`.
4.  Check for regressions in shell environment inheritance by running the app.

## 📏 Standards
- **Naming**: Use `CamelCase` for structs/types and `snake_case` for methods/variables.
- **Safety**: Prefer safe wrappers over `unsafe` blocks. If `unsafe` is necessary (rare in this project), document the invariants clearly.
- **Visuals**: Maintain the "Gemini Theme". Interactive elements should have subtle hover/active states consistent with the current UI.
