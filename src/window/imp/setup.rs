//! The first-start walkthrough: a welcome, then the agents (which are installed, which to use,
//! who is signed in, and which new threads start on).
//!
//! Offered once per process when no settings file existed at launch
//! ([`crate::config::TerminalConfig::is_first_run`]), and from Settings → General → Setup at any
//! time. Its rows are the Agents page's own, so they apply on change like every setting; closing
//! it saves the settings, which is what makes the next start not a first one.

use agent_core::adapter::Driver;

use super::agents_prefs::APPROVAL_GROUP_HELP;
use super::*;

thread_local! {
    /// Set once the walkthrough has been offered, so a second window does not offer it again.
    static SETUP_OFFERED: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

thread_local! {
    /// Set once a walkthrough has closed. Until then a first run holds back the startup update
    /// check, which the walkthrough starts when it closes.
    static SETUP_FINISHED: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

/// Whether a walkthrough has closed in this process.
pub(super) fn setup_finished() -> bool {
    SETUP_FINISHED.with(std::cell::Cell::get)
}

/// The navigation tag of the agents page.
const AGENTS_TAG: &str = "agents";

impl AgentTerminalWindow {
    /// Shows the walkthrough if this is a first start and it has not been offered yet.
    pub(super) fn offer_setup_on_first_run(&self) {
        if !self.config.borrow().is_first_run() || SETUP_OFFERED.with(|s| s.replace(true)) {
            return;
        }
        info!("No settings file at launch: offering the setup walkthrough");
        self.show_setup();
    }

    /// Shows the walkthrough.
    pub(super) fn show_setup(&self) {
        let obj = self.obj();
        let dialog = adw::Dialog::builder()
            .title("Set Up Agent Terminal")
            .content_width(560)
            .content_height(640)
            .build();

        let nav = adw::NavigationView::new();
        // The agents page waits in the pool; the welcome is the root, shown first.
        nav.add(&self.setup_agents_page(&dialog));
        nav.replace(&[self.setup_welcome_page(&nav)]);
        dialog.set_child(Some(&nav));

        dialog.connect_closed(glib::clone!(
            #[weak]
            obj,
            move |_| {
                let imp = obj.imp();
                // Written even when nothing changed: the file existing is what ends "first run".
                // At once, not debounced, so a failure can be said while the window is up.
                if !imp.save_config_now() {
                    imp.show_toast(
                        "Your settings could not be saved. Setup will return next start.",
                    );
                }
                // A first run held back the startup update check until now; it reads the switch
                // as set here. (A later window, or a re-run from Settings, finds it done.)
                SETUP_FINISHED.with(|f| f.set(true));
                imp.check_updates_on_startup();
                // The welcome screen is up when no CLI was found; an agent switched on or set up
                // meanwhile shows on a fresh check.
                if imp.no_cli.get() {
                    imp.check_again();
                }
            }
        ));
        dialog.present(Some(obj.upcast_ref::<gtk4::Widget>()));
    }

    /// What the app is, and what setting up involves.
    fn setup_welcome_page(&self, nav: &adw::NavigationView) -> adw::NavigationPage {
        let status = adw::StatusPage::builder()
            .title("Welcome to Agent Terminal")
            .description(
                "Chat with Claude, Antigravity and Codex in threads, with a terminal a keystroke away.\n\n\
                 Setup takes a minute: choose the agents you use and check you are signed in. \
                 Everything here can be changed later in Settings.\n\n\
                 Setup writes Agent Terminal's own settings file and nothing in any agent's \
                 configuration.",
            )
            .vexpand(true)
            .build();
        crate::icons::set_status_icon(&status, crate::icons::APP_ART);

        let start = Button::builder()
            .label("Get Started")
            .halign(Align::Center)
            .css_classes(["suggested-action", "pill"])
            .build();
        start.connect_clicked(glib::clone!(
            #[weak]
            nav,
            move |_| nav.push_by_tag(AGENTS_TAG)
        ));

        let toolbar = adw::ToolbarView::new();
        toolbar.add_top_bar(&adw::HeaderBar::new());
        toolbar.set_content(Some(&status));
        // In a bar of its own, like "Done" on the next page: always on screen and clear of the
        // dialog's edge, not scrolled out of a short dialog with the page.
        start.set_margin_top(12);
        start.set_margin_bottom(12);
        toolbar.add_bottom_bar(&start);
        adw::NavigationPage::builder()
            .title("Welcome")
            .child(&toolbar)
            .build()
    }

    /// Each agent: whether it is installed and used, and who is signed in; then the default.
    pub(super) fn setup_agents_page(&self, dialog: &adw::Dialog) -> adw::NavigationPage {
        let page = adw::PreferencesPage::builder()
            .description(
                "Switch off any agent you do not use, and sign in to the ones you do. An agent \
                 that is not installed is shown as not installed, with a link to get it; install \
                 it, then choose Check Again.",
            )
            .build();
        page.add(&self.agents_ready_group(dialog));
        for driver in Driver::ALL {
            let group = adw::PreferencesGroup::builder()
                .title(driver.info().long_label)
                .build();
            group.add(&self.agent_enabled_row(driver, dialog));
            group.add(&self.agent_account_row(driver, dialog));
            if driver == Driver::Agy {
                // Read-only: setup shows the hook and the entry to copy, and never writes it.
                self.add_hook_rows(&group);
            }
            page.add(&group);
        }
        let threads = adw::PreferencesGroup::builder()
            .title("New Threads")
            .description("Commands, models and modes are in Settings → Agents.")
            .build();
        threads.add(&self.default_agent_row());
        page.add(&threads);

        let approvals = adw::PreferencesGroup::builder()
            .title("In-App Approvals and Pattern Matching")
            .description(APPROVAL_GROUP_HELP)
            .build();
        approvals.add(&self.pattern_approval_row());
        approvals.add(&self.confirm_rule_row());
        page.add(&approvals);

        let updates = adw::PreferencesGroup::builder()
            .title("Updates")
            .description("The only request Agent Terminal makes by itself. On a first start it waits until setup closes.")
            .build();
        updates.add(&self.update_check_row());
        page.add(&updates);

        let done = Button::builder()
            .label("Done")
            .halign(Align::Center)
            .margin_top(12)
            .margin_bottom(12)
            .css_classes(["suggested-action", "pill"])
            .build();
        done.connect_clicked(glib::clone!(
            #[weak]
            dialog,
            move |_| {
                dialog.close();
            }
        ));

        let toolbar = adw::ToolbarView::new();
        toolbar.add_top_bar(&adw::HeaderBar::new());
        toolbar.set_content(Some(&page));
        toolbar.add_bottom_bar(&done);
        adw::NavigationPage::builder()
            .title("Your Agents")
            .tag(AGENTS_TAG)
            .child(&toolbar)
            .build()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Closing the walkthrough writes the settings file, which is what ends "first run": a reload
    /// after it is not a first start.
    #[test]
    #[ignore = "native GTK layout; run alone in an isolated display"]
    fn native_closing_setup_ends_the_first_run() {
        gtk4::init().expect("GTK must be available for the native gate");
        adw::init().expect("libadwaita init");
        crate::icons::register();
        crate::load_css();
        let config_home = tempfile::tempdir().unwrap();
        std::env::set_var("XDG_CONFIG_HOME", config_home.path());
        assert!(
            crate::config::TerminalConfig::load().is_first_run(),
            "an empty config home is a first run"
        );
        // `load` writes nothing, so the file is still absent.
        assert!(!crate::config::TerminalConfig::config_path().exists());
        let app = adw::Application::builder()
            .application_id("org.test.SetupCloses")
            .flags(gtk4::gio::ApplicationFlags::NON_UNIQUE)
            .build();
        app.register(None::<&gtk4::gio::Cancellable>)
            .expect("registering a non-unique application");
        let ctx = glib::MainContext::default();
        let window = super::super::super::AgentTerminalWindow::new(&app);
        window.present();
        window.imp().show_setup();
        crate::testutil::settle(&ctx);
        window
            .visible_dialog()
            .expect("the walkthrough is up")
            .force_close();
        crate::testutil::settle(&ctx);
        window.imp().flush_pending_save();
        assert!(
            crate::config::TerminalConfig::config_path().exists(),
            "closing setup wrote the settings file"
        );
        assert!(
            !crate::config::TerminalConfig::load().is_first_run(),
            "a reload after setup is not a first run"
        );
        window.destroy();
        crate::testutil::settle(&ctx);
    }

    /// The walkthrough's "Get Started" lies fully inside the dialog, clear of its edge, in a
    /// narrow window (the dialog becomes a bottom sheet) and a wide one (ui-audit: it sat on
    /// the dialog's bottom edge).
    #[test]
    #[ignore = "native GTK layout; run alone in an isolated display"]
    fn native_welcome_button_is_inside_the_dialog() {
        gtk4::init().expect("GTK must be available for the native gate");
        adw::init().expect("libadwaita init");
        crate::icons::register();
        crate::load_css();
        let config_home = tempfile::tempdir().unwrap();
        std::env::set_var("XDG_CONFIG_HOME", config_home.path());
        let app = adw::Application::builder()
            .application_id("org.test.Welcome")
            .flags(gtk4::gio::ApplicationFlags::NON_UNIQUE)
            .build();
        app.register(None::<&gtk4::gio::Cancellable>)
            .expect("registering a non-unique application");
        let ctx = glib::MainContext::default();
        for (width, height) in [(360, 640), (360, 480), (600, 420), (900, 800)] {
            let window = super::super::super::AgentTerminalWindow::new(&app);
            window.set_default_size(width, height);
            window.present();
            window.imp().show_setup();
            let settle = || crate::testutil::settle(&ctx);
            settle();
            let start = crate::testutil::descendants(window.upcast_ref())
                .into_iter()
                .filter_map(|w| w.downcast::<Button>().ok())
                .find(|b| b.label().as_deref() == Some("Get Started") && b.is_mapped())
                .expect("the Get Started button is on screen");
            let page = start
                .ancestor(adw::NavigationView::static_type())
                .expect("inside the walkthrough");
            let bounds = start.compute_bounds(&page).expect("button bounds");
            assert!(
                bounds.x() >= 0.0 && bounds.x() + bounds.width() <= page.width() as f32,
                "{width}px: button {bounds:?} sideways out of the dialog ({} wide)",
                page.width()
            );
            assert!(
                bounds.y() >= 0.0 && bounds.y() + bounds.height() + 8.0 <= page.height() as f32,
                "{width}px: button {bounds:?} sits on the dialog's bottom edge ({} high)",
                page.height()
            );
            window.destroy();
            settle();
        }
    }
}
