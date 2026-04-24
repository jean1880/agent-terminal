use gtk::prelude::*;
use gtk::{Application, ApplicationWindow};
use vte::prelude::*;
use std::env;

const APP_ID: &str = "com.jdesroches.GeminiTerminal";

fn main() -> glib::ExitCode {
    let app = Application::builder()
        .application_id(APP_ID)
        .build();

    app.connect_activate(build_ui);
    app.run()
}

fn build_ui(app: &Application) {
    let window = ApplicationWindow::builder()
        .application(app)
        .title("Gemini Terminal")
        .default_width(900)
        .default_height(600)
        .build();

    let terminal = vte::Terminal::new();

    // Colors from Gemini Profile
    let bg_color = gdk::RGBA::parse("rgb(24,20,37)").unwrap();
    let fg_color = gdk::RGBA::parse("rgb(200,200,255)").unwrap();
    let bold_color = gdk::RGBA::parse("rgb(142,117,255)").unwrap();

    terminal.set_colors(Some(&fg_color), Some(&bg_color), &[]);
    terminal.set_color_bold(Some(&bold_color));
    
    terminal.set_cursor_blink_mode(vte::CursorBlinkMode::On);
    terminal.set_cursor_shape(vte::CursorShape::Block);
    terminal.set_scrollback_lines(10000);

    // Command to run
    let shell = "/bin/zsh";
    let command = [
        "-ic",
        "/home/jdesroches/scripts/gemini-terminal-launcher.sh; exec zsh",
    ];

    terminal.spawn_async(
        vte::PtyFlags::DEFAULT,
        env::var_os("HOME").as_deref(),
        &[shell, command[0], command[1]],
        &[],
        glib::SpawnFlags::DO_NOT_REAP_CHILD,
        None::<fn()>,
        -1,
        None::<&glib::Cancellable>,
        move |_terminal, _pid, _error| {
            if let Some(err) = _error {
                eprintln!("Error spawning terminal: {}", err);
            }
        },
    );

    terminal.connect_child_exited(move |_, _| {
        glib::ExitCode::SUCCESS; // Just a placeholder, we quit via app
        // In GTK4 we often just close the window
    });

    // ScrolledWindow for the terminal
    let scrolled = gtk::ScrolledWindow::builder()
        .hscrollbar_policy(gtk::PolicyType::Never)
        .vscrollbar_policy(gtk::PolicyType::Automatic)
        .child(&terminal)
        .build();

    window.set_child(Some(&scrolled));
    window.present();
}
