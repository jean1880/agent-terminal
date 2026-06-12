//! Private implementation details of the AntigravityWindow.

use crate::utils::{detect_cli_binary, get_startup_command};
use adw::prelude::*;
use adw::subclass::prelude::*;
use gtk4::glib;
use gtk4::{Align, Box, Button, Image, Label, Orientation, ScrolledWindow, Stack};
use std::cell::RefCell;
use std::env;
use tracing::{debug, error, info, warn};
use vte4::prelude::*;
use vte4::{CursorBlinkMode, CursorShape, Format, PtyFlags, Terminal};

/// Static logo SVG for standalone binary.
const LOGO_SVG: &str = include_str!("../../assets/antigravity_logo.svg");

/// Internal state for the AntigravityWindow.
#[derive(Default)]
pub struct AntigravityWindow {
    pub terminal: RefCell<Option<Terminal>>,
    pub header: RefCell<Option<adw::HeaderBar>>,
    pub window_title: RefCell<Option<adw::WindowTitle>>,
    pub loading_stack: RefCell<Option<Stack>>,
    pub config: RefCell<crate::config::TerminalConfig>,
}

#[glib::object_subclass]
impl ObjectSubclass for AntigravityWindow {
    const NAME: &'static str = "AntigravityWindow";
    type Type = super::AntigravityWindow;
    type ParentType = adw::ApplicationWindow;
}

impl ObjectImpl for AntigravityWindow {
    fn constructed(&self) {
        self.parent_constructed();
        *self.config.borrow_mut() = crate::config::TerminalConfig::load();
        self.setup_ui();
        self.setup_actions();
    }
}

impl WidgetImpl for AntigravityWindow {}
impl WindowImpl for AntigravityWindow {}
impl ApplicationWindowImpl for AntigravityWindow {}
impl AdwApplicationWindowImpl for AntigravityWindow {}

impl AntigravityWindow {
    /// Sets up GAction handlers for context menu items.
    fn setup_actions(&self) {
        let obj = self.obj();

        // Copy Action
        let copy_action = gtk4::gio::SimpleAction::new("copy", None);
        copy_action.connect_activate(glib::clone!(@weak obj => move |_, _| {
            let imp = obj.imp();
            let terminal_borrow = imp.terminal.borrow();
            if let Some(terminal) = terminal_borrow.as_ref() {
                debug!("Action: Copy");
                terminal.copy_clipboard_format(Format::Text);
            }
        }));
        obj.add_action(&copy_action);

        // Paste Action
        let paste_action = gtk4::gio::SimpleAction::new("paste", None);
        paste_action.connect_activate(glib::clone!(@weak obj => move |_, _| {
            let imp = obj.imp();
            let terminal_borrow = imp.terminal.borrow();
            if let Some(terminal) = terminal_borrow.as_ref() {
                debug!("Action: Paste");
                terminal.paste_clipboard();
            }
        }));
        obj.add_action(&paste_action);
    }

    /// Initializes the user interface, switching between terminal and welcome screen.
    fn setup_ui(&self) {
        let obj = self.obj();
        debug!("Setting up UI for Antigravity Terminal");

        obj.set_default_width(950);
        obj.set_default_height(650);
        obj.set_title(Some("Antigravity Terminal"));

        let content = Box::builder().orientation(Orientation::Vertical).build();

        // Modern AdwHeaderBar
        let window_title = adw::WindowTitle::new("Antigravity Terminal", "");
        let header = adw::HeaderBar::builder()
            .title_widget(&window_title)
            .build();

        let settings_btn = gtk4::Button::builder()
            .icon_name("document-properties-symbolic")
            .tooltip_text("Settings")
            .build();
        settings_btn.connect_clicked(glib::clone!(@weak obj => move |_| {
            let imp = obj.imp();
            imp.show_preferences();
        }));
        header.pack_end(&settings_btn);

        content.append(&header);
        *self.header.borrow_mut() = Some(header);
        *self.window_title.borrow_mut() = Some(window_title);

        // Show a temporary "Detecting" state
        let status_page = adw::StatusPage::builder()
            .title("Initializing...")
            .description("Checking for Antigravity CLI environment...")
            .icon_name("view-refresh-symbolic")
            .vexpand(true)
            .build();

        content.append(&status_page);
        obj.set_content(Some(&content));

        // Use spawn_local to handle UI state without leaving the main thread
        let path = env::var("PATH").ok();
        let home = env::var("HOME").ok();
        let shell = env::var("SHELL").ok();

        glib::MainContext::default().spawn_local(
            glib::clone!(@weak obj, @weak content => async move {
                // Give the UI one frame to render the "Initializing" screen
                glib::timeout_future(std::time::Duration::from_millis(10)).await;

                // Perform the detection. We do this here as it's the simplest way
                // to keep obj/content on the main thread.
                let detected = detect_cli_binary(path, home, shell);

                let imp = obj.imp();
                content.remove(&status_page);

                if let Some(ref binary) = detected {
                    info!("CLI binary '{}' detected, setting up terminal UI", binary);
                    imp.setup_terminal_ui(&content, Some(binary));
                } else {
                    warn!("No compatible CLI detected, setting up welcome UI");
                    imp.setup_welcome_ui(&content);
                }
            }),
        );
    }

    /// Sets up the terminal interface.
    fn setup_terminal_ui(&self, container: &Box, cli_binary: Option<&str>) {
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
            let load_result = loader
                .write(LOGO_SVG.as_bytes())
                .and_then(|_| loader.close())
                .and_then(|_| {
                    loader.pixbuf().ok_or(glib::Error::new(
                        gtk4::gio::IOErrorEnum::Failed,
                        "Failed to get pixbuf",
                    ))
                });

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
                    error!("Failed to load embedded logo: {}", e);
                    None
                }
            }
        } else {
            error!("SVG PixbufLoader not available");
            None
        };

        let loading_label = Label::builder()
            .label("Antigravity is calculating...")
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

        // Terminal Theme Colors (Antigravity Theme)
        let bg_color = gtk4::gdk::RGBA::parse("rgb(24,20,37)").unwrap_or(gtk4::gdk::RGBA::BLACK);
        let fg_color = gtk4::gdk::RGBA::parse("rgb(200,200,255)").unwrap_or(gtk4::gdk::RGBA::WHITE);
        let bold_color = gtk4::gdk::RGBA::parse("rgb(180,155,255)").unwrap_or(fg_color);
        let cursor_color = gtk4::gdk::RGBA::parse("rgb(180,155,255)").unwrap_or(fg_color);
        let normal_black = gtk4::gdk::RGBA::parse("rgb(45,40,62)").unwrap(); // Visible dark grey-violet
        let highlight_bg = gtk4::gdk::RGBA::parse("rgb(68,58,94)").unwrap(); // Medium purple for selection
        let highlight_fg = gtk4::gdk::RGBA::parse("rgb(230,230,255)").unwrap(); // Bright selection text

        // Harmonious pastel 16-color ANSI palette
        let c1 = gtk4::gdk::RGBA::parse("rgb(255,120,120)").unwrap(); // Red (pastel red, adjusted for contrast)
        let c2 = gtk4::gdk::RGBA::parse("rgb(78,232,176)").unwrap(); // Green (mint green)
        let c3 = gtk4::gdk::RGBA::parse("rgb(255,224,102)").unwrap(); // Yellow (warm yellow)
        let c4 = gtk4::gdk::RGBA::parse("rgb(116,192,252)").unwrap(); // Blue (sky blue)
        let c5 = bold_color; // Magenta (accent violet)
        let c6 = gtk4::gdk::RGBA::parse("rgb(102,217,232)").unwrap(); // Cyan (pastel cyan)

        let b0 = gtk4::gdk::RGBA::parse("rgb(170,162,185)").unwrap(); // Bright Black (lavender-grey for comments/details, adjusted for contrast)
        let b1 = gtk4::gdk::RGBA::parse("rgb(255,135,135)").unwrap(); // Bright Red
        let b2 = gtk4::gdk::RGBA::parse("rgb(99,241,195)").unwrap(); // Bright Green
        let b3 = gtk4::gdk::RGBA::parse("rgb(255,236,153)").unwrap(); // Bright Yellow
        let b4 = gtk4::gdk::RGBA::parse("rgb(165,216,255)").unwrap(); // Bright Blue
        let b5 = gtk4::gdk::RGBA::parse("rgb(208,191,255)").unwrap(); // Bright Magenta
        let b6 = gtk4::gdk::RGBA::parse("rgb(154,230,242)").unwrap(); // Bright Cyan
        let b7 = gtk4::gdk::RGBA::parse("rgb(230,230,255)").unwrap(); // Bright White

        let palette = [
            &normal_black,
            &c1,
            &c2,
            &c3,
            &c4,
            &c5,
            &c6,
            &fg_color,
            &b0,
            &b1,
            &b2,
            &b3,
            &b4,
            &b5,
            &b6,
            &b7,
        ];

        terminal.set_colors(Some(&fg_color), Some(&bg_color), &palette);
        terminal.set_color_bold(Some(&bold_color));
        terminal.set_color_cursor(Some(&cursor_color));
        terminal.set_color_highlight(Some(&highlight_bg));
        terminal.set_color_highlight_foreground(Some(&highlight_fg));

        // High-quality developer monospace font
        let font_desc =
            gtk4::pango::FontDescription::from_string("JetBrains Mono, Fira Code, Monospace 11");
        terminal.set_font(Some(&font_desc));

        terminal.set_cursor_blink_mode(CursorBlinkMode::On);
        terminal.set_cursor_shape(CursorShape::Block);
        
        let scrollback = self.config.borrow().scrollback_lines;
        terminal.set_scrollback_lines(scrollback as i64);
        
        let font_scale = self.config.borrow().font_scale;
        terminal.set_font_scale(font_scale);
        
        terminal.set_enable_sixel(true);
        terminal.set_allow_hyperlink(true);
        info!("Terminal configured, setting up controllers and signals");

        // Window Title Handling (Session Info)
        terminal.connect_window_title_changed(glib::clone!(@weak obj => move |terminal| {
            let title = terminal.window_title();
            debug!("Terminal window title changed: {:?}", title);
            let imp = obj.imp();
            let window_title_borrow = imp.window_title.borrow();

            if let Some(window_title) = window_title_borrow.as_ref() {
                let session_info = title.as_deref().unwrap_or("");
                window_title.set_subtitle(session_info);

                // Also update the main GtkWindow title so GNOME Shell sees it
                if !session_info.is_empty() {
                    obj.set_title(Some(&format!("Antigravity Terminal — {}", session_info)));
                } else {
                    obj.set_title(Some("Antigravity Terminal"));
                }
            }
        }));

        // Context Menu (Right Click)
        let menu = gtk4::gio::Menu::new();
        menu.append(Some("New Window"), Some("app.new-window"));

        let section = gtk4::gio::Menu::new();
        section.append(Some("Copy"), Some("win.copy"));
        section.append(Some("Paste"), Some("win.paste"));
        menu.append_section(None, &section);

        let popover = gtk4::PopoverMenu::builder()
            .menu_model(&menu)
            .has_arrow(false)
            .build();
        popover.set_parent(&terminal);

        let click_gesture = gtk4::GestureClick::new();
        click_gesture.set_button(3); // Right click
        click_gesture.connect_pressed(glib::clone!(@weak popover => move |gesture, _, x, y| {
            gesture.set_state(gtk4::EventSequenceState::Claimed);
            let rect = gtk4::gdk::Rectangle::new(x as i32, y as i32, 1, 1);
            popover.set_pointing_to(Some(&rect));
            popover.popup();
        }));
        terminal.add_controller(click_gesture);

        // Hyperlink Click Handler (Ctrl + Left Click)
        let link_click_gesture = gtk4::GestureClick::new();
        link_click_gesture.set_button(1); // Left click
        link_click_gesture.connect_pressed(glib::clone!(@weak terminal => move |gesture, _, _x, _y| {
            if let Some(event) = gesture.current_event() {
                let modifiers = event.modifier_state();
                if modifiers.contains(gtk4::gdk::ModifierType::CONTROL_MASK) {
                    if let Some(uri) = terminal.hyperlink_hover_uri() {
                        gesture.set_state(gtk4::EventSequenceState::Claimed);
                        debug!("Opening hyperlink: {}", uri);
                        gtk4::show_uri(None::<&gtk4::Window>, &uri, 0);
                    }
                }
            }
        }));
        terminal.add_controller(link_click_gesture);

        // Keyboard Shortcuts (Copy/Paste/Zoom)
        let key_controller = gtk4::EventControllerKey::new();
        key_controller.connect_key_pressed(glib::clone!(@weak terminal => @default-return glib::Propagation::Proceed, move |_ctrl, key, _code, state| {
            let is_ctrl = state.contains(gtk4::gdk::ModifierType::CONTROL_MASK);
            let is_shift = state.contains(gtk4::gdk::ModifierType::SHIFT_MASK);

            match key {
                gtk4::gdk::Key::C | gtk4::gdk::Key::c if is_ctrl && is_shift => {
                    debug!("Hotkey: Copy");
                    terminal.copy_clipboard_format(Format::Text);
                    glib::Propagation::Stop
                }
                gtk4::gdk::Key::V | gtk4::gdk::Key::v if is_ctrl && is_shift => {
                    debug!("Hotkey: Paste");
                    terminal.paste_clipboard();
                    glib::Propagation::Stop
                }
                gtk4::gdk::Key::plus | gtk4::gdk::Key::equal if is_ctrl => {
                    let scale = terminal.font_scale();
                    debug!("Hotkey: Zoom In (new scale: {})", scale + 0.1);
                    terminal.set_font_scale(scale + 0.1);
                    glib::Propagation::Stop
                }
                gtk4::gdk::Key::minus if is_ctrl => {
                    let scale = terminal.font_scale();
                    debug!("Hotkey: Zoom Out (new scale: {})", (scale - 0.1).max(0.1));
                    terminal.set_font_scale((scale - 0.1).max(0.1));
                    // Optional: save scale to config?
                    glib::Propagation::Stop
                }
                k if k.to_unicode() == Some('0') && is_ctrl => {
                    debug!("Hotkey: Zoom Reset");
                    terminal.set_font_scale(1.0);
                    glib::Propagation::Stop
                }
                _ => glib::Propagation::Proceed,
            }
        }));
        terminal.add_controller(key_controller);

        // Close window when the terminal child exits (e.g., user exits agy)
        terminal.connect_child_exited(glib::clone!(@weak obj => move |_, status| {
            info!("Terminal child exited with status: {}", status);
            obj.close();
        }));

        // Dynamic Shell Detection
        let shell = env::var("SHELL").unwrap_or_else(|_| "/bin/sh".to_string());
        let home_dir = env::var("HOME").unwrap_or_else(|_| "/".to_string());

        let config = self.config.borrow().clone();
        let command = get_startup_command(cli_binary, &config);
        info!(
            "Spawning terminal with shell: {}, command: {:?}",
            shell, command
        );

        // Connect to contents-changed to detect when the command actually starts printing
        terminal.connect_contents_changed(glib::clone!(@weak stack => move |_| {
            if stack.visible_child_name().as_deref() == Some("loading") {
                debug!("Terminal content detected, switching from loading screen");
                stack.set_visible_child_name("terminal");
            }
        }));

        // Inherit the current user environment
        let env_vars = glib::environ();
        let env_strs: Vec<String> = env_vars
            .iter()
            .map(|os| os.to_string_lossy().to_string())
            .collect();
        let env_ptrs: Vec<&str> = env_strs.iter().map(|s| s.as_str()).collect();

        terminal.spawn_async(
            PtyFlags::DEFAULT,
            Some(&home_dir),
            &[&shell, &command[0], &command[1]],
            &env_ptrs,
            glib::SpawnFlags::DEFAULT,
            || {},
            -1,
            None::<&gtk4::gio::Cancellable>,
            glib::clone!(@weak obj, @weak stack => move |result| {
                match result {
                    Ok(_) => {
                        info!("Terminal process spawned, waiting for content...");
                    }
                    Err(err) => {
                        error!("Error spawning terminal: {}", err);
                        stack.set_visible_child_name("terminal"); // Show terminal anyway so error is visible
                        let dialog = gtk4::MessageDialog::builder()
                            .transient_for(&obj)
                            .message_type(gtk4::MessageType::Error)
                            .buttons(gtk4::ButtonsType::Ok)
                            .text("Terminal Error")
                            .secondary_text(format!("Error spawning terminal: {}", err))
                            .build();
                        dialog.connect_response(|dialog, _| dialog.close());
                        dialog.present();
                    }
                }
            }),
        );
    }

    /// Sets up the welcome screen using AdwStatusPage.
    fn setup_welcome_ui(&self, container: &Box) {
        let obj = self.obj();

        let status_page = adw::StatusPage::builder()
            .title("Welcome to Antigravity Terminal")
            .description("The Antigravity CLI was not detected on your system. We checked your PATH and interactive shell environment (-ic).\n\nTo get started, please install it using the official script:\ncurl -fsSL https://antigravity.google/cli/install.sh | bash\n\nThen launch it:\nagy")
            .icon_name("utilities-terminal-symbolic")
            .vexpand(true)
            .build();

        let refresh_button = Button::builder()
            .label("Check for agy again")
            .halign(Align::Center)
            .margin_top(20)
            .css_classes(["suggested-action"])
            .build();

        refresh_button.connect_clicked(glib::clone!(@weak obj => move |_| {
            let imp = obj.imp();
            imp.setup_ui();
        }));

        status_page.set_child(Some(&refresh_button));
        container.append(&status_page);
    }

    fn show_preferences(&self) {
        let obj = self.obj();
        let config = self.config.borrow().clone();

        let window = adw::PreferencesWindow::builder()
            .transient_for(obj.upcast_ref::<gtk4::Window>())
            .modal(true)
            .search_enabled(false)
            .title("Settings")
            .build();

        let page = adw::PreferencesPage::new();
        let group = adw::PreferencesGroup::new();
        group.set_title("Terminal Preferences");

        let startup_script_entry = gtk4::Entry::builder()
            .text(&config.startup_script)
            .hexpand(true)
            .valign(gtk4::Align::Center)
            .build();
            
        let startup_script_row = adw::ActionRow::builder()
            .title("Startup Script Path")
            .build();
        startup_script_row.add_suffix(&startup_script_entry);

        let scroll_adj = gtk4::Adjustment::new(config.scrollback_lines as f64, 100.0, 100000.0, 100.0, 1000.0, 0.0);
        let scroll_spin = gtk4::SpinButton::builder()
            .adjustment(&scroll_adj)
            .valign(gtk4::Align::Center)
            .build();
            
        let scrollback_row = adw::ActionRow::builder()
            .title("Scrollback Lines")
            .build();
        scrollback_row.add_suffix(&scroll_spin);

        group.add(&startup_script_row);
        group.add(&scrollback_row);
        page.add(&group);
        window.add(&page);

        window.connect_close_request(glib::clone!(@weak obj => @default-return glib::Propagation::Proceed, move |_win| {
            let imp = obj.imp();
            let mut current_config = imp.config.borrow_mut();
            current_config.startup_script = startup_script_entry.text().to_string();
            current_config.scrollback_lines = scroll_spin.value() as u32;
            current_config.save();
            
            // To apply dynamically, we'd adjust terminal here. Scrollback can be updated:
            if let Some(term) = imp.terminal.borrow().as_ref() {
                term.set_scrollback_lines(current_config.scrollback_lines as i64);
            }
            glib::Propagation::Proceed
        }));

        window.present();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn init_gtk() {
        if !gtk4::is_initialized_main_thread() {
            gtk4::init().expect("GTK init failed");
        }
    }

    #[test]
    fn test_window_initialization() {
        init_gtk();
        let app = adw::Application::builder()
            .application_id("org.test.Window")
            .build();
        let window = super::super::AntigravityWindow::new(&app);

        assert_eq!(window.title(), Some("Antigravity Terminal".into()));
    }
}
