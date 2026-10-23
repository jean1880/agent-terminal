//! Window test fixtures: a registered application with a private config home, and a window
//! whose shell is up. GTK and libadwaita are initialised by the caller, first.

use gtk4::glib;
use gtk4::prelude::*;
use gtk4::subclass::prelude::ObjectSubclassIsExt;

use crate::window::AgentTerminalWindow;

/// A registered, non-unique application, with `XDG_CONFIG_HOME` on a temporary folder that
/// outlives it. Fields drop in order: the hold, the application, then the folder.
pub(crate) struct TestApp {
    _hold: Option<gtk4::gio::ApplicationHoldGuard>,
    app: adw::Application,
    config: tempfile::TempDir,
}

impl TestApp {
    /// The temporary config home.
    pub(crate) fn config_dir(&self) -> &std::path::Path {
        self.config.path()
    }
}

impl std::ops::Deref for TestApp {
    type Target = adw::Application;

    fn deref(&self) -> &adw::Application {
        &self.app
    }
}

/// The application `id`, registered, held when `hold` (so its windows' lifecycle does not
/// depend on how many are open).
pub(crate) fn test_app(id: &str, hold: bool) -> TestApp {
    let config = tempfile::tempdir().unwrap();
    std::env::set_var("XDG_CONFIG_HOME", config.path());
    let app = adw::Application::builder()
        .application_id(id)
        .flags(gtk4::gio::ApplicationFlags::NON_UNIQUE)
        .build();
    app.register(None::<&gtk4::gio::Cancellable>).unwrap();
    TestApp {
        _hold: hold.then(|| app.hold()),
        app,
        config,
    }
}

/// A new window of `app`, once its tab view exists (at most 5 s of `ctx`).
pub(crate) fn test_window(app: &adw::Application, ctx: &glib::MainContext) -> AgentTerminalWindow {
    let window = AgentTerminalWindow::new(app);
    assert!(crate::testutil::pump_until(ctx, 5, || window
        .imp()
        .tab_view
        .borrow()
        .is_some()));
    window
}
