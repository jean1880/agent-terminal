//! Private implementation details of the GeminiWindow.

use adw::prelude::*;
use adw::subclass::prelude::*;
use gtk4::glib;
use gtk4::prelude::*;
use gtk4::{Align, Box, Button, Image, Label, Orientation, ScrolledWindow, Stack};
use std::env;
use vte4::prelude::*;
use vte4::{CursorBlinkMode, CursorShape, Format, PtyFlags, Terminal};
use tracing::{info, warn, error, debug};

use std::cell::RefCell;

/// Static logo SVG for standalone binary.
const LOGO_SVG: &str = include_str!("../../assets/gemini_logo.svg");

/// Internal state for the GeminiWindow.
#[derive(Default)]
pub struct GeminiWindow {
    pub terminal: RefCell<Option<Terminal>>,
    pub header: RefCell<Option<adw::HeaderBar>>,
    pub loading_stack: RefCell<Option<Stack>>,
}

#[glib::object_subclass]
impl ObjectSubclass for GeminiWindow {
    const NAME: &'static str = "GeminiWindow";
    type Type = super::GeminiWindow;
    type ParentType = adw::ApplicationWindow;
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
impl AdwApplicationWindowImpl for GeminiWindow {}

impl GeminiWindow {
    /// Initializes the user interface, switching between terminal and welcome screen.
    fn setup_ui(&self) {
        let obj = self.obj();
        debug!("Setting up UI for GeminiWindow");

        obj.set_default_width(950);
        obj.set_default_height(650);

        let content = Box::builder()
            .orientation(Orientation::Vertical)
            .build();

        // Modern AdwHeaderBar
        let header = adw::HeaderBar::builder()
            .title_widget(&adw::WindowTitle::new("Gemini Terminal", ""))
            .build();

        content.append(&header);
        *self.header.borrow_mut() = Some(header);

        obj.set_content(Some(&content));

        if self.is_gemini_available() {
            info!("Gemini CLI detected, setting up terminal UI");
            self.setup_terminal_ui(&content);
        } else {
            warn!("Gemini CLI not detected, setting up welcome UI");
            self.setup_welcome_ui(&content);
        }
    }

    /// Checks if the gemini binary is available in the PATH or common locations.
    fn is_gemini_available(&self) -> bool {
        let current_path = env::var("PATH").unwrap_or_default();
        info!("Detection PATH: {}", current_path);
        debug!("Checking for gemini binary...");

        // 1. Try which
        debug!("Step 1: Trying 'which gemini'");
        match std::process::Command::new("which").arg("gemini").output() {
            Ok(output) => {
                if output.status.success() {
                    let path = String::from_utf8_lossy(&output.stdout).trim().to_string();
                    info!("Gemini found via 'which' at: {}", path);
                    return true;
                } else {
                    warn!("'which gemini' failed with status: {}", output.status);
                }
            }
            Err(e) => error!("Failed to execute 'which': {}", e),
        }

        // 2. Try common absolute paths
        debug!("Step 2: Trying common absolute paths");
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
                debug!("Gemini found at absolute path: {}", path);
                return true;
            }
        }

        // 3. Try shell command -v (interactive)
        debug!("Step 3: Trying shell command -v gemini");
        let shell = env::var("SHELL").unwrap_or_else(|_| "/bin/sh".to_string());
        // Use -i (interactive) to ensure NVM/rbenv are loaded
        if let Ok(output) = std::process::Command::new(&shell)
            .args(["-ic", "command -v gemini"])
            .output()
        {
            if output.status.success() {
                let path = String::from_utf8_lossy(&output.stdout).trim().to_string();
                info!("Gemini found via shell -ic at: {}", path);
                return true;
            }
        }

        warn!("Gemini binary not found after all checks.");
        false
    }

    /// Sets up the terminal interface.
    fn setup_terminal_ui(&self, container: &Box) {
        let obj = self.obj();
        debug!("Initializing terminal UI");
        let terminal = Terminal::new();
        *self.terminal.borrow_mut() = Some(terminal.clone());

        // Create Stack for transition
        let stack = Stack::builder()
            .transition_type(gtk4::StackTransitionType::Crossfade)
            .transition_duration(500)
            .vexpand(true)
            .build();
        *self.loading_stack.borrow_mut() = Some(stack.clone());

        // Loading Screen
        let loading_box = Box::builder()
            .orientation(Orientation::Vertical)
            .valign(Align::Center)
            .halign(Align::Center)
            .spacing(10)
            .css_classes(["loading-container"])
            .build();

        // Logo with rotation (SVG)
        let logo_image = if let Ok(loader) = gtk4::gdk_pixbuf::PixbufLoader::with_type("svg") {
            loader.set_size(128, 128);
            let load_result = loader.write(LOGO_SVG.as_bytes())
                .and_then(|_| loader.close())
                .and_then(|_| loader.pixbuf().ok_or(glib::Error::new(gtk4::gio::IOErrorEnum::Failed, "Failed to get pixbuf")));

            match load_result {
                Ok(pixbuf) => {
                    let texture = gtk4::gdk::Texture::for_pixbuf(&pixbuf);
                    let img = Image::builder()
                        .pixel_size(96)
                        .css_classes(["loading-icon"])
                        .build();
                    img.set_paintable(Some(&texture));
                    Some(img)
                }
                Err(e) => {
                    error!("Failed to load embedded Gemini logo: {}", e);
                    None
                }
            }
        } else {
            error!("SVG PixbufLoader not available");
            None
        };

        let loading_label = Label::builder()
            .label("Gemini is thinking...")
            .css_classes(["loading-text"])
            .build();

        let loading_sub = Label::builder()
            .label("Spawning secure terminal session")
            .css_classes(["loading-subtext"])
            .build();

        if let Some(img) = logo_image {
            loading_box.append(&img);
        }
        loading_box.append(&loading_label);
        loading_box.append(&loading_sub);

        // Terminal Container
        let scrolled = ScrolledWindow::builder()
            .hscrollbar_policy(gtk4::PolicyType::Never)
            .vscrollbar_policy(gtk4::PolicyType::Automatic)
            .child(&terminal)
            .css_classes(["terminal-container"])
            .build();

        stack.add_named(&loading_box, Some("loading"));
        stack.add_named(&scrolled, Some("terminal"));
        stack.set_visible_child_name("loading");

        container.append(&stack);

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
        info!("Terminal configured, setting up hotkeys");

        // Keyboard Shortcuts (Copy/Paste/Zoom)
        let key_controller = gtk4::EventControllerKey::new();
        let term_clone = terminal.clone();
        key_controller.connect_key_pressed(move |_ctrl, key, _code, state| {
            let is_ctrl = state.contains(gtk4::gdk::ModifierType::CONTROL_MASK);
            let is_shift = state.contains(gtk4::gdk::ModifierType::SHIFT_MASK);

            match key {
                gtk4::gdk::Key::C | gtk4::gdk::Key::c if is_ctrl && is_shift => {
                    debug!("Hotkey: Copy");
                    term_clone.copy_clipboard_format(Format::Text);
                    glib::Propagation::Stop
                }
                gtk4::gdk::Key::V | gtk4::gdk::Key::v if is_ctrl && is_shift => {
                    debug!("Hotkey: Paste");
                    term_clone.paste_clipboard();
                    glib::Propagation::Stop
                }
                gtk4::gdk::Key::plus | gtk4::gdk::Key::equal if is_ctrl => {
                    let scale = term_clone.font_scale();
                    debug!("Hotkey: Zoom In (new scale: {})", scale + 0.1);
                    term_clone.set_font_scale(scale + 0.1);
                    glib::Propagation::Stop
                }
                gtk4::gdk::Key::minus if is_ctrl => {
                    let scale = term_clone.font_scale();
                    debug!("Hotkey: Zoom Out (new scale: {})", (scale - 0.1).max(0.1));
                    term_clone.set_font_scale((scale - 0.1).max(0.1));
                    glib::Propagation::Stop
                }
                k if k.to_unicode() == Some('0') && is_ctrl => {
                    debug!("Hotkey: Zoom Reset");
                    term_clone.set_font_scale(1.0);
                    glib::Propagation::Stop
                }
                _ => glib::Propagation::Proceed,
            }
        });
        terminal.add_controller(key_controller);

        // Close window when the terminal child exits (e.g., user exits gemini)
        let obj_clone = obj.clone();
        terminal.connect_child_exited(move |_, status| {
            info!("Terminal child exited with status: {}", status);
            obj_clone.close();
        });

        // Dynamic Shell Detection
        let shell = env::var("SHELL").unwrap_or_else(|_| "/bin/sh".to_string());
        let home_dir = env::var("HOME").unwrap_or_else(|_| "/".to_string());

        let command = get_startup_command(true);
        info!("Spawning terminal with shell: {}, command: {:?}", shell, command);

        let obj_clone = obj.clone();
        let stack_clone = stack.clone();
        
        // Connect to contents-changed to detect when the command actually starts printing
        terminal.connect_contents_changed(move |_| {
            if stack_clone.visible_child_name().as_deref() == Some("loading") {
                debug!("Terminal content detected, switching from loading screen");
                stack_clone.set_visible_child_name("terminal");
            }
        });

        let stack_for_spawn = stack.clone();
        terminal.spawn_async(
            PtyFlags::DEFAULT,
            Some(&home_dir),
            &[&shell, command[0], command[1]],
            &[],
            glib::SpawnFlags::DEFAULT,
            || {},
            -1,
            None::<&gtk4::gio::Cancellable>,
            move |result| {
                match result {
                    Ok(_) => {
                        info!("Terminal process spawned, waiting for content...");
                        // We no longer switch here; we wait for contents_changed
                    }
                    Err(err) => {
                        error!("Error spawning terminal: {}", err);
                        stack_for_spawn.set_visible_child_name("terminal"); // Show terminal anyway so error is visible
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
                }
            },
        );
    }

    /// Sets up the welcome screen using AdwStatusPage.
    fn setup_welcome_ui(&self, container: &Box) {
        let obj = self.obj();
        
        let status_page = adw::StatusPage::builder()
            .title("Welcome to Gemini Terminal")
            .description("The Gemini CLI was not detected on your system. We checked your PATH and interactive shell environment (-ic).\n\nTo get started, please install it using npm:\nnpm install -g @google/gemini-cli\n\nThen configure it:\ngemini configure")
            .icon_name("utilities-terminal-symbolic")
            .vexpand(true)
            .build();

        let refresh_button = Button::builder()
            .label("Check for Gemini again")
            .halign(Align::Center)
            .margin_top(20)
            .css_classes(["suggested-action"])
            .build();

        let obj_clone = obj.clone();
        refresh_button.connect_clicked(move |_| {
            let imp = obj_clone.imp();
            imp.setup_ui();
        });

        status_page.set_child(Some(&refresh_button));
        container.append(&status_page);
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
