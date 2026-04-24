//! Private implementation details of the GeminiWindow.

use gtk4::glib;
use gtk4::prelude::*;
use gtk4::subclass::prelude::*;
use gtk4::{Align, Box, Button, HeaderBar, Label, Orientation, ScrolledWindow};
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
    /// Initializes the user interface, switching between terminal and welcome screen.
    fn setup_ui(&self) {
        let obj = self.obj();

        obj.set_default_width(950);
        obj.set_default_height(650);

        let is_available = self.is_gemini_available();

        // Start Gemini Button in Header
        let start_button = Button::builder()
            .label("Start Gemini")
            .css_classes(["suggested-action"])
            .visible(is_available)
            .build();

        let obj_clone = obj.clone();
        start_button.connect_clicked(move |_| {
            let imp = obj_clone.imp();
            imp.setup_terminal_ui();
        });

        // Modern HeaderBar
        let header = HeaderBar::builder()
            .title_widget(&Label::new(Some("Gemini Terminal")))
            .show_title_buttons(true)
            .build();

        header.pack_start(&start_button);
        obj.set_titlebar(Some(&header));

        if is_available {
            self.setup_terminal_ui();
        } else {
            self.setup_welcome_ui();
        }
    }

    /// Checks if the gemini binary is available in the PATH or common locations.
    fn is_gemini_available(&self) -> bool {
        // 1. Try which
        if std::process::Command::new("which")
            .arg("gemini")
            .output()
            .map(|o| o.status.success())
            .unwrap_or(false)
        {
            return true;
        }

        // 2. Try common absolute paths
        let home = env::var("HOME").unwrap_or_default();
        let paths = [
            "/usr/bin/gemini",
            "/usr/local/bin/gemini",
            &format!("{}/.local/bin/gemini", home),
            &format!("{}/.npm-global/bin/gemini", home),
            &format!("{}/bin/gemini", home),
        ];

        for path in paths {
            if !path.is_empty() && std::path::Path::new(path).exists() {
                return true;
            }
        }

        // 3. Try shell command -v as a last resort
        let shell = env::var("SHELL").unwrap_or_else(|_| "/bin/sh".to_string());
        std::process::Command::new(shell)
            .args(["-ic", "command -v gemini"])
            .output()
            .map(|o| o.status.success())
            .unwrap_or(false)
    }

    /// Sets up the terminal interface.
    fn setup_terminal_ui(&self) {
        let obj = self.obj();
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

        // Close window when the terminal child exits (e.g., user exits gemini)
        let obj_clone = obj.clone();
        terminal.connect_child_exited(move |_, _| {
            obj_clone.close();
        });

        // Dynamic Shell Detection
        let shell = env::var("SHELL").unwrap_or_else(|_| "/bin/sh".to_string());
        let home_dir = env::var("HOME").unwrap_or_else(|_| "/".to_string());

        let command = get_startup_command(true);

        let obj_clone = obj.clone();
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
                    let dialog = gtk4::MessageDialog::builder()
                        .transient_for(&obj_clone)
                        .message_type(gtk4::MessageType::Error)
                        .buttons(gtk4::ButtonsType::Ok)
                        .text("Terminal Error")
                        .secondary_text(format!("Error spawning terminal: {}", err))
                        .build();
                    dialog.connect_response(|dialog, _| dialog.close());
                    dialog.present();
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

    /// Sets up the welcome screen with installation instructions.
    fn setup_welcome_ui(&self) {
        let obj = self.obj();
        let available = self.is_gemini_available();

        let container = Box::builder()
            .orientation(Orientation::Vertical)
            .valign(Align::Center)
            .halign(Align::Center)
            .spacing(20)
            .margin_top(40)
            .margin_bottom(40)
            .margin_start(40)
            .margin_end(40)
            .build();

        let title = Label::builder()
            .label("Welcome to Gemini Terminal")
            .css_classes(["title-1"])
            .build();

        let subtitle = Label::builder()
            .label("The Gemini CLI was not detected on your system.")
            .css_classes(["subtitle"])
            .build();

        let instructions = Label::builder()
            .label("To get started, please install the Gemini CLI using npm:")
            .margin_top(10)
            .build();

        let command_label = Label::builder()
            .label("npm install -g @google/gemini-cli")
            .selectable(true)
            .css_classes(["command-text"])
            .build();

        let config_instructions = Label::builder()
            .label("After installation, configure it by running:")
            .margin_top(10)
            .build();

        let config_command = Label::builder()
            .label("gemini configure")
            .selectable(true)
            .css_classes(["command-text"])
            .build();

        let refresh_button = Button::builder()
            .label(if available { "Start Gemini" } else { "I've installed it, let's go!" })
            .margin_top(20)
            .css_classes(["suggested-action"])
            .build();

        let obj_clone = obj.clone();
        refresh_button.connect_clicked(move |_| {
            let imp = obj_clone.imp();
            imp.setup_ui();
        });

        container.append(&title);
        container.append(&subtitle);
        container.append(&instructions);
        container.append(&command_label);
        container.append(&config_instructions);
        container.append(&config_command);
        container.append(&refresh_button);

        obj.set_child(Some(&container));
    }
}

/// Determines the startup command based on whether the gemini binary exists.
fn get_startup_command(has_gemini: bool) -> Vec<&'static str> {
    if has_gemini {
        vec!["-ic", "gemini"]
    } else {
        vec!["-ic", "exec $SHELL"]
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_startup_command_gemini_exists() {
        let cmd = get_startup_command(true);
        assert_eq!(cmd[0], "-ic");
        assert!(cmd[1].contains("gemini"));
    }

    #[test]
    fn test_startup_command_gemini_missing() {
        let cmd = get_startup_command(false);
        assert_eq!(cmd[0], "-ic");
        assert!(cmd[1].contains("exec $SHELL"));
    }
}
