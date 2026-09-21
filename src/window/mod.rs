//! Main window implementation for Agent Terminal.

mod imp;

use gtk4::gio;
use gtk4::glib;

glib::wrapper! {
    /// The main application window for Agent Terminal.
    pub struct AgentTerminalWindow(ObjectSubclass<imp::AgentTerminalWindow>)
        // gtk4::ApplicationWindow belongs in this chain: AdwApplicationWindow
        // derives from it. Omitting it left the type without
        // IsA<gtk4::ApplicationWindow>, which ApplicationWindowImpl and
        // AdwApplicationWindowImpl both require.
        @extends adw::ApplicationWindow, gtk4::ApplicationWindow, gtk4::Window, gtk4::Widget,
        @implements gio::ActionGroup, gio::ActionMap, gtk4::Accessible, gtk4::Buildable, gtk4::ConstraintTarget, gtk4::Native, gtk4::Root, gtk4::ShortcutManager;
}

impl AgentTerminalWindow {
    /// Creates a new AgentTerminalWindow instance.
    pub fn new(app: &adw::Application) -> Self {
        glib::Object::builder().property("application", app).build()
    }

    /// Opens a tab resuming `session_id`, once the window is ready for it.
    ///
    /// `session_id` must already have passed
    /// [`crate::utils::validate_session_id`]. `dir` overrides the session-store
    /// lookup of where the session was recorded.
    pub fn resume_session(&self, session_id: String, dir: Option<String>) {
        use gtk4::subclass::prelude::ObjectSubclassIsExt;
        self.imp()
            .request_resume(imp::ResumeRequest { session_id, dir });
    }
}
