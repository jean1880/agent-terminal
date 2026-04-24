use gtk::prelude::*;
use gtk::{Application, ApplicationWindow, HeaderBar, ScrolledWindow, CssProvider};
use vte::prelude::*;
use std::env;

use gtk::glib;
use gtk::gio;
use gtk::gdk;

const APP_ID: &str = "com.jdesroches.GeminiTerminal";

fn main() -> glib::ExitCode {
    let app = Application::builder()
        .application_id(APP_ID)
        .build();

    app.connect_startup(|_| {
        load_css();
    });

    app.connect_activate(build_ui);
    app.run()
}

fn load_css() {
    let provider = CssProvider::new();
    provider.load_from_data("
        window {
            background-color: #181425;
        }
        headerbar {
            background-color: #120f1d;
            color: #c8c8ff;
            border-bottom: 1px solid #2d2444;
        }
        .terminal-container {
            padding: 10px;
        }
    ");

    gtk::style_context_add_provider_for_display(
        &gdk::Display::default().expect("Could not connect to a display."),
        &provider,
        gtk::STYLE_PROVIDER_PRIORITY_APPLICATION,
    );
}

fn build_ui(app: &Application) {
    let window = ApplicationWindow::builder()
        .application(app)
        .default_width(950)
        .default_height(650)
        .build();

    // Modern HeaderBar
    let header = HeaderBar::builder()
        .title_widget(&gtk::Label::new(Some("Gemini Terminal")))
        .show_title_buttons(true)
        .build();
    
    window.set_titlebar(Some(&header));

    let terminal = vte::Terminal::new();

    // Gemini Theme Colors
    let bg_color = gdk::RGBA::parse("rgb(24,20,37)").unwrap();
    let fg_color = gdk::RGBA::parse("rgb(200,200,255)").unwrap();
    let bold_color = gdk::RGBA::parse("rgb(142,117,255)").unwrap();

    terminal.set_colors(Some(&fg_color), Some(&bg_color), &[]);
    terminal.set_color_bold(Some(&bold_color));
    
    terminal.set_cursor_blink_mode(vte::CursorBlinkMode::On);
    terminal.set_cursor_shape(vte::CursorShape::Block);
    terminal.set_scrollback_lines(10000);
    terminal.set_enable_sixel(true);

    // Keyboard Shortcuts (Copy/Paste/Zoom)
    let key_controller = gtk::EventControllerKey::new();
    let term_clone = terminal.clone();
    key_controller.connect_key_pressed(move |_ctrl, key, _code, state| {
        let is_ctrl = state.contains(gdk::ModifierType::CONTROL_MASK);
        let is_shift = state.contains(gdk::ModifierType::SHIFT_MASK);

        match key.name().as_deref() {
            Some("C") if is_ctrl && is_shift => {
                term_clone.copy_clipboard_format(vte::Format::Text);
                glib::Propagation::Stop
            }
            Some("V") if is_ctrl && is_shift => {
                term_clone.paste_clipboard();
                glib::Propagation::Stop
            }
            Some("plus") | Some("equal") if is_ctrl => {
                let scale = term_clone.font_scale();
                term_clone.set_font_scale(scale + 0.1);
                glib::Propagation::Stop
            }
            Some("minus") if is_ctrl => {
                let scale = term_clone.font_scale();
                term_clone.set_font_scale((scale - 0.1).max(0.1));
                glib::Propagation::Stop
            }
            Some("0") if is_ctrl => {
                term_clone.set_font_scale(1.0);
                glib::Propagation::Stop
            }
            _ => glib::Propagation::Proceed,
        }
    });
    terminal.add_controller(key_controller);

    // Command Execution
    let shell = "/bin/zsh";
    let command = ["-ic", "/home/jdesroches/scripts/gemini-terminal-launcher.sh; exec zsh"];
    let home_dir = env::var("HOME").unwrap_or_else(|_| "/".to_string());

    terminal.spawn_async(
        vte::PtyFlags::DEFAULT,
        Some(&home_dir),
        &[shell, command[0], command[1]],
        &[],
        glib::SpawnFlags::DO_NOT_REAP_CHILD,
        || {},
        -1,
        None::<&gio::Cancellable>,
        move |result| {
            if let Err(err) = result {
                eprintln!("Error spawning terminal: {}", err);
            }
        },
    );

    let scrolled = ScrolledWindow::builder()
        .hscrollbar_policy(gtk::PolicyType::Never)
        .vscrollbar_policy(gtk::PolicyType::Automatic)
        .child(&terminal)
        .css_classes(["terminal-container"])
        .build();

    window.set_child(Some(&scrolled));
    window.present();
}
