//! Main window implementation for Gemini Terminal.

mod imp;

use gtk4::gio;
use gtk4::glib;

glib::wrapper! {
    /// The main application window for Gemini Terminal.
    pub struct GeminiWindow(ObjectSubclass<imp::GeminiWindow>)
        @extends adw::ApplicationWindow, gtk4::Window, gtk4::Widget,
        @implements gio::ActionGroup, gio::ActionMap, gtk4::Accessible, gtk4::Buildable, gtk4::ConstraintTarget, gtk4::Native, gtk4::Root, gtk4::ShortcutManager;
}

impl GeminiWindow {
    /// Creates a new GeminiWindow instance.
    pub fn new(app: &adw::Application) -> Self {
        glib::Object::builder().property("application", app).build()
    }
}
