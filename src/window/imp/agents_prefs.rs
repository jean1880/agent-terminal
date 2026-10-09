//! Preferences → Agents: each chat agent's command, arguments, env file, defaults and on/off,
//! the default agent for new threads, and agy's approval-hook status.
//!
//! Every row applies on change and persists through the shared config (atomic writes). Rows
//! that can hold an invalid value say so inline and do not persist it. An agent's settings live
//! on the 2.x profile that runs it (see [`crate::config::TerminalConfig::agent_profile`]); one
//! is created on the first edit when the profile list has none.

use std::rc::Rc;

use agent_core::adapter::{Driver, Mode};

use super::*;
use crate::availability::AgentAvailability;
use crate::config::Profile;
use crate::model_catalog::ModelCatalog;

/// The default-mode dropdown, by index both ways.
const MODES: [(Mode, &str); 3] = [
    (Mode::Ask, "Ask before edits"),
    (Mode::AcceptEdits, "Accept edits"),
    (Mode::Plan, "Plan"),
];

/// Effort levels offered when the catalogue lists none for the agent.
const FALLBACK_EFFORTS: [&str; 3] = ["low", "medium", "high"];

/// The Account row's text for `driver`, and whether it is signed out (which shows the button).
fn account_text(driver: Driver, snap: &crate::account_status::Snapshot) -> (String, bool) {
    if snap.signed_out {
        return (
            format!("Not signed in. {}", driver.info().sign_in_hint),
            true,
        );
    }
    match &snap.account {
        Some(a) => match &a.plan {
            Some(plan) => (format!("Signed in as {} ({plan})", a.label), false),
            None => (format!("Signed in as {}", a.label), false),
        },
        None => ("Not checked yet".to_owned(), false),
    }
}

/// What the agent rows hold, as their tooltips (an entry row has no subtitle). Shown again once
/// a value checks out; an invalid one shows why instead.
const COMMAND_HELP: &str =
    "The agent's command name (found on PATH) or the full path to its binary";
const ARGS_HELP: &str =
    "Arguments added to every launch of this agent, quoted as in a shell (e.g. --verbose)";
const ENV_FILE_HELP: &str =
    "A KEY=value file whose variables this agent's threads, probes and usage checks run with \
     (an API key or account, say). Blank: none";

fn validation_row() -> adw::ActionRow {
    adw::ActionRow::builder()
        .title("Not Saved")
        .use_markup(false)
        .subtitle_lines(0)
        .css_classes(["error"])
        .visible(false)
        .build()
}

/// Older GTK expects one accessible for ErrorMessage; current GTK expects a reference list.
/// Ask the runtime for its value type because gtk-rs always constructs the newer list form.
fn set_error_message(widget: &gtk4::Widget, feedback: &adw::ActionRow) {
    use glib::translate::{IntoGlib, ToGlibPtr, Uninitialized};

    let relation = gtk4::AccessibleRelation::ErrorMessage;
    // SAFETY: GTK's init_value requires an empty GValue and initialises it before any use.
    let mut expected = unsafe { glib::Value::uninitialized() };
    relation.init_value(&mut expected);
    if expected.type_().is_a(glib::Object::static_type())
        || expected.type_().is_a(gtk4::Accessible::static_type())
    {
        let value = feedback.to_value();
        let mut relations = [relation.into_glib()];
        // SAFETY: one relation and one initialised object GValue are alive for the synchronous
        // call. Both widgets are GTK objects on the main thread; GTK copies the referenced
        // accessible into its relation state. This preserves the ErrorMessage relation on GTK
        // runtimes whose ABI accepts a single object rather than gtk-rs's list value.
        unsafe {
            gtk4::ffi::gtk_accessible_update_relation_value(
                widget.upcast_ref::<gtk4::Accessible>().to_glib_none().0,
                1,
                relations.as_mut_ptr(),
                value.to_glib_none().0,
            );
        }
    } else {
        widget.update_relation(&[gtk4::accessible::Relation::ErrorMessage(&[feedback
            .upcast_ref::<gtk4::Widget>()
            .upcast_ref()])]);
    }
}

/// Invalid drafts remain in the entry so they can be corrected, while the saved value stays
/// in use. Say that visibly and expose the reason alongside the native invalid state.
fn show_validation(
    entry: &adw::EntryRow,
    feedback: &adw::ActionRow,
    help: &str,
    error: Option<&str>,
) {
    // EntryRow delegates text editing to an inner GtkText. Expose the error on the settings
    // row and on the editable field that actually receives keyboard focus.
    let delegate = entry
        .delegate()
        .and_then(|editable| editable.dynamic_cast::<gtk4::Widget>().ok());
    let targets = [Some(entry.upcast_ref::<gtk4::Widget>()), delegate.as_ref()];
    match error {
        Some(reason) => {
            let message = format!("{reason}. The previous saved value is still in use.");
            entry.add_css_class("error");
            entry.set_tooltip_text(Some(reason));
            feedback.set_subtitle(&message);
            feedback.set_visible(true);
            for widget in targets.into_iter().flatten() {
                widget.update_property(&[gtk4::accessible::Property::Description(&message)]);
                widget.update_state(&[gtk4::accessible::State::Invalid(
                    gtk4::AccessibleInvalidState::True,
                )]);
                set_error_message(widget, feedback);
            }
        }
        None => {
            entry.remove_css_class("error");
            entry.set_tooltip_text(Some(help));
            for widget in targets.into_iter().flatten() {
                widget.update_property(&[gtk4::accessible::Property::Description(help)]);
                widget.update_state(&[gtk4::accessible::State::Invalid(
                    gtk4::AccessibleInvalidState::False,
                )]);
                widget.reset_relation(gtk4::AccessibleRelation::ErrorMessage);
            }
            feedback.set_visible(false);
            feedback.set_subtitle("");
        }
    }
}

/// The "default agent" dropdown, by index both ways: Automatic, then every driver in registry
/// order.
fn default_agent_choices() -> Vec<(Option<Driver>, &'static str)> {
    std::iter::once((None, "Automatic"))
        .chain(
            Driver::ALL
                .into_iter()
                .map(|d| (Some(d), d.info().long_label)),
        )
        .collect()
}

/// Whether a command field is acceptable: non-empty, one line, and not something an exec would
/// read as an option.
fn command_ok(text: &str) -> bool {
    !text.is_empty() && !text.contains('\n') && !text.starts_with('-')
}

/// Splits an arguments field as a shell would. `Err` is the reason to show.
pub(super) fn parse_args(text: &str) -> Result<Vec<String>, String> {
    if text.trim().is_empty() {
        return Ok(Vec::new());
    }
    glib::shell_parse_argv(text)
        .map(|v| {
            v.into_iter()
                .map(|a| a.to_string_lossy().into_owned())
                .collect()
        })
        .map_err(|e| e.message().to_owned())
}

/// Whether an env-file field is usable: blank, or a file that exists.
fn env_file_ok(text: &str) -> bool {
    let text = text.trim();
    text.is_empty() || std::path::Path::new(&crate::utils::expand_tilde(text)).is_file()
}

impl AgentTerminalWindow {
    /// Applies `edit` to `driver`'s profile (created when missing) and schedules the save.
    fn edit_agent_profile(&self, driver: Driver, edit: impl FnOnce(&mut Profile)) {
        {
            let mut config = self.config.borrow_mut();
            let index = match config.agent_profile_index(driver) {
                Some(i) => i,
                None => {
                    config
                        .profiles
                        .push(crate::config::new_agent_profile(driver));
                    config.profiles.len() - 1
                }
            };
            if let Some(profile) = config.profiles.get_mut(index) {
                edit(profile);
            }
        }
        self.schedule_config_save();
    }

    /// After the user picks `mode` in a thread's header: unless it already is `driver`'s default
    /// for new threads, a toast offers to make it so (the same setting as Settings → Agents →
    /// Default Mode, which is easy to miss).
    pub(super) fn offer_default_mode(&self, driver: Driver, mode: Mode) {
        if self
            .agent_profile_now(driver)
            .default_mode
            .unwrap_or_default()
            == mode
        {
            return;
        }
        let Some(overlay) = self.toast_overlay.borrow().clone() else {
            return;
        };
        let name = MODES
            .iter()
            .find(|(m, _)| *m == mode)
            .map_or("This mode", |(_, n)| *n);
        let toast = adw::Toast::builder()
            .title(format!("{name} for this thread"))
            .use_markup(false)
            .button_label(format!("Make Default for {}", driver.info().long_label))
            .timeout(8)
            .build();
        let obj = self.obj();
        toast.connect_button_clicked(glib::clone!(
            #[weak]
            obj,
            move |_| {
                let imp = obj.imp();
                // Ask is the built-in default, stored as none (as the Settings row does).
                imp.edit_agent_profile(driver, |p| {
                    p.default_mode = Some(mode).filter(|m| *m != Mode::Ask);
                });
                imp.show_toast(&format!(
                    "New {} threads start in {name}",
                    driver.info().long_label
                ));
            }
        ));
        overlay.add_toast(toast);
    }

    fn agent_profile_now(&self, driver: Driver) -> Profile {
        self.config
            .borrow()
            .agent_profile(driver)
            .cloned()
            .unwrap_or_else(|| crate::config::new_agent_profile(driver))
    }

    pub(super) fn add_agents_page(&self, dialog: &adw::PreferencesDialog) {
        let page = adw::PreferencesPage::builder()
            .title("Agents")
            .icon_name("at-system-users-symbolic")
            .build();

        let general = adw::PreferencesGroup::builder()
            .title("Chat Threads")
            .description(
                "New threads start on this agent; switch agents and models from a thread's header.",
            )
            .build();
        general.add(&self.default_agent_row());
        page.add(&general);

        page.add(&self.permissions_group());

        for driver in Driver::ALL {
            page.add(&self.agent_group(driver, dialog));
        }
        dialog.add(&page);
    }

    /// The "Default Agent for New Threads" dropdown. Applies on change.
    pub(super) fn default_agent_row(&self) -> adw::ComboRow {
        let obj = self.obj();
        let choices = default_agent_choices();
        let names: Vec<&str> = choices.iter().map(|(_, n)| *n).collect();
        let current = self.config.borrow().default_agent;
        let default_row = adw::ComboRow::builder()
            .title("Default Agent for New Threads")
            .subtitle("Automatic: the default profile's agent, else the first installed")
            .model(&gtk4::StringList::new(&names))
            .selected(choices.iter().position(|(d, _)| *d == current).unwrap_or(0) as u32)
            .build();
        default_row.connect_selected_notify(glib::clone!(
            #[weak]
            obj,
            move |row| {
                let choice = choices.get(row.selected() as usize).and_then(|(d, _)| *d);
                let imp = obj.imp();
                if imp.config.borrow().default_agent != choice {
                    imp.config.borrow_mut().default_agent = choice;
                    imp.schedule_config_save();
                }
            }
        ));
        default_row
    }

    /// The "In-App Approvals & Pattern Matching" preferences group.
    pub(super) fn permissions_group(&self) -> adw::PreferencesGroup {
        let group = adw::PreferencesGroup::builder()
            .title("In-App Approvals & Pattern Matching")
            .description(
                "Agent Terminal manages approvals directly within the app, automatically permitting \
                 commands and tools matching wildcard rules (e.g. 'cargo test*') without interrupting your work.",
            )
            .build();

        let obj = self.obj();
        let auto_approve_row = adw::SwitchRow::builder()
            .title("In-App Pattern Auto-Approval")
            .subtitle("Automatically approve commands and tools matching saved pattern rules")
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
        group.add(&auto_approve_row);

        let confirm_row = adw::SwitchRow::builder()
            .title("Confirm & Customise Patterns on Save")
            .subtitle(
                "Prompt with an interactive dialogue to loosen or tighten patterns when choosing 'Always allow'",
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
        group.add(&confirm_row);

        // Import permissions row
        let import_row = adw::ActionRow::builder()
            .title("Import Agent Permissions")
            .subtitle("Import allowlisted commands and tools from Claude, Antigravity, and permissions.toml")
            .build();
        let import_btn = gtk4::Button::builder()
            .label("Import Now")
            .valign(gtk4::Align::Center)
            .css_classes(["pill"])
            .build();
        import_btn.connect_clicked(glib::clone!(
            #[weak]
            obj,
            move |_| {
                let imp = obj.imp();
                if let Some(path) = crate::always_allow::path() {
                    let mut rules = crate::always_allow::AlwaysRules::load(&path);
                    let added_count = rules.import_agent_permissions(None);
                    if added_count > 0 {
                        let _ = rules.save(&path);
                        imp.show_toast(&format!(
                            "Imported {added_count} permission rules from agent configurations"
                        ));
                    } else {
                        imp.show_toast("Permission rules are already up to date");
                    }
                }
            }
        ));
        import_row.add_suffix(&import_btn);
        group.add(&import_row);

        // Manage rules row
        let rule_count = crate::always_allow::path()
            .map(|p| crate::always_allow::AlwaysRules::load(&p).rules.len())
            .unwrap_or(0);
        let manage_row = adw::ActionRow::builder()
            .title("Saved Permission Rules")
            .subtitle(format!(
                "{rule_count} active rules stored in always-allow.json"
            ))
            .build();
        let manage_btn = gtk4::Button::builder()
            .label("Manage Rules…")
            .valign(gtk4::Align::Center)
            .css_classes(["pill"])
            .build();
        manage_btn.connect_clicked(glib::clone!(
            #[weak]
            obj,
            move |btn| {
                let imp = obj.imp();
                imp.show_rules_dialog(btn.upcast_ref());
            }
        ));
        manage_row.add_suffix(&manage_btn);
        group.add(&manage_row);

        group
    }

    fn show_rules_dialog(&self, parent: &gtk4::Widget) {
        let Some(path) = crate::always_allow::path() else {
            return;
        };
        let rules = crate::always_allow::AlwaysRules::load(&path);

        let dialog = adw::AlertDialog::new(
            Some("Permission Rules"),
            Some("Rules that automatically approve matching commands and tools."),
        );

        let scroll = gtk4::ScrolledWindow::builder()
            .hscrollbar_policy(gtk4::PolicyType::Never)
            .vscrollbar_policy(gtk4::PolicyType::Automatic)
            .min_content_height(250)
            .max_content_height(400)
            .min_content_width(420)
            .build();

        let list = gtk4::ListBox::builder()
            .selection_mode(gtk4::SelectionMode::None)
            .css_classes(["boxed-list"])
            .build();

        if rules.rules.is_empty() {
            let empty_row = adw::ActionRow::builder()
                .title("No rules saved yet")
                .subtitle(
                    "Choose 'Always allow' during approval or click 'Import Agent Permissions'",
                )
                .build();
            list.append(&empty_row);
        } else {
            for (i, r) in rules.rules.iter().enumerate() {
                let ws_label = if r.workspace == "*" {
                    "All workspaces"
                } else {
                    &r.workspace
                };
                let row = adw::ActionRow::builder()
                    .title(&r.detail)
                    .subtitle(format!("{} • {} • {:?}", r.tool, ws_label, r.kind))
                    .build();
                let del_btn = gtk4::Button::builder()
                    .icon_name("at-user-trash-symbolic")
                    .valign(gtk4::Align::Center)
                    .css_classes(["flat", "destructive-action"])
                    .tooltip_text("Delete this rule")
                    .build();
                let obj = self.obj();
                let p = path.clone();
                let idx = i;
                del_btn.connect_clicked(glib::clone!(
                    #[weak]
                    obj,
                    #[weak]
                    row,
                    move |_| {
                        let imp = obj.imp();
                        let mut current_rules = crate::always_allow::AlwaysRules::load(&p);
                        if idx < current_rules.rules.len() {
                            current_rules.rules.remove(idx);
                            let _ = current_rules.save(&p);
                            row.set_visible(false);
                            imp.show_toast("Rule removed");
                        }
                    }
                ));
                row.add_suffix(&del_btn);
                list.append(&row);
            }
        }
        scroll.set_child(Some(&list));
        dialog.set_extra_child(Some(&scroll));
        dialog.add_responses(&[("close", "Close")]);
        dialog.set_default_response(Some("close"));
        dialog.set_close_response("close");

        let root = parent.root();
        glib::MainContext::default().spawn_local(async move {
            let _ = dialog.choose_future(root.as_ref()).await;
        });
    }

    fn agent_group(
        &self,
        driver: Driver,
        dialog: &adw::PreferencesDialog,
    ) -> adw::PreferencesGroup {
        let profile = self.agent_profile_now(driver);
        let group = adw::PreferencesGroup::builder()
            .title(driver.info().long_label)
            .description(format!("Profile “{}”", profile.name))
            .build();
        group.add(&self.agent_enabled_row(driver, dialog.upcast_ref()));
        group.add(&self.agent_account_row(driver, dialog.upcast_ref()));
        self.add_agent_detail_rows(&group, driver, &profile, dialog);
        group
    }

    /// `driver`'s on/off switch, saying what detection found (ready and where, missing and how to
    /// install, or off). Follows the scan until `dialog` closes.
    pub(super) fn agent_enabled_row(&self, driver: Driver, dialog: &adw::Dialog) -> adw::SwitchRow {
        let obj = self.obj();
        let profile = self.agent_profile_now(driver);
        let enabled = adw::SwitchRow::builder()
            .title("Enabled")
            .subtitle(AgentAvailability::shared().get(driver).describe(driver))
            .active(!profile.disabled)
            .build();
        let brand = gtk4::Image::from_gicon(&crate::icons::driver_icon(driver));
        brand.set_pixel_size(20);
        enabled.add_prefix(&brand);
        enabled.connect_active_notify(glib::clone!(
            #[weak]
            obj,
            move |row| {
                let on = row.is_active();
                let imp = obj.imp();
                imp.edit_agent_profile(driver, |p| p.disabled = !on);
                // The scan decides what is offered: run it again with the new setting.
                imp.refresh_agent_data();
            }
        ));
        // The row says what detection found: ready (where), missing (how to install), or off.
        let state_id = AgentAvailability::shared().connect_changed(glib::clone!(
            #[weak]
            enabled,
            move || {
                enabled.set_subtitle(&AgentAvailability::shared().get(driver).describe(driver));
            }
        ));
        dialog.connect_closed(move |_| AgentAvailability::shared().disconnect(state_id));
        enabled
    }

    /// Who is signed in to `driver`, with Sign In (then Check Again) when nobody is: an installed
    /// agent nobody signed in to cannot run a turn. Shown only while the agent is in use; follows
    /// detection and the account checks until `dialog` closes.
    pub(super) fn agent_account_row(&self, driver: Driver, dialog: &adw::Dialog) -> adw::ActionRow {
        let obj = self.obj();
        let account = adw::ActionRow::builder()
            .title("Account")
            .use_markup(false)
            .build();
        let sign_in = gtk4::Button::builder()
            .label("Sign In…")
            .valign(gtk4::Align::Center)
            .tooltip_text(driver.info().sign_in_hint)
            .build();
        account.add_suffix(&sign_in);
        let show_account = glib::clone!(
            #[weak]
            obj,
            #[weak]
            account,
            #[weak]
            sign_in,
            move || {
                // Only an agent in use has an account worth showing: one switched off or not
                // installed never says "signed out".
                account.set_visible(AgentAvailability::shared().is_ready(driver));
                let snap = crate::account_status::AccountStatus::shared().snapshot(driver);
                let (text, signed_out) = account_text(driver, &snap);
                account.set_subtitle(&text);
                sign_in.set_visible(signed_out);
                // The same two steps as a thread's banner: open the sign-in, then check.
                sign_in.set_label(if obj.imp().sign_in_was_started(driver) {
                    "Check Again"
                } else {
                    "Sign In…"
                });
                if signed_out {
                    account.add_css_class("error");
                } else {
                    account.remove_css_class("error");
                }
            }
        );
        sign_in.connect_clicked(glib::clone!(
            #[weak]
            obj,
            #[strong]
            show_account,
            move |_| {
                obj.imp().sign_in_or_check(driver);
                show_account();
            }
        ));
        show_account();
        let ready_id = AgentAvailability::shared().connect_changed(show_account.clone());
        let account_id =
            crate::account_status::AccountStatus::shared().connect_changed(show_account);
        dialog.connect_closed(move |_| {
            AgentAvailability::shared().disconnect(ready_id);
            crate::account_status::AccountStatus::shared().disconnect(account_id);
        });
        account
    }

    /// The rest of `driver`'s Settings rows: command, arguments, env file, defaults, and agy's
    /// hook.
    fn add_agent_detail_rows(
        &self,
        group: &adw::PreferencesGroup,
        driver: Driver,
        profile: &Profile,
        dialog: &adw::PreferencesDialog,
    ) {
        let obj = self.obj();
        // Command, with what detection makes of it.
        // Shows a path: plain text, not markup (an `&` or `<` in it would blank the row).
        let status = adw::ActionRow::builder()
            .use_markup(false)
            .title("Detected")
            .subtitle("Checking…")
            .subtitle_selectable(true)
            .build();
        let command = adw::EntryRow::builder()
            .title("Command or Path")
            .tooltip_text(COMMAND_HELP)
            .text(&profile.command)
            .build();
        let command_error = validation_row();
        show_validation(&command, &command_error, COMMAND_HELP, None);
        let generation = Rc::new(std::cell::Cell::new(0u64));
        let detect = Rc::new(glib::clone!(
            #[weak]
            status,
            #[strong]
            generation,
            move |command: String| {
                let current = generation.get().wrapping_add(1);
                generation.set(current);
                status.set_subtitle("Checking…");
                let generation = generation.clone();
                glib::MainContext::default().spawn_local(async move {
                    let found = gtk4::gio::spawn_blocking(move || detect_agent(&command))
                        .await
                        .unwrap_or_else(|_| Err("the check panicked".to_owned()));
                    if generation.get() != current {
                        return;
                    }
                    match found {
                        Ok(text) => {
                            status.remove_css_class("error");
                            status.set_subtitle(&text);
                        }
                        Err(text) => {
                            status.add_css_class("error");
                            status.set_subtitle(&text);
                        }
                    }
                });
            }
        ));
        detect(profile.command.clone());
        command.connect_changed(glib::clone!(
            #[weak]
            obj,
            #[strong]
            detect,
            #[weak]
            command_error,
            move |row| {
                let text = row.text().trim().to_owned();
                if !command_ok(&text) {
                    show_validation(
                        row,
                        &command_error,
                        COMMAND_HELP,
                        Some("Enter a command name or a path (not an option)"),
                    );
                    return;
                }
                show_validation(row, &command_error, COMMAND_HELP, None);
                let imp = obj.imp();
                if imp.agent_profile_now(driver).command == text {
                    return;
                }
                imp.edit_agent_profile(driver, |p| p.command = text.clone());
                detect(text);
                imp.refresh_profile_selection();
                imp.refresh_agent_data();
            }
        ));
        group.add(&command);
        group.add(&command_error);
        group.add(&status);

        let args = adw::EntryRow::builder()
            .title("Extra Arguments")
            .tooltip_text(ARGS_HELP)
            .text(
                profile
                    .args
                    .iter()
                    .map(|a| crate::utils::shell_quote(a))
                    .collect::<Vec<_>>()
                    .join(" "),
            )
            .build();
        let args_error = validation_row();
        show_validation(&args, &args_error, ARGS_HELP, None);
        args.connect_changed(glib::clone!(
            #[weak]
            obj,
            #[weak]
            args_error,
            move |row| match parse_args(&row.text()) {
                Ok(parsed) => {
                    show_validation(row, &args_error, ARGS_HELP, None);
                    let imp = obj.imp();
                    if imp.agent_profile_now(driver).args != parsed {
                        imp.edit_agent_profile(driver, |p| p.args = parsed);
                    }
                }
                Err(reason) => {
                    show_validation(row, &args_error, ARGS_HELP, Some(&reason));
                }
            }
        ));
        group.add(&args);
        group.add(&args_error);

        let env_file = adw::EntryRow::builder()
            .title("Environment File")
            .tooltip_text(ENV_FILE_HELP)
            .text(profile.env_file.as_deref().unwrap_or(""))
            .build();
        let env_error = validation_row();
        show_validation(&env_file, &env_error, ENV_FILE_HELP, None);
        env_file.connect_changed(glib::clone!(
            #[weak]
            obj,
            #[weak]
            env_error,
            move |row| {
                let text = row.text().trim().to_owned();
                if !env_file_ok(&text) {
                    show_validation(
                        row,
                        &env_error,
                        ENV_FILE_HELP,
                        Some("That file does not exist"),
                    );
                    return;
                }
                show_validation(row, &env_error, ENV_FILE_HELP, None);
                let value = (!text.is_empty()).then_some(text);
                let imp = obj.imp();
                if imp.agent_profile_now(driver).env_file != value {
                    imp.edit_agent_profile(driver, |p| p.env_file = value);
                    imp.refresh_agent_data();
                }
            }
        ));
        group.add(&env_file);
        group.add(&env_error);

        // Default model, from the live catalogue; the saved one stays listed even when the
        // catalogue does not have it (yet).
        let model_row = adw::ComboRow::builder()
            .title("Default Model")
            .subtitle("The model this agent's new threads start on")
            .use_subtitle(false)
            .build();
        let model_ids: Rc<RefCell<Vec<Option<String>>>> = Rc::default();
        let filling = Rc::new(std::cell::Cell::new(false));
        let fill_models = Rc::new(glib::clone!(
            #[weak]
            obj,
            #[weak]
            model_row,
            #[strong]
            model_ids,
            #[strong]
            filling,
            move || {
                let saved = obj.imp().agent_profile_now(driver).default_model;
                let mut ids: Vec<Option<String>> = vec![None];
                let mut labels: Vec<String> = vec!["The CLI's default".to_owned()];
                for m in ModelCatalog::shared()
                    .models()
                    .into_iter()
                    .filter(|m| m.driver == driver)
                {
                    labels.push(match &m.via {
                        Some(via) => format!("{} (via {via})", m.display),
                        None => m.display.clone(),
                    });
                    ids.push(Some(m.id));
                }
                if let Some(saved) = saved
                    .as_ref()
                    .filter(|s| !ids.contains(&Some((*s).clone())))
                {
                    // Not in the list: "retired" once the list is a fresh one, plain while it is
                    // only the cache or still loading. New threads never start on a retired id.
                    let catalog = ModelCatalog::shared();
                    let gone = catalog.is_fresh(driver)
                        && !catalog.models_of(driver).iter().any(|m| m.is_model(saved));
                    labels.push(if gone {
                        format!("{saved} (retired)")
                    } else {
                        saved.clone()
                    });
                    ids.push(Some(saved.clone()));
                }
                let refs: Vec<&str> = labels.iter().map(String::as_str).collect();
                let selected = ids.iter().position(|i| *i == saved).unwrap_or(0) as u32;
                filling.set(true);
                model_row.set_model(Some(&gtk4::StringList::new(&refs)));
                model_row.set_selected(selected);
                filling.set(false);
                *model_ids.borrow_mut() = ids;
            }
        ));
        fill_models();
        {
            // Disconnected when the dialog closes: the catalogue is app-wide.
            let fill = fill_models.clone();
            let id = ModelCatalog::shared().connect_changed(move || fill());
            dialog.connect_closed(move |_| ModelCatalog::shared().disconnect(id));
        }
        model_row.connect_selected_notify(glib::clone!(
            #[weak]
            obj,
            #[strong]
            model_ids,
            #[strong]
            filling,
            move |row| {
                if filling.get() {
                    return;
                }
                let choice = model_ids
                    .borrow()
                    .get(row.selected() as usize)
                    .cloned()
                    .flatten();
                obj.imp()
                    .edit_agent_profile(driver, |p| p.default_model = choice);
            }
        ));
        group.add(&model_row);

        let mode_names: Vec<&str> = MODES.iter().map(|(_, n)| *n).collect();
        let mode_row = adw::ComboRow::builder()
            .title("Default Mode")
            .model(&gtk4::StringList::new(&mode_names))
            .selected(
                MODES
                    .iter()
                    .position(|(m, _)| *m == profile.default_mode.unwrap_or_default())
                    .unwrap_or(0) as u32,
            )
            .build();
        mode_row.set_subtitle(if driver == Driver::Agy {
            "New threads start here; a thread keeps its own. Without the approval hook, \
             Ask before edits runs as Plan"
        } else {
            "New threads start here; a thread keeps its own"
        });
        mode_row.connect_selected_notify(glib::clone!(
            #[weak]
            obj,
            move |row| {
                let mode = MODES.get(row.selected() as usize).map(|(m, _)| *m);
                obj.imp().edit_agent_profile(driver, |p| {
                    p.default_mode = mode.filter(|m| *m != Mode::Ask);
                });
            }
        ));
        group.add(&mode_row);

        let efforts = catalogue_efforts(driver);
        let mut effort_names = vec!["The CLI's default".to_owned()];
        effort_names.extend(efforts.iter().cloned());
        if let Some(saved) = profile
            .default_effort
            .as_ref()
            .filter(|e| !efforts.contains(e))
        {
            effort_names.push(saved.clone());
        }
        let effort_refs: Vec<&str> = effort_names.iter().map(String::as_str).collect();
        let effort_row = adw::ComboRow::builder()
            .title("Default Effort")
            .subtitle("Where the model supports it")
            .model(&gtk4::StringList::new(&effort_refs))
            .selected(
                profile
                    .default_effort
                    .as_ref()
                    .and_then(|e| effort_names.iter().position(|n| n == e))
                    .unwrap_or(0) as u32,
            )
            .build();
        effort_row.connect_selected_notify(glib::clone!(
            #[weak]
            obj,
            move |row| {
                let choice = (row.selected() > 0)
                    .then(|| effort_names.get(row.selected() as usize).cloned())
                    .flatten();
                obj.imp()
                    .edit_agent_profile(driver, |p| p.default_effort = choice);
            }
        ));
        group.add(&effort_row);

        if driver == Driver::Agy {
            self.add_hook_rows(group);
        }
    }

    /// agy's approval hook: whether hooks.json installs it, the entry to add, and a Copy button.
    fn add_hook_rows(&self, group: &adw::PreferencesGroup) {
        let entry = crate::hook_config::install_entry_json();
        let hook = adw::ActionRow::builder()
            .title("Approval Hook")
            .subtitle("Checking ~/.gemini/config/hooks.json…")
            .build();
        let copy = Button::builder()
            .icon_name("at-edit-copy-symbolic")
            .tooltip_text("Copy the hooks.json entry")
            .valign(Align::Center)
            .css_classes(["flat"])
            .build();
        let to_copy = entry.clone();
        copy.connect_clicked(move |b| {
            b.clipboard().set_text(&to_copy);
            if let Some(obj) = window_of(b) {
                obj.imp().show_toast("Copied the hooks.json entry");
            }
        });
        hook.add_suffix(&copy);
        group.add(&hook);

        let json = Label::builder()
            .label(&entry)
            .xalign(0.0)
            .wrap(true)
            .wrap_mode(gtk4::pango::WrapMode::WordChar)
            .selectable(true)
            .css_classes(["monospace", "caption", "hook-entry"])
            .margin_top(8)
            .margin_bottom(8)
            .margin_start(12)
            .margin_end(12)
            .build();
        let expander = adw::ExpanderRow::builder()
            .title("Hook Entry")
            .subtitle(
                "Add it as a top-level key of ~/.gemini/config/hooks.json. Until it is \
                 installed, agy cannot ask before acting: it refuses shell commands but \
                 applies file edits on its own, in Plan mode too.",
            )
            .build();
        expander.add_row(&json);
        group.add(&expander);
        group.add(&always_allowed_rows());

        glib::MainContext::default().spawn_local(glib::clone!(
            #[weak]
            hook,
            async move {
                // Off the main thread, and it refreshes the verdict new agy sessions use.
                match super::threads::check_hook().await {
                    Ok(()) => {
                        hook.set_subtitle("Installed: agy asks agent-terminal before it acts");
                        hook.remove_css_class("error");
                    }
                    Err(_) => {
                        hook.set_subtitle(
                            "Not installed: agy cannot ask before acting until the entry below is added",
                        );
                        hook.add_css_class("error");
                    }
                }
            }
        ));
    }
}

/// agy's remembered "Always allow" rules, each with a Remove button. A removal is saved at once
/// and applies to agy sessions started after it (a running one keeps what it already allowed).
fn always_allowed_rows() -> adw::ExpanderRow {
    use crate::always_allow::{self, AlwaysRules};
    let rules = always_allow::path()
        .map(|p| AlwaysRules::load(&p))
        .unwrap_or_default();
    let expander = adw::ExpanderRow::builder()
        .title("Always-Allowed Actions")
        .subtitle(match rules.rules.len() {
            0 => {
                "None yet. “Always allow” on an approval adds one, for that folder only".to_owned()
            }
            n => format!("{n} remembered. Removing one applies to new agy sessions"),
        })
        .build();
    for rule in rules.rules {
        let row = adw::ActionRow::builder()
            .use_markup(false)
            .title(rule.summary())
            .subtitle(&rule.workspace)
            .title_lines(2)
            .build();
        let remove = Button::builder()
            .icon_name("at-window-close-symbolic")
            .tooltip_text("Forget this rule")
            .valign(Align::Center)
            .css_classes(["flat"])
            .build();
        remove.connect_clicked(glib::clone!(
            #[weak]
            row,
            #[weak]
            expander,
            move |_| {
                let Some(path) = always_allow::path() else {
                    return;
                };
                let mut rules = AlwaysRules::load(&path);
                rules.rules.retain(|r| *r != rule);
                match rules.save(&path) {
                    Ok(()) => expander.remove(&row),
                    Err(e) => warn!(error = %e, "could not save the always-allow rules"),
                }
            }
        ));
        row.add_suffix(&remove);
        expander.add_row(&row);
    }
    expander
}

/// The effort levels the catalogue lists for `driver`'s models, in first-seen order.
fn catalogue_efforts(driver: Driver) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for m in ModelCatalog::shared()
        .models()
        .into_iter()
        .filter(|m| m.driver == driver)
    {
        for e in m.efforts {
            if !out.contains(&e) {
                out.push(e);
            }
        }
    }
    if out.is_empty() {
        out = FALLBACK_EFFORTS.iter().map(|e| (*e).to_owned()).collect();
    }
    out
}

/// "Installed at <path> · <version>", or why not. Blocking: locate + `--version`.
fn detect_agent(command: &str) -> Result<String, String> {
    let (path, home, shell) = env_triplet();
    let probe = crate::utils::SystemProbe::new(path, home, shell);
    let Some(found) = probe.locate(command) else {
        return Err(format!("Not found: {command}"));
    };
    let mut cmd = std::process::Command::new(&found);
    cmd.arg("--version");
    let version = crate::utils::run_command(cmd, &found, 5)
        .ok()
        .filter(|o| o.status.success())
        .map(|o| {
            String::from_utf8_lossy(&o.stdout)
                .lines()
                .next()
                .unwrap_or_default()
                .trim()
                .to_owned()
        })
        .filter(|v| !v.is_empty());
    Ok(match version {
        Some(v) => format!("Installed at {found} · {v}"),
        None => format!("Installed at {found}"),
    })
}

#[cfg(test)]
pub(super) mod tests {
    use super::*;

    /// Called by the window smoke test on GTK's thread. Invalid text must give a lasting
    /// correction reason and must not change the profile used to launch the agent.
    pub(crate) fn invalid_agent_drafts_are_not_saved(imp: &AgentTerminalWindow) {
        let saved = imp.agent_profile_now(Driver::Codex);
        let group = adw::PreferencesGroup::new();
        let dialog = adw::PreferencesDialog::new();
        imp.add_agent_detail_rows(&group, Driver::Codex, &saved, &dialog);
        let mut stack = vec![group.upcast::<gtk4::Widget>()];
        let mut fields = Vec::new();
        let mut feedback = Vec::new();
        while let Some(widget) = stack.pop() {
            if let Some(row) = widget.downcast_ref::<adw::EntryRow>() {
                fields.push(row.clone());
            }
            if let Some(row) = widget.downcast_ref::<adw::ActionRow>() {
                if row.title() == "Not Saved" {
                    feedback.push(row.clone());
                }
            }
            let mut child = widget.first_child();
            while let Some(widget) = child {
                child = widget.next_sibling();
                stack.push(widget);
            }
        }
        for (title, bad) in [
            ("Command or Path", "--invalid-command"),
            ("Extra Arguments", "'unfinished"),
            (
                "Environment File",
                "/definitely/not/here/agent-terminal-validation.env",
            ),
        ] {
            let field = fields
                .iter()
                .find(|row| row.title() == title)
                .expect("entry row");
            let original = field.text();
            field.set_text(bad);
            assert!(field.has_css_class("error"));
            assert!(gtk4::test_accessible_has_relation(
                field.upcast_ref::<gtk4::Widget>(),
                gtk4::AccessibleRelation::ErrorMessage
            ));
            let editable = field
                .delegate()
                .and_then(|delegate| delegate.dynamic_cast::<gtk4::Widget>().ok())
                .expect("editable field");
            assert!(gtk4::test_accessible_has_relation(
                &editable,
                gtk4::AccessibleRelation::ErrorMessage
            ));
            assert!(feedback.iter().any(|row| {
                row.is_visible()
                    && row
                        .subtitle()
                        .is_some_and(|s| s.contains("previous saved value"))
            }));
            assert_eq!(
                imp.agent_profile_now(Driver::Codex),
                saved,
                "invalid {title} was persisted"
            );
            field.set_text(&original);
            assert!(!field.has_css_class("error"));
            assert!(!gtk4::test_accessible_has_relation(
                field.upcast_ref::<gtk4::Widget>(),
                gtk4::AccessibleRelation::ErrorMessage
            ));
            assert!(!gtk4::test_accessible_has_relation(
                &editable,
                gtk4::AccessibleRelation::ErrorMessage
            ));
            assert!(feedback.iter().all(|row| !row.is_visible()));
        }
    }

    #[test]
    fn the_default_mode_dropdown_lists_every_mode_once() {
        // Exhaustive: a new mode does not compile until it is placed in `MODES`.
        let slot = |m: Mode| match m {
            Mode::Ask | Mode::AcceptEdits | Mode::Plan => MODES.iter().position(|(x, _)| *x == m),
        };
        for m in [Mode::Ask, Mode::AcceptEdits, Mode::Plan] {
            let i = slot(m).expect("listed");
            assert_eq!(MODES[i].0, m);
        }
        let mut seen: Vec<Mode> = MODES.iter().map(|(m, _)| *m).collect();
        seen.dedup();
        assert_eq!(seen.len(), MODES.len(), "no mode twice");
    }

    #[test]
    fn arguments_split_like_a_shell_and_refuse_bad_quoting() {
        assert_eq!(parse_args("").unwrap(), Vec::<String>::new());
        assert_eq!(
            parse_args("--model 'opus 4' -v").unwrap(),
            ["--model", "opus 4", "-v"]
        );
        assert!(parse_args("--x 'unclosed").is_err());
        // The field shows quoted arguments; reading that back gives the same list.
        let args = ["a b".to_owned(), "c".to_owned()];
        let shown: Vec<String> = args.iter().map(|a| crate::utils::shell_quote(a)).collect();
        assert_eq!(parse_args(&shown.join(" ")).unwrap(), args);
    }

    #[test]
    fn dropdown_arrays_are_complete_and_indexed_both_ways() {
        for (i, (mode, _)) in MODES.iter().enumerate() {
            match mode {
                Mode::Ask | Mode::AcceptEdits | Mode::Plan => {}
            }
            assert_eq!(MODES.iter().position(|(m, _)| m == mode), Some(i));
        }
        // The default-agent dropdown is Automatic plus every registered driver, in order.
        let choices = default_agent_choices();
        assert_eq!(choices.len(), Driver::ALL.len() + 1);
        assert_eq!(choices[0].0, None);
        for (i, d) in Driver::ALL.into_iter().enumerate() {
            assert_eq!(choices[i + 1], (Some(d), d.info().long_label));
        }
        assert!(command_ok("codex") && command_ok("/opt/bin/claude"));
        assert!(!command_ok("") && !command_ok("--help") && !command_ok("a\nb"));
        assert!(env_file_ok(""));
        assert!(!env_file_ok("/definitely/not/here.env"));
    }
}
