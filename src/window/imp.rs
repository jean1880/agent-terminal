//! Private implementation details of the GeminiWindow.

use gtk4::glib;
use gtk4::prelude::*;
use gtk4::subclass::prelude::*;
use gtk4::{HeaderBar, Label, ScrolledWindow};
use std::env;
use vte4::prelude::*;
use vte4::{CursorBlinkMode, CursorShape, Format, PtyFlags, Terminal};

/// Internal state for the GeminiWindow.
#[derive(Default)]
pub struct GeminiWindow {}

#[glib::object_subclass]
impl ObjectSubclass for GeminiWindow {
    const NAME: &'static str = "GeminiWindow";
    type Type = super::GeminiWindow;
    type ParentType = gtk4::ApplicationWindow;
}

impl ObjectImpl for GeminiWindow {
    fn constructed(&self) {
        self.parent_constructed();
        self.setup_ui();
    }
}

impl WidgetImpl for GeminiWindow {}
impl WindowImpl for GeminiWindow {}
impl ApplicationWindowImpl for GeminiWindow {}

impl GeminiWindow {
    /// Initializes the user interface, terminal, and keybindings.
    fn setup_ui(&self) {
        let obj = self.obj();

        obj.set_default_width(950);
        obj.set_default_height(650);

        // Modern HeaderBar
        let header = HeaderBar::builder()
            .title_widget(&Label::new(Some("Gemini Terminal")))
            .show_title_buttons(true)
            .build();

        obj.set_titlebar(Some(&header));

        let terminal = Terminal::new();

        // Gemini Theme Colors
        let bg_color = gtk4::gdk::RGBA::parse("rgb(24,20,37)").unwrap_or(gtk4::gdk::RGBA::BLACK);
        let fg_color = gtk4::gdk::RGBA::parse("rgb(200,200,255)").unwrap_or(gtk4::gdk::RGBA::WHITE);
        let bold_color = gtk4::gdk::RGBA::parse("rgb(142,117,255)").unwrap_or(fg_color);

        terminal.set_colors(Some(&fg_color), Some(&bg_color), &[]);
        terminal.set_color_bold(Some(&bold_color));

        terminal.set_cursor_blink_mode(CursorBlinkMode::On);
        terminal.set_cursor_shape(CursorShape::Block);
        terminal.set_scrollback_lines(10000);
        terminal.set_enable_sixel(true);

        // Keyboard Shortcuts (Copy/Paste/Zoom)
        let key_controller = gtk4::EventControllerKey::new();
        let term_clone = terminal.clone();
        key_controller.connect_key_pressed(move |_ctrl, key, _code, state| {
            let is_ctrl = state.contains(gtk4::gdk::ModifierType::CONTROL_MASK);
            let is_shift = state.contains(gtk4::gdk::ModifierType::SHIFT_MASK);

            match key {
                gtk4::gdk::Key::C | gtk4::gdk::Key::c if is_ctrl && is_shift => {
                    term_clone.copy_clipboard_format(Format::Text);
                    glib::Propagation::Stop
                }
                gtk4::gdk::Key::V | gtk4::gdk::Key::v if is_ctrl && is_shift => {
                    term_clone.paste_clipboard();
                    glib::Propagation::Stop
                }
                gtk4::gdk::Key::plus | gtk4::gdk::Key::equal if is_ctrl => {
                    let scale = term_clone.font_scale();
                    term_clone.set_font_scale(scale + 0.1);
                    glib::Propagation::Stop
                }
                gtk4::gdk::Key::minus if is_ctrl => {
                    let scale = term_clone.font_scale();
                    term_clone.set_font_scale((scale - 0.1).max(0.1));
                    glib::Propagation::Stop
                }
                k if k.to_unicode() == Some('0') && is_ctrl => {
                    term_clone.set_font_scale(1.0);
                    glib::Propagation::Stop
                }
                _ => glib::Propagation::Proceed,
            }
        });
        terminal.add_controller(key_controller);

        // Dynamic Shell Detection
        let shell = env::var("SHELL").unwrap_or_else(|_| "/bin/sh".to_string());
        let home_dir = env::var("HOME").unwrap_or_else(|_| "/".to_string());

        // Check if gemini command exists
        let gemini_exists = std::process::Command::new("which")
            .arg("gemini")
            .output()
            .map(|o| o.status.success())
            .unwrap_or(false);

        let command = get_startup_command(gemini_exists);

        terminal.spawn_async(
            PtyFlags::DEFAULT,
            Some(&home_dir),
            &[&shell, command[0], command[1]],
            &[],
            glib::SpawnFlags::DO_NOT_REAP_CHILD,
            || {},
            -1,
            None::<&gtk4::gio::Cancellable>,
            move |result| {
                if let Err(err) = result {
                    eprintln!("Error spawning terminal: {}", err);
                }
            },
        );
        let scrolled = ScrolledWindow::builder()
            .hscrollbar_policy(gtk4::PolicyType::Never)
            .vscrollbar_policy(gtk4::PolicyType::Automatic)
            .child(&terminal)
            .css_classes(["terminal-container"])
            .build();

        obj.set_child(Some(&scrolled));
    }
}

/// Determines the startup command based on whether the gemini binary exists.
fn get_startup_command(has_gemini: bool) -> Vec<&'static str> {
    if has_gemini {
        vec!["-ic", "gemini; exec $SHELL"]
    } else {
        vec![
            "-ic",
            "echo '⚠️  Gemini CLI not found in PATH.'; echo ''; echo 'To install it, run:'; echo '  npm install -g @google/gemini-cli'; echo ''; exec $SHELL",
        ]
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_startup_command_gemini_exists() {
        let cmd = get_startup_command(true);
        assert_eq!(cmd[0], "-ic");
        assert!(cmd[1].contains("gemini; exec $SHELL"));
    }

    #[test]
    fn test_startup_command_gemini_missing() {
        let cmd = get_startup_command(false);
        assert_eq!(cmd[0], "-ic");
        assert!(cmd[1].contains("npm install -g @google/gemini-cli"));
    }
}
