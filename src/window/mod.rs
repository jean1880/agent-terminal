//! Main window implementation for Antigravity Terminal.

mod imp;

use gtk4::gio;
use gtk4::glib;

glib::wrapper! {
    /// The main application window for Antigravity Terminal.
    pub struct AntigravityWindow(ObjectSubclass<imp::AntigravityWindow>)
        // gtk4::ApplicationWindow belongs in this chain: AdwApplicationWindow
        // derives from it. Omitting it left the type without
        // IsA<gtk4::ApplicationWindow>, which ApplicationWindowImpl and
        // AdwApplicationWindowImpl both require.
        @extends adw::ApplicationWindow, gtk4::ApplicationWindow, gtk4::Window, gtk4::Widget,
        @implements gio::ActionGroup, gio::ActionMap, gtk4::Accessible, gtk4::Buildable, gtk4::ConstraintTarget, gtk4::Native, gtk4::Root, gtk4::ShortcutManager;
}

impl AntigravityWindow {
    /// Creates a new AntigravityWindow instance.
    pub fn new(app: &adw::Application) -> Self {
        glib::Object::builder().property("application", app).build()
    }
}
