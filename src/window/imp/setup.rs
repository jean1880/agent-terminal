//! The first-start walkthrough: a welcome, then the agents (which are installed, which to use,
//! who is signed in, and which new threads start on).
//!
//! Offered once per process when no settings file existed at launch
//! ([`crate::config::TerminalConfig::is_first_run`]), and from Settings → General → Setup at any
//! time. Its rows are the Agents page's own, so they apply on change like every setting; closing
//! it saves the settings, which is what makes the next start not a first one.

use agent_core::adapter::Driver;

use super::*;

thread_local! {
    /// Set once the walkthrough has been offered, so a second window does not offer it again.
    static SETUP_OFFERED: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
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
                imp.schedule_config_save();
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
                 Everything here can be changed later in Settings.",
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
        status.set_child(Some(&start));

        let toolbar = adw::ToolbarView::new();
        toolbar.add_top_bar(&adw::HeaderBar::new());
        toolbar.set_content(Some(&status));
        adw::NavigationPage::builder()
            .title("Welcome")
            .child(&toolbar)
            .build()
    }

    /// Each agent: whether it is installed and used, and who is signed in; then the default.
    fn setup_agents_page(&self, dialog: &adw::Dialog) -> adw::NavigationPage {
        let page = adw::PreferencesPage::builder()
            .description(
                "Agents found on this computer are switched on. Switch off any you do not use, \
                 and sign in to the ones you do. An agent that is not installed says how to \
                 install it.",
            )
            .build();
        for driver in Driver::ALL {
            let group = adw::PreferencesGroup::builder()
                .title(driver.info().long_label)
                .build();
            group.add(&self.agent_enabled_row(driver, dialog));
            group.add(&self.agent_account_row(driver, dialog));
            page.add(&group);
        }
        let threads = adw::PreferencesGroup::builder()
            .title("New Threads")
            .description(
                "Commands, models, modes and Antigravity's approval hook are in Settings → Agents.",
            )
            .build();
        threads.add(&self.default_agent_row());
        page.add(&threads);

        let approvals = adw::PreferencesGroup::builder()
            .title("In-App Approvals & Pattern Matching")
            .description(
                "Agent Terminal can manage approvals directly within the app, automatically \
                 permitting commands and tools that match saved pattern rules (e.g. 'cargo test*') \
                 without blocking on modal prompts.",
            )
            .build();
        let obj = self.obj();
        let auto_approve_row = adw::SwitchRow::builder()
            .title("Automatic Pattern Approvals")
            .subtitle(
                "Auto-approve matched commands. On by default for smooth operation, but broad \
                 wildcards carry the risk of running commands without review.",
            )
            .active(self.config.borrow().pattern_auto_approval)
            .build();
        auto_approve_row.connect_active_notify(glib::clone!(
            #[weak]
            obj,
            move |row| {
                let imp = obj.imp();
                let active = row.is_active();
                if imp.config.borrow().pattern_auto_approval != active {
                    imp.config.borrow_mut().pattern_auto_approval = active;
                    imp.schedule_config_save();
                }
            }
        ));
        approvals.add(&auto_approve_row);

        let confirm_row = adw::SwitchRow::builder()
            .title("Confirm & Customise Patterns on Save")
            .subtitle(
                "Show a confirmation dialogue to loosen or tighten patterns when choosing 'Always allow'.",
            )
            .active(self.config.borrow().confirm_rule_modification)
            .build();
        confirm_row.connect_active_notify(glib::clone!(
            #[weak]
            obj,
            move |row| {
                let imp = obj.imp();
                let active = row.is_active();
                if imp.config.borrow().confirm_rule_modification != active {
                    imp.config.borrow_mut().confirm_rule_modification = active;
                    imp.schedule_config_save();
                }
            }
        ));
        approvals.add(&confirm_row);
        page.add(&approvals);

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
