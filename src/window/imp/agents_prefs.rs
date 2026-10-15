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

mod delegation;

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

const PERMISSION_SOURCES_HELP: &str =
    "Extra permissions.toml files to import from and export to, separated by ':' (absolute or \
     ~/ paths to existing files). Blank: none";

const AGENT_EDIT_DELAY: std::time::Duration = std::time::Duration::from_millis(400);

#[derive(Default)]
pub(super) struct EditDebounce {
    source: RefCell<Option<glib::SourceId>>,
    pending: RefCell<Option<std::boxed::Box<dyn FnOnce()>>>,
}

impl EditDebounce {
    fn cancel(&self) {
        if let Some(id) = self.source.borrow_mut().take() {
            id.remove();
        }
        self.pending.borrow_mut().take();
    }

    fn flush(&self) {
        if let Some(id) = self.source.borrow_mut().take() {
            id.remove();
        }
        let pending = self.pending.borrow_mut().take();
        if let Some(pending) = pending {
            pending();
        }
    }

    fn schedule(self: &Rc<Self>, then: impl FnOnce() + 'static) {
        self.cancel();
        *self.pending.borrow_mut() = Some(std::boxed::Box::new(then));
        let weak = Rc::downgrade(self);
        *self.source.borrow_mut() =
            Some(glib::timeout_add_local_once(AGENT_EDIT_DELAY, move || {
                if let Some(timer) = weak.upgrade() {
                    timer.source.borrow_mut().take();
                    timer.flush();
                }
            }));
    }
}

impl Drop for EditDebounce {
    fn drop(&mut self) {
        self.flush();
    }
}

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

/// Where Codex's rules are, in the environment a Codex thread gets ([`crate::codex_home`]: the
/// app's environment minus `clear_env`, the shell `PATH`, then the Codex profile's env file),
/// with the binary that checks the rules. Off the main thread (the env file is sourced by a
/// shell). `Err`: why it cannot be known.
async fn codex_target(
    (profile, clear): (Profile, Vec<String>),
) -> Result<crate::always_allow::CodexTarget, String> {
    let resolved = super::threads::resolve_agent(profile).await;
    let env = crate::codex_home::child_env(&crate::codex_home::Inputs {
        inherited: crate::codex_home::process_env(),
        shell_path: crate::utils::known_shell_path().map(str::to_owned),
        env_file: resolved.env,
        clear,
    });
    let home = crate::codex_home::resolve(&env)?;
    Ok(crate::always_allow::CodexTarget {
        home,
        program: resolved.program.map(std::path::PathBuf::from),
        env,
    })
}

/// What the rules list shows, read off the main thread.
struct RulesView {
    rules: crate::always_allow::AlwaysRules,
    /// Why the rules file cannot be used, if it cannot.
    blocked: Option<String>,
    status: crate::always_allow::PermissionSyncStatus,
    /// The export confirmation's text.
    warning: String,
}

/// What the agent files are called in Settings, Codex included only when it is a source and
/// target.
fn source_names(codex: bool) -> &'static str {
    if codex {
        "Claude, Antigravity, Codex and permissions.toml"
    } else {
        "Claude, Antigravity and permissions.toml"
    }
}

impl AgentTerminalWindow {
    pub(super) fn flush_agent_edits(&self) {
        let timers: Vec<_> = self
            .agent_edits
            .borrow()
            .iter()
            .filter_map(std::rc::Weak::upgrade)
            .collect();
        for timer in timers {
            timer.flush();
        }
        self.agent_edits
            .borrow_mut()
            .retain(|timer| timer.strong_count() > 0);
    }
    /// The permission files the user named in Settings, `~/` expanded.
    fn permission_sources(&self) -> Vec<std::path::PathBuf> {
        self.config.borrow().permission_source_paths()
    }

    /// Imports the agent configurations into the rules file, off the main thread (it takes the
    /// store lock and reads files); an unusable rules file is named and left untouched. `done`
    /// runs on the main thread once the job has finished (and its toast is shown), whatever
    /// the outcome.
    fn import_rules(&self, path: &std::path::Path, done: impl FnOnce() + 'static) {
        let path = path.to_owned();
        let sources = self.permission_sources();
        let obj = self.obj().downgrade();
        let target = path.clone();
        let codex_inputs = self.codex_inputs();
        glib::spawn_future_local(async move {
            let codex = codex_target(codex_inputs).await.ok();
            Self::store_job(
                move || {
                    crate::always_allow::AlwaysRules::import_into(&target, &sources, codex.as_ref())
                },
                move |result| {
                    if let Some(obj) = obj.upgrade() {
                        let imp = obj.imp();
                        match result {
                            Some(Ok(imported)) => imp.show_toast(&imported.summary()),
                            Some(Err(e)) => {
                                imp.show_toast(&crate::always_allow::blocked_notice(&path, &e))
                            }
                            None => imp.show_toast("The import did not finish"),
                        }
                    }
                    done();
                },
            );
        });
    }

    /// Exports the global rules off the main thread (the caller has shown
    /// [`crate::always_allow::export_warning`]), Codex's file included when Codex's folder
    /// resolves. `done` runs on the main thread once the job has finished.
    fn export_rules(&self, path: &std::path::Path, done: impl FnOnce() + 'static) {
        let path = path.to_owned();
        let sources = self.permission_sources();
        let obj = self.obj().downgrade();
        let target = path.clone();
        let codex_inputs = self.codex_inputs();
        glib::spawn_future_local(async move {
            let codex = codex_target(codex_inputs).await;
            let unresolved = codex.as_ref().err().cloned();
            let codex = codex.ok();
            Self::store_job(
                move || {
                    crate::always_allow::AlwaysRules::export_from(&target, &sources, codex.as_ref())
                },
                move |result| {
                    if let Some(obj) = obj.upgrade() {
                        let imp = obj.imp();
                        match result {
                            Some(Ok(exported)) => {
                                let mut text = exported.summary();
                                if let Some(reason) = unresolved {
                                    text.push_str(&format!(
                                        ". Codex rules were not exported: {reason}"
                                    ));
                                }
                                imp.show_toast(&text)
                            }
                            Some(Err(e)) => {
                                imp.show_toast(&crate::always_allow::blocked_notice(&path, &e))
                            }
                            None => imp.show_toast("The export did not finish"),
                        }
                    }
                    done();
                },
            );
        });
    }

    /// What resolving Codex's folder needs from the config: the Codex profile (its command and
    /// env file) and `clear_env`.
    fn codex_inputs(&self) -> (Profile, Vec<String>) {
        let config = self.config.borrow();
        (
            config
                .agent_profile(Driver::Codex)
                .cloned()
                .unwrap_or_else(|| crate::config::new_agent_profile(Driver::Codex)),
            config.clear_env.clone(),
        )
    }

    /// Runs a rules-store `job` on a worker thread (it may wait for the store lock) and
    /// `finish` on the main thread with its result (`None`: the worker failed). Nothing that
    /// depends on the job's outcome (a toast, the sync indicator) runs before `finish`.
    fn store_job<T: Send + 'static>(
        job: impl FnOnce() -> T + Send + 'static,
        finish: impl FnOnce(Option<T>) + 'static,
    ) {
        glib::spawn_future_local(async move {
            finish(gtk4::gio::spawn_blocking(job).await.ok());
        });
    }

    /// Removes `rule` from the rules file off the main thread (it takes the store lock), then
    /// calls `done` on the main thread with the outcome.
    fn forget_rule(
        path: std::path::PathBuf,
        rule: crate::always_allow::Rule,
        done: impl FnOnce(Result<bool, crate::always_allow::StoreError>) + 'static,
    ) {
        glib::spawn_future_local(async move {
            let result = gtk4::gio::spawn_blocking(move || {
                crate::always_allow::AlwaysRules::forget(&path, &rule)
            })
            .await
            .unwrap_or_else(|_| {
                Err(crate::always_allow::StoreError::Write(
                    "the removal did not finish".to_owned(),
                ))
            });
            done(result);
        });
    }

    /// Applies `edit` to `driver`'s profile (created when missing) and schedules the save.
    fn edit_agent_profile(&self, driver: Driver, edit: impl FnOnce(&mut Profile)) {
        let profile_id = {
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
            config.profiles.get_mut(index).map(|profile| {
                edit(profile);
                profile.id.clone()
            })
        };
        self.schedule_config_save();
        if let Some((supervisor, profile_id)) = crate::delegation::supervisor().zip(profile_id) {
            supervisor.config_changed(&profile_id);
        }
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
        self.add_toast(toast);
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
        page.add(&self.delegation_group(dialog));

        page.add(&self.permissions_group());

        for driver in Driver::ALL {
            page.add(&self.agent_group(driver, dialog));
        }
        dialog.add(&page);
    }

    /// A dropdown row's selected value is never cut short: the value label stops ellipsizing,
    /// so a long subtitle wraps instead of squeezing it ("Agent T…" for a theme name). The
    /// label is made when the row is first shown, so this runs again then and on every change.
    pub(super) fn keep_value_whole(row: &adw::ComboRow) {
        Self::whole_value_now(row);
        row.connect_map(Self::whole_value_now);
        row.connect_selected_notify(Self::whole_value_now);
    }

    fn whole_value_now(row: &adw::ComboRow) {
        let selected = row
            .selected_item()
            .and_downcast::<gtk4::StringObject>()
            .map(|s| s.string());
        let mut stack = vec![row.clone().upcast::<gtk4::Widget>()];
        while let Some(widget) = stack.pop() {
            if let Some(label) = widget.downcast_ref::<gtk4::Label>() {
                if selected.as_deref() == Some(label.text().as_str()) {
                    label.set_ellipsize(gtk4::pango::EllipsizeMode::None);
                }
            }
            let mut child = widget.first_child();
            while let Some(c) = child {
                child = c.next_sibling();
                stack.push(c);
            }
        }
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
        Self::keep_value_whole(&default_row);
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

    /// The "In-App Approvals and Pattern Matching" preferences group.
    pub(super) fn permissions_group(&self) -> adw::PreferencesGroup {
        let group = adw::PreferencesGroup::builder()
            .title("In-App Approvals and Pattern Matching")
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
            .title("Confirm and Customise Patterns on Save")
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

        // Extra permission files to import from and export to; none unless the user names them.
        let sources_row = adw::EntryRow::builder()
            .title("Permission Sources")
            .tooltip_text(PERMISSION_SOURCES_HELP)
            .text(self.config.borrow().permission_sources.join(":"))
            .build();
        let sources_error = validation_row();
        show_validation(&sources_row, &sources_error, PERMISSION_SOURCES_HELP, None);
        group.add(&sources_row);
        group.add(&sources_error);

        // Warning row: shown prominently when rules differ between Agent Terminal and agent configs
        let warning_row = adw::ActionRow::builder()
            .title("Permissions Out of Sync")
            .use_markup(false)
            .subtitle("Saved rules differ from agent configurations. Use Import or Export below to synchronise.")
            .css_classes(["warning"])
            .visible(false)
            .build();
        let warning_icon = gtk4::Image::from_icon_name("at-dialog-warning-symbolic");
        warning_icon.set_valign(gtk4::Align::Center);
        warning_icon.add_css_class("warning-indicator");
        warning_row.add_prefix(&warning_icon);
        let view_diff_btn = gtk4::Button::builder()
            .label("View Differences…")
            .valign(gtk4::Align::Center)
            .css_classes(["pill"])
            .build();
        warning_row.add_suffix(&view_diff_btn);
        group.add(&warning_row);

        // Import permissions row
        let import_row = adw::ActionRow::builder()
            .title("Import Agent Permissions")
            .subtitle("One-way import: pull allowlisted commands and tools from Claude, Antigravity, and permissions.toml")
            .build();
        let import_btn = gtk4::Button::builder()
            .label("Import Now")
            .valign(gtk4::Align::Center)
            .css_classes(["pill"])
            .build();

        // Export permissions row
        let export_row = adw::ActionRow::builder()
            .title("Export Agent Permissions")
            .subtitle("One-way export: push Agent Terminal's saved rules to permissions.toml and agent configurations")
            .build();
        let export_btn = gtk4::Button::builder()
            .label("Export Now")
            .valign(gtk4::Align::Center)
            .css_classes(["pill"])
            .build();

        // Manage rules row
        let rule_count = crate::always_allow::path()
            .map(|p| crate::always_allow::AlwaysRules::load(&p).rules.len())
            .unwrap_or(0);
        let manage_row = adw::ActionRow::builder()
            .title("Saved Permission Rules")
            .subtitle(format!(
                "{rule_count} active rules stored in {}",
                crate::always_allow::FILE_NAME
            ))
            .build();
        let manage_btn = gtk4::Button::builder()
            .label("Manage Rules…")
            .valign(gtk4::Align::Center)
            .css_classes(["pill"])
            .build();

        // Codex's rules folder, as the Codex threads resolve it: shown so the user can see
        // where import reads and export writes.
        let codex_row = adw::ActionRow::builder()
            .title("Codex Rules")
            .use_markup(false)
            .subtitle("Looking for Codex's rules folder…")
            .subtitle_selectable(true)
            .build();

        // What the export confirmation says, counts included, from the newest check.
        let export_text = Rc::new(RefCell::new(crate::always_allow::EXPORT_WARNING.to_owned()));

        // The rules file and the permission files are read off the main thread; only the newest
        // result is shown.
        let sync_generation = Rc::new(std::cell::Cell::new(0u64));
        let sync_cb = Rc::new(glib::clone!(
            #[weak]
            warning_row,
            #[weak]
            import_row,
            #[weak]
            import_btn,
            #[weak]
            export_row,
            #[weak]
            export_btn,
            #[weak]
            manage_row,
            #[weak]
            codex_row,
            #[weak]
            obj,
            #[strong]
            sync_generation,
            #[strong]
            export_text,
            move || {
                let Some(path) = crate::always_allow::path() else {
                    return;
                };
                let sources = obj.imp().permission_sources();
                let codex_inputs = obj.imp().codex_inputs();
                let current = sync_generation.get().wrapping_add(1);
                sync_generation.set(current);
                let sync_generation = sync_generation.clone();
                let export_text = export_text.clone();
                glib::spawn_future_local(glib::clone!(
                    #[weak]
                    warning_row,
                    #[weak]
                    import_row,
                    #[weak]
                    import_btn,
                    #[weak]
                    export_row,
                    #[weak]
                    export_btn,
                    #[weak]
                    manage_row,
                    #[weak]
                    codex_row,
                    async move {
                        let codex = codex_target(codex_inputs).await;
                        let computed = gtk4::gio::spawn_blocking(move || {
                            // An unusable file is named, never shown as "0 rules".
                            let opened = crate::always_allow::AlwaysRules::open(&path);
                            let blocked = opened
                                .as_ref()
                                .err()
                                .map(|e| crate::always_allow::blocked_notice(&path, e));
                            let rules = opened.unwrap_or_default();
                            let target = codex.as_ref().ok();
                            let status = rules.check_sync_status(None, &sources, target);
                            let available = target.is_some_and(|t| t.available());
                            let warning = rules.export_warning_for(target);
                            let mut described = match &codex {
                                Ok(t) => t.describe(),
                                Err(reason) => format!("Codex's rules folder is unknown: {reason}"),
                            };
                            if let Some(reason) = &status.codex_refused {
                                described.push_str(&format!(
                                    ". Codex's own rules are not imported: {reason}"
                                ));
                            }
                            (
                                rules.rules.len(),
                                blocked,
                                status,
                                available,
                                warning,
                                described,
                            )
                        })
                        .await;
                        let Ok((count, blocked, status, available, warning, described)) = computed
                        else {
                            return;
                        };
                        if sync_generation.get() != current {
                            return;
                        }
                        manage_row.set_subtitle(&blocked.unwrap_or_else(|| {
                            format!(
                                "{count} active rules stored in {}",
                                crate::always_allow::FILE_NAME
                            )
                        }));
                        codex_row.set_subtitle(&described);
                        *export_text.borrow_mut() = warning;
                        Self::update_sync_indicator(
                            &status,
                            available,
                            &warning_row,
                            &import_row,
                            &import_btn,
                            &export_row,
                            &export_btn,
                        );
                    }
                ));
            }
        ));

        // Initial sync state check
        sync_cb();

        // Applied once valid, ~400 ms after typing stops (the file checks run off the main
        // thread) or at once when focus leaves. An invalid draft stays in the entry with its
        // error shown inline, and the previously saved list stays in use; nothing reverts.
        let on_sources = sync_cb.clone();
        let typing = Rc::new(std::cell::Cell::new(0u64));
        let validate = Rc::new(glib::clone!(
            #[weak]
            obj,
            #[weak]
            sources_row,
            #[weak]
            sources_error,
            #[strong]
            typing,
            move |wait: bool| {
                let current = typing.get().wrapping_add(1);
                typing.set(current);
                let text = sources_row.text().to_string();
                let typing = typing.clone();
                let on_sources = on_sources.clone();
                glib::spawn_future_local(glib::clone!(
                    #[weak]
                    obj,
                    #[weak]
                    sources_row,
                    #[weak]
                    sources_error,
                    async move {
                        if wait {
                            glib::timeout_future(std::time::Duration::from_millis(400)).await;
                            if typing.get() != current {
                                return;
                            }
                        }
                        let checked = gtk4::gio::spawn_blocking(move || {
                            crate::config::parse_permission_sources(&text)
                        })
                        .await;
                        let Ok(checked) = checked else { return };
                        if typing.get() != current {
                            return; // typed on meanwhile; that edit has its own check
                        }
                        match checked {
                            Ok(parsed) => {
                                show_validation(
                                    &sources_row,
                                    &sources_error,
                                    PERMISSION_SOURCES_HELP,
                                    None,
                                );
                                let imp = obj.imp();
                                if imp.config.borrow().permission_sources != parsed {
                                    imp.config.borrow_mut().permission_sources = parsed;
                                    imp.schedule_config_save();
                                    on_sources();
                                }
                            }
                            Err(reason) => show_validation(
                                &sources_row,
                                &sources_error,
                                PERMISSION_SOURCES_HELP,
                                Some(&reason),
                            ),
                        }
                    }
                ));
            }
        ));
        let on_change = validate.clone();
        sources_row.connect_changed(move |_| on_change(true));
        let focus = gtk4::EventControllerFocus::new();
        focus.connect_leave(move |_| validate(false));
        sources_row.add_controller(focus);

        let on_sync_for_diff = sync_cb.clone();
        view_diff_btn.connect_clicked(glib::clone!(
            #[weak]
            obj,
            move |btn| {
                let imp = obj.imp();
                imp.show_sync_differences_dialog(btn.upcast_ref(), on_sync_for_diff.clone());
            }
        ));

        let on_sync = sync_cb.clone();
        let import_btn_weak = import_btn.downgrade();
        import_btn.connect_clicked(glib::clone!(
            #[weak]
            obj,
            move |_| {
                let Some(btn) = import_btn_weak.upgrade() else {
                    return;
                };
                btn.set_sensitive(false);
                btn.set_label("Syncing…");
                let obj_weak = obj.downgrade();
                let on_sync = on_sync.clone();
                let btn_weak = btn.downgrade();
                glib::timeout_add_local_once(std::time::Duration::from_millis(300), move || {
                    let Some(obj) = obj_weak.upgrade() else {
                        return;
                    };
                    let imp = obj.imp();
                    // E4: the button and the sync indicator follow the finished job.
                    let on_sync = on_sync.clone();
                    let done = move || mark_synced(&btn_weak, move || on_sync());
                    match crate::always_allow::path() {
                        Some(path) => imp.import_rules(&path, done),
                        None => done(),
                    }
                });
            }
        ));
        import_row.add_suffix(&import_btn);
        group.add(&import_row);

        let on_sync = sync_cb.clone();
        let export_btn_weak = export_btn.downgrade();
        export_btn.connect_clicked(glib::clone!(
            #[weak]
            obj,
            #[strong]
            export_text,
            move |clicked| {
                let Some(btn) = export_btn_weak.upgrade() else {
                    return;
                };
                if !confirm_export_first(clicked, &export_text.borrow()) {
                    return;
                }
                btn.set_sensitive(false);
                btn.set_label("Syncing…");
                let obj_weak = obj.downgrade();
                let on_sync = on_sync.clone();
                let btn_weak = btn.downgrade();
                glib::timeout_add_local_once(std::time::Duration::from_millis(300), move || {
                    let Some(obj) = obj_weak.upgrade() else {
                        return;
                    };
                    let imp = obj.imp();
                    // E4: the button and the sync indicator follow the finished job.
                    let on_sync = on_sync.clone();
                    let done = move || mark_synced(&btn_weak, move || on_sync());
                    match crate::always_allow::path() {
                        Some(path) => imp.export_rules(&path, done),
                        None => done(),
                    }
                });
            }
        ));
        export_row.add_suffix(&export_btn);
        group.add(&export_row);
        group.add(&codex_row);

        let on_sync = sync_cb.clone();
        manage_btn.connect_clicked(glib::clone!(
            #[weak]
            obj,
            move |btn| {
                let imp = obj.imp();
                imp.show_rules_dialog(btn.upcast_ref(), Some(on_sync.clone()));
            }
        ));
        manage_row.add_suffix(&manage_btn);
        group.add(&manage_row);

        group
    }

    fn update_sync_indicator(
        status: &crate::always_allow::PermissionSyncStatus,
        codex: bool,
        warning_row: &adw::ActionRow,
        import_row: &adw::ActionRow,
        import_btn: &gtk4::Button,
        export_row: &adw::ActionRow,
        export_btn: &gtk4::Button,
    ) {
        let import_idle = format!(
            "One-way import: pull allowlisted commands and tools from {}",
            source_names(codex)
        );
        let export_idle = if codex {
            "One-way export: push Agent Terminal's saved rules to permissions.toml, agent \
             configurations and Codex's agent-terminal.rules"
        } else {
            "One-way export: push Agent Terminal's saved rules to permissions.toml and agent \
             configurations"
        };
        if status.is_out_of_sync() {
            warning_row.set_subtitle(&status.summary());
            warning_row.set_visible(true);

            if status.unimported_count > 0 {
                import_row.add_css_class("warning");
                import_row.set_subtitle(&format!(
                    "One-way import: pull {} unimported rule{} from {}",
                    status.unimported_count,
                    if status.unimported_count == 1 {
                        ""
                    } else {
                        "s"
                    },
                    source_names(codex)
                ));
                import_btn.set_sensitive(true);
                import_btn.set_label("Import Now");
            } else {
                import_row.remove_css_class("warning");
                import_row.set_subtitle(&import_idle);
                import_btn.set_sensitive(false);
                import_btn.set_label("Synced");
            }

            if status.unexported_count > 0 {
                export_row.add_css_class("warning");
                export_row.set_subtitle(&format!(
                    "One-way export: push {} unsynced rule{} to agent configurations",
                    status.unexported_count,
                    if status.unexported_count == 1 {
                        ""
                    } else {
                        "s"
                    }
                ));
                export_btn.set_sensitive(true);
                export_btn.set_label("Export Now");
            } else {
                export_row.remove_css_class("warning");
                export_row.set_subtitle(export_idle);
                export_btn.set_sensitive(false);
                export_btn.set_label("Synced");
            }
        } else {
            warning_row.set_visible(false);
            import_row.remove_css_class("warning");
            import_row.set_subtitle(&import_idle);
            import_btn.set_sensitive(false);
            import_btn.set_label("Synced");

            export_row.remove_css_class("warning");
            export_row.set_subtitle(export_idle);
            export_btn.set_sensitive(false);
            export_btn.set_label("Synced");
        }
    }

    /// Lists the differences between the rules file and every agent's own (Codex's folder
    /// included when it resolves), read off the main thread.
    fn show_sync_differences_dialog(&self, parent: &gtk4::Widget, _on_sync: Rc<dyn Fn()>) {
        let Some(path) = crate::always_allow::path() else {
            return;
        };
        let sources = self.permission_sources();
        let codex_inputs = self.codex_inputs();
        let parent = parent.downgrade();
        glib::spawn_future_local(async move {
            let codex = codex_target(codex_inputs).await.ok();
            let computed = gtk4::gio::spawn_blocking(move || {
                let rules = crate::always_allow::AlwaysRules::load(&path);
                let available = codex.as_ref().is_some_and(|t| t.available());
                (
                    rules.check_sync_status(None, &sources, codex.as_ref()),
                    available,
                )
            })
            .await;
            let (Ok((status, codex)), Some(parent)) = (computed, parent.upgrade()) else {
                return;
            };
            Self::present_sync_differences(&parent, &status, codex);
        });
    }

    fn present_sync_differences(
        parent: &gtk4::Widget,
        status: &crate::always_allow::PermissionSyncStatus,
        codex: bool,
    ) {
        let content = gtk4::Box::new(gtk4::Orientation::Vertical, 16);
        content.set_margin_top(12);
        content.set_margin_bottom(12);
        content.set_margin_start(16);
        content.set_margin_end(16);

        if !status.unimported_rules.is_empty() {
            let label = gtk4::Label::new(Some(&format!(
                "Unimported Rules from Agent Configs ({})",
                status.unimported_rules.len()
            )));
            label.add_css_class("heading");
            label.set_halign(gtk4::Align::Start);
            content.append(&label);

            let desc = gtk4::Label::new(Some(&format!(
                "These rules exist in {} but are missing in Agent Terminal:",
                source_names(codex)
            )));
            desc.set_wrap(true);
            desc.set_halign(gtk4::Align::Start);
            desc.add_css_class("dim-label");
            content.append(&desc);

            let list = gtk4::ListBox::new();
            list.add_css_class("boxed-list");
            for r in &status.unimported_rules {
                let row = adw::ActionRow::builder().use_markup(false).title(r).build();
                let icon = gtk4::Image::from_icon_name("at-go-down-symbolic");
                row.add_prefix(&icon);
                list.append(&row);
            }
            content.append(&list);
        }

        if !status.unexported_rules.is_empty() {
            let label = gtk4::Label::new(Some(&format!(
                "Unexported Rules from Agent Terminal ({})",
                status.unexported_rules.len()
            )));
            label.add_css_class("heading");
            label.set_halign(gtk4::Align::Start);
            content.append(&label);

            let desc = gtk4::Label::new(Some(&format!(
                "What the next export changes in {}:",
                source_names(codex)
            )));
            desc.set_wrap(true);
            desc.set_halign(gtk4::Align::Start);
            desc.add_css_class("dim-label");
            content.append(&desc);

            let list = gtk4::ListBox::new();
            list.add_css_class("boxed-list");
            for r in &status.unexported_rules {
                let row = adw::ActionRow::builder().use_markup(false).title(r).build();
                let icon = gtk4::Image::from_icon_name("at-go-up-symbolic");
                row.add_prefix(&icon);
                list.append(&row);
            }
            content.append(&list);
        }

        if status.unimported_rules.is_empty() && status.unexported_rules.is_empty() {
            let label = gtk4::Label::new(Some("All permission rules are currently synchronized!"));
            label.add_css_class("heading");
            label.set_halign(gtk4::Align::Center);
            content.append(&label);
        }

        let scroller = gtk4::ScrolledWindow::builder()
            .hscrollbar_policy(gtk4::PolicyType::Never)
            .min_content_height(240)
            .max_content_height(420)
            .min_content_width(460)
            .child(&content)
            .build();

        let dialog = adw::AlertDialog::new(
            Some("Permission Synchronization Differences"),
            Some(&status.summary()),
        );
        dialog.set_extra_child(Some(&scroller));
        dialog.add_response("close", "Close");
        dialog.set_default_response(Some("close"));
        dialog.present(Some(parent));
    }

    /// Opens the rules list once the rules file and the agents' files (Codex's folder included
    /// when it resolves) have been read off the main thread.
    fn show_rules_dialog(&self, parent: &gtk4::Widget, on_changed: Option<Rc<dyn Fn()>>) {
        let Some(path) = crate::always_allow::path() else {
            return;
        };
        let sources = self.permission_sources();
        let codex_inputs = self.codex_inputs();
        let parent = parent.downgrade();
        let obj = self.obj().downgrade();
        glib::spawn_future_local(async move {
            let codex = codex_target(codex_inputs).await.ok();
            let target = path.clone();
            let computed = gtk4::gio::spawn_blocking(move || {
                let opened = crate::always_allow::AlwaysRules::open(&target);
                let blocked = opened
                    .as_ref()
                    .err()
                    .map(|e| crate::always_allow::blocked_notice(&target, e));
                let rules = opened.unwrap_or_default();
                let status = rules.check_sync_status(None, &sources, codex.as_ref());
                let warning = rules.export_warning_for(codex.as_ref());
                (rules, blocked, status, warning)
            })
            .await;
            let (Ok((rules, blocked, status, warning)), Some(parent), Some(obj)) =
                (computed, parent.upgrade(), obj.upgrade())
            else {
                return;
            };
            obj.imp().present_rules_dialog(
                &parent,
                path,
                RulesView {
                    rules,
                    blocked,
                    status,
                    warning,
                },
                on_changed,
            );
        });
    }

    fn present_rules_dialog(
        &self,
        parent: &gtk4::Widget,
        path: std::path::PathBuf,
        view: RulesView,
        on_changed: Option<Rc<dyn Fn()>>,
    ) {
        let RulesView {
            rules,
            blocked,
            status,
            warning,
        } = view;
        let subtitle = if let Some(blocked) = blocked {
            blocked
        } else if status.is_out_of_sync() {
            format!(
                "Warning: {}. Rules that automatically approve matching commands and tools.",
                status.summary()
            )
        } else {
            "Rules that automatically approve matching commands and tools.".to_string()
        };

        let dialog = adw::AlertDialog::new(Some("Permission Rules"), Some(&subtitle));

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
            for r in &rules.rules {
                let ws_label = if r.workspace == "*" {
                    "All workspaces"
                } else {
                    &r.workspace
                };
                let row = adw::ActionRow::builder()
                    .use_markup(false)
                    .title(r.detail())
                    .subtitle(format!(
                        "{} • {} • from {}",
                        r.kind.label(),
                        ws_label,
                        r.origin
                    ))
                    .build();
                let del_btn = gtk4::Button::builder()
                    .icon_name("at-user-trash-symbolic")
                    .valign(gtk4::Align::Center)
                    .css_classes(["flat", "destructive-action"])
                    .tooltip_text("Delete this rule")
                    .build();
                let obj = self.obj();
                let p = path.clone();
                let rule = r.clone();
                let cb = on_changed.clone();
                del_btn.connect_clicked(glib::clone!(
                    #[weak]
                    obj,
                    #[weak]
                    row,
                    move |_| {
                        // By the rule itself, not its position: the file may have changed since
                        // this list was built. Off the main thread (the store lock); the row is
                        // hidden only once the removal is saved.
                        let obj = obj.downgrade();
                        let row = row.downgrade();
                        let cb = cb.clone();
                        let shown = p.clone();
                        Self::forget_rule(p.clone(), rule.clone(), move |result| {
                            let Some(obj) = obj.upgrade() else { return };
                            let imp = obj.imp();
                            match result {
                                Ok(true) => {
                                    if let Some(row) = row.upgrade() {
                                        row.set_visible(false);
                                    }
                                    imp.show_toast("Rule removed");
                                    if let Some(ref f) = cb {
                                        f();
                                    }
                                }
                                Ok(false) => imp.show_toast("That rule was already gone"),
                                Err(e) => {
                                    imp.show_toast(&crate::always_allow::blocked_notice(&shown, &e))
                                }
                            }
                        });
                    }
                ));
                row.add_suffix(&del_btn);
                list.append(&row);
            }
        }
        scroll.set_child(Some(&list));
        dialog.set_extra_child(Some(&scroll));
        dialog.add_responses(&[
            ("import", "Import Rules"),
            ("export", "Export Rules"),
            ("close", "Close"),
        ]);
        dialog.set_default_response(Some("close"));
        dialog.set_close_response("close");

        let root = parent.root();
        let obj = self.obj();
        let p = path.clone();
        glib::MainContext::default().spawn_local(glib::clone!(
            #[weak]
            obj,
            async move {
                let response = dialog.choose_future(root.as_ref()).await;
                let imp = obj.imp();
                match response.as_str() {
                    "import" => {
                        // E4: refreshed once the import has finished.
                        imp.import_rules(&p, move || {
                            if let Some(f) = on_changed {
                                f();
                            }
                        });
                    }
                    "export" => {
                        let confirm = adw::AlertDialog::new(
                            Some("Export rules outside agent-terminal?"),
                            Some(&warning),
                        );
                        confirm.add_responses(&[("cancel", "Cancel"), ("export", "Export")]);
                        confirm.set_response_appearance(
                            "export",
                            adw::ResponseAppearance::Destructive,
                        );
                        confirm.set_default_response(Some("cancel"));
                        confirm.set_close_response("cancel");
                        if confirm.choose_future(root.as_ref()).await == "export" {
                            // E4: refreshed once the export has finished.
                            imp.export_rules(&p, move || {
                                if let Some(f) = on_changed {
                                    f();
                                }
                            });
                        }
                    }
                    _ => {}
                }
            }
        ));
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
        let delegation = adw::SwitchRow::builder()
            .title("Available for delegation")
            .subtitle("Allow this agent to delegate and receive work in Agent Terminal. Verified routes only; writing agents use isolated worktrees.")
            .active(profile.available_for_delegation)
            .build();
        let obj = self.obj();
        delegation.connect_active_notify(glib::clone!(
            #[weak]
            obj,
            move |row| {
                obj.imp()
                    .edit_agent_profile(driver, |p| p.available_for_delegation = row.is_active());
            }
        ));
        group.add(&delegation);
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
                imp.refresh_agent_data(super::threads::Probes::Force);
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
        let command_timer = Rc::new(EditDebounce::default());
        self.agent_edits
            .borrow_mut()
            .push(Rc::downgrade(&command_timer));
        dialog.connect_closed(glib::clone!(
            #[strong]
            command_timer,
            #[weak]
            obj,
            move |_| {
                command_timer.flush();
                obj.imp().flush_pending_save();
            }
        ));
        command.connect_changed(glib::clone!(
            #[strong]
            command_timer,
            #[weak]
            obj,
            #[strong]
            detect,
            #[weak]
            command_error,
            move |row| {
                command_timer.cancel();
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
                let obj = obj.downgrade();
                let detect = detect.clone();
                command_timer.schedule(move || {
                    let Some(obj) = obj.upgrade() else {
                        return;
                    };
                    let imp = obj.imp();
                    let changed = imp.agent_profile_now(driver).command != text;
                    if changed {
                        imp.edit_agent_profile(driver, |p| p.command = text.clone());
                    }
                    if !imp.is_closing() {
                        detect(text);
                        if changed {
                            imp.refresh_profile_selection();
                            imp.refresh_agent_data(super::threads::Probes::Force);
                        }
                    }
                });
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
        let env_timer = Rc::new(EditDebounce::default());
        self.agent_edits
            .borrow_mut()
            .push(Rc::downgrade(&env_timer));
        dialog.connect_closed(glib::clone!(
            #[strong]
            env_timer,
            #[weak]
            obj,
            move |_| {
                env_timer.flush();
                obj.imp().flush_pending_save();
            }
        ));
        env_file.connect_changed(glib::clone!(
            #[strong]
            env_timer,
            #[weak]
            obj,
            #[weak]
            env_error,
            move |row| {
                env_timer.cancel();
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
                let obj = obj.downgrade();
                env_timer.schedule(move || {
                    let Some(obj) = obj.upgrade() else {
                        return;
                    };
                    let imp = obj.imp();
                    if imp.agent_profile_now(driver).env_file != value {
                        imp.edit_agent_profile(driver, |p| p.env_file = value);
                        if !imp.is_closing() {
                            imp.refresh_agent_data(super::threads::Probes::Force);
                        }
                    }
                });
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
                let from_settings = crate::config::agent_settings_model(driver, None);
                let default_label = match &from_settings {
                    Some(m) => format!("The CLI's default ({m})"),
                    None => "The CLI's default".to_owned(),
                };
                let mut ids: Vec<Option<String>> = vec![None];
                let mut labels: Vec<String> = vec![default_label];
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
                 installed, Antigravity cannot ask before acting: it refuses shell commands but \
                 applies file edits on its own, in Plan mode too. Each thread's own directory \
                 is checked when the thread opens.",
            )
            .build();
        expander.add_row(&json);
        group.add(&expander);
        group.add(&always_allowed_rows());

        glib::MainContext::default().spawn_local(glib::clone!(
            #[weak]
            hook,
            async move {
                // Settings checks the home-level installation, never a thread's launch proof.
                let target = super::threads::agy_hook_target(None).await;
                let (probe, bound) = gtk4::gio::spawn_blocking(move || {
                    (
                        crate::hook_config::probe_settings(&target),
                        crate::approval_server::socket_bindable(),
                    )
                })
                .await
                .unwrap_or_else(|_| {
                    (
                        crate::hook_config::HookProbe::Unreadable(
                            "the hooks check panicked".into(),
                        ),
                        false,
                    )
                });
                let badge = crate::hook_config::badge(&probe, bound);
                crate::hook_config::log_verdict(badge, &probe, bound);
                let (text, class) = match badge {
                    crate::hook_config::HookBadge::Ready => (
                        "Ready: Hook entry present and runtime directory available",
                        None,
                    ),
                    crate::hook_config::HookBadge::Missing => (
                        "Not installed: Antigravity cannot ask before acting until the entry below is added",
                        Some("error"),
                    ),
                    crate::hook_config::HookBadge::Unreadable => (
                        "Could not verify the approval hook for a thread. Treat Antigravity as ungated",
                        Some("warning"),
                    ),
                };
                let detail = match &probe {
                    crate::hook_config::HookProbe::Unreadable(why) => format!("Could not verify: {why}"),
                    _ => text.to_owned(),
                };
                let detail = format!(
                    "{detail}\nAny other Antigravity hooks file (home or plugin) must also route \
                     through this app's approval hook, or Antigravity runs without skipping permissions."
                );
                hook.set_subtitle(&detail);
                hook.remove_css_class("error");
                hook.remove_css_class("warning");
                if let Some(class) = class {
                    hook.add_css_class(class);
                }
            }
        ));
    }
}

/// Shows a sync button as done ("✓ Synced", then "Synced" a second later) and then runs
/// `refresh` (the sync indicator). Called from the background job's completion (E4).
fn mark_synced(button: &glib::WeakRef<Button>, refresh: impl Fn() + 'static) {
    let Some(btn) = button.upgrade() else {
        refresh();
        return;
    };
    btn.set_label("✓ Synced");
    btn.add_css_class("suggested-action");
    let fade = btn.downgrade();
    glib::timeout_add_local_once(std::time::Duration::from_millis(1000), move || {
        if let Some(btn) = fade.upgrade() {
            btn.remove_css_class("suggested-action");
            btn.set_label("Synced");
            btn.set_sensitive(false);
        }
        refresh();
    });
}

/// The widget name a confirmed export click carries (see [`confirm_export_first`]).
const EXPORT_CONFIRMED: &str = "export-confirmed";

/// Export writes standing grants outside agent-terminal, so it is confirmed first: the first
/// click shows `warning` ([`crate::always_allow::export_warning`]) and returns false; choosing Export there
/// clicks `button` again, marked confirmed, and that click returns true.
fn confirm_export_first(button: &Button, warning: &str) -> bool {
    if button.widget_name() == EXPORT_CONFIRMED {
        button.set_widget_name("");
        return true;
    }
    let dialog = adw::AlertDialog::new(Some("Export rules outside agent-terminal?"), Some(warning));
    dialog.add_responses(&[("cancel", "Cancel"), ("export", "Export")]);
    dialog.set_response_appearance("export", adw::ResponseAppearance::Destructive);
    dialog.set_default_response(Some("cancel"));
    dialog.set_close_response("cancel");
    let weak = button.downgrade();
    dialog.connect_response(None, move |_, response| {
        if response == "export" {
            if let Some(button) = weak.upgrade() {
                button.set_widget_name(EXPORT_CONFIRMED);
                button.emit_clicked();
            }
        }
    });
    dialog.present(Some(button));
    false
}

/// Antigravity's remembered "Always allow" rules, each with a Remove button. A removal is saved at once
/// and applies to Antigravity sessions started after it (a running one keeps what it already allowed).
fn always_allowed_rows() -> adw::ExpanderRow {
    use crate::always_allow::{self, AlwaysRules};
    let path = always_allow::path();
    let opened = path.as_deref().map(AlwaysRules::open);
    let subtitle = match (&opened, &path) {
        (Some(Err(e)), Some(p)) => always_allow::blocked_notice(p, e),
        (Some(Ok(rules)), _) if !rules.rules.is_empty() => format!(
            "{} remembered. Removing one applies at once",
            rules.rules.len()
        ),
        _ => "None yet. “Always allow” on an approval adds one, for that folder only".to_owned(),
    };
    let rules = opened.and_then(Result::ok).unwrap_or_default();
    let expander = adw::ExpanderRow::builder()
        .title("Always-Allowed Actions")
        .subtitle(subtitle)
        .build();
    for rule in rules.rules {
        let row = adw::ActionRow::builder()
            .use_markup(false)
            .title(rule.summary())
            .subtitle(format!("{} • from {}", rule.workspace, rule.origin))
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
                // Off the main thread (the store lock); the row goes once the removal is saved.
                let shown = path.clone();
                let row = row.downgrade();
                let expander = expander.downgrade();
                AgentTerminalWindow::forget_rule(path, rule.clone(), move |result| {
                    let (Some(row), Some(expander)) = (row.upgrade(), expander.upgrade()) else {
                        return;
                    };
                    match result {
                        Ok(_) => expander.remove(&row),
                        Err(e) => {
                            warn!(error = %e, "could not save the always-allow rules");
                            // The rule stays, and its row says why it could not be removed.
                            let notice = always_allow::blocked_notice(&shown, &e);
                            row.set_subtitle(&notice);
                            if let Some(window) = row
                                .root()
                                .and_downcast::<crate::window::AgentTerminalWindow>()
                            {
                                window.imp().show_toast(&notice);
                            }
                        }
                    }
                });
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

    #[test]
    fn dropping_a_debounce_saves_its_pending_edit() {
        let saved = Rc::new(std::cell::Cell::new(false));
        let timer = EditDebounce::default();
        let seen = saved.clone();
        *timer.pending.borrow_mut() = Some(std::boxed::Box::new(move || seen.set(true)));
        drop(timer);
        assert!(saved.get());
    }

    #[test]
    #[ignore = "needs a display; run alone in an isolated display"]
    fn closing_settings_and_window_flushes_agent_edits() {
        gtk4::init().unwrap();
        adw::init().unwrap();
        let config_home = tempfile::tempdir().unwrap();
        std::env::set_var("XDG_CONFIG_HOME", config_home.path());
        let env_file = config_home.path().join("agent.env");
        std::fs::write(&env_file, "TEST_FLAG=yes\n").unwrap();
        let app = adw::Application::builder()
            .application_id("org.test.SettingsFlush")
            .flags(gtk4::gio::ApplicationFlags::NON_UNIQUE)
            .build();
        app.register(None::<&gtk4::gio::Cancellable>).unwrap();
        let window = crate::window::AgentTerminalWindow::new(&app);
        window.present();
        let prefs = adw::PreferencesDialog::new();
        let page = adw::PreferencesPage::new();
        let group = adw::PreferencesGroup::new();
        page.add(&group);
        prefs.add(&page);
        let saved = window.imp().agent_profile_now(Driver::Codex);
        window
            .imp()
            .add_agent_detail_rows(&group, Driver::Codex, &saved, &prefs);
        prefs.present(Some(&window));
        let ctx = glib::MainContext::default();
        // Closing during presentation is not the user's Settings path on libadwaita 1.5.
        assert!(crate::testutil::pump_until(&ctx, 5, || prefs.is_mapped()));
        crate::testutil::settle(&ctx);
        let widgets = crate::testutil::descendants(group.upcast_ref());
        let field = |title: &str| {
            widgets
                .iter()
                .find_map(|w| {
                    w.downcast_ref::<adw::EntryRow>()
                        .filter(|r| r.title() == title)
                })
                .unwrap()
        };
        let closed = Rc::new(std::cell::Cell::new(false));
        let checked = closed.clone();
        let weak = window.downgrade();
        let expected_env = env_file.to_string_lossy().into_owned();
        let expected = expected_env.clone();
        prefs.connect_closed(move |_| {
            let window = weak.upgrade().unwrap();
            let profile = window.imp().agent_profile_now(Driver::Codex);
            assert_eq!(profile.command, "/nonexistent/close-dialog/codex");
            assert_eq!(profile.env_file.as_deref(), Some(expected.as_str()));
            let on_disk = crate::config::TerminalConfig::load();
            assert_eq!(
                on_disk.agent_profile(Driver::Codex).unwrap().command,
                profile.command
            );
            assert_eq!(
                on_disk.agent_profile(Driver::Codex).unwrap().env_file,
                profile.env_file
            );
            checked.set(true);
        });
        field("Command or Path").set_text("/nonexistent/close-dialog/codex");
        field("Environment File").set_text(&expected_env);
        assert!(prefs.close(), "the presented Settings dialog must close");
        assert!(crate::testutil::pump_until(&ctx, 5, || closed.get()));
        // The rows are still alive here; the window close must flush its own registry too.
        field("Command or Path").set_text("/nonexistent/close-window/codex");
        field("Environment File").set_text("");
        window.close();
        let profile = window.imp().agent_profile_now(Driver::Codex);
        assert_eq!(profile.command, "/nonexistent/close-window/codex");
        assert_eq!(profile.env_file, None);
        let on_disk = crate::config::TerminalConfig::load();
        assert_eq!(
            on_disk.agent_profile(Driver::Codex).unwrap().command,
            profile.command
        );
        assert_eq!(on_disk.agent_profile(Driver::Codex).unwrap().env_file, None);
    }

    #[test]
    #[ignore = "needs a display; run alone in an isolated display"]
    fn invalid_text_keeps_the_saved_command_detection() {
        gtk4::init().unwrap();
        adw::init().unwrap();
        let config_home = tempfile::tempdir().unwrap();
        std::env::set_var("XDG_CONFIG_HOME", config_home.path());
        let app = adw::Application::builder()
            .application_id("org.test.InvalidDetection")
            .flags(gtk4::gio::ApplicationFlags::NON_UNIQUE)
            .build();
        app.register(None::<&gtk4::gio::Cancellable>).unwrap();
        let window = crate::window::AgentTerminalWindow::new(&app);
        let prefs = adw::PreferencesDialog::new();
        let group = adw::PreferencesGroup::new();
        let profile = Profile {
            command: "/nonexistent/codex".into(),
            ..crate::config::new_agent_profile(Driver::Codex)
        };
        window
            .imp()
            .add_agent_detail_rows(&group, Driver::Codex, &profile, &prefs);
        let widgets = crate::testutil::descendants(group.upcast_ref());
        let command = widgets
            .iter()
            .find_map(|w| {
                w.downcast_ref::<adw::EntryRow>()
                    .filter(|r| r.title() == "Command or Path")
            })
            .unwrap();
        let status = widgets
            .iter()
            .find_map(|w| {
                w.downcast_ref::<adw::ActionRow>()
                    .filter(|r| r.title() == "Detected")
            })
            .unwrap();
        command.set_text("--invalid"); // before the initial detection future can answer
        assert!(crate::testutil::pump_until(
            &glib::MainContext::default(),
            10,
            || status.subtitle().is_some_and(|s| s != "Checking…")
        ));
        assert!(status.has_css_class("error"));
        window.destroy();
    }

    #[test]
    #[ignore = "needs a display; run alone in an isolated display"]
    fn typing_a_command_runs_one_scan() {
        gtk4::init().unwrap();
        adw::init().unwrap();
        let config_home = tempfile::tempdir().unwrap();
        std::env::set_var("XDG_CONFIG_HOME", config_home.path());
        let app = adw::Application::builder()
            .application_id("org.test.AgentDebounce")
            .flags(gtk4::gio::ApplicationFlags::NON_UNIQUE)
            .build();
        app.register(None::<&gtk4::gio::Cancellable>).unwrap();
        let window = crate::window::AgentTerminalWindow::new(&app);
        let ctx = glib::MainContext::default();
        crate::testutil::settle(&ctx);
        let group = adw::PreferencesGroup::new();
        let prefs = adw::PreferencesDialog::new();
        let saved = window.imp().agent_profile_now(Driver::Codex);
        window
            .imp()
            .add_agent_detail_rows(&group, Driver::Codex, &saved, &prefs);
        let widgets = crate::testutil::descendants(group.upcast_ref());
        let field = widgets
            .iter()
            .find_map(|w| {
                w.downcast_ref::<adw::EntryRow>()
                    .filter(|r| r.title() == "Command or Path")
            })
            .unwrap();
        let status = widgets
            .iter()
            .find_map(|w| {
                w.downcast_ref::<adw::ActionRow>()
                    .filter(|r| r.title() == "Detected")
            })
            .unwrap();
        let before = super::super::threads::scan_generation_for_test();
        // Preserve the executable basename: profile_driver deliberately gives wrappers with
        // other names no chat adapter. This tests path edits within the Codex profile.
        let final_command = "/nonexistent/final/codex";
        for text in [
            "/nonexistent/first/codex",
            "/nonexistent/draft/codex",
            final_command,
        ] {
            field.set_text(text);
        }
        assert_eq!(
            window.imp().agent_profile_now(Driver::Codex).command,
            saved.command
        );
        assert!(crate::testutil::pump_until(&ctx, 5, || window
            .imp()
            .agent_profile_now(Driver::Codex)
            .command
            == final_command));
        assert_eq!(
            super::super::threads::scan_generation_for_test(),
            before + 1
        );

        {
            let config = window.imp().config.borrow();
            let stored = config
                .profiles
                .iter()
                .find(|p| p.name == saved.name)
                .unwrap();
            assert_eq!(
                stored.command, final_command,
                "the original profile stores the path"
            );
            assert_eq!(crate::config::profile_driver(stored), Some(Driver::Codex));
        }
        let before = super::super::threads::scan_generation_for_test();
        field.set_text("/nonexistent/valid-draft/codex");
        field.set_text("--invalid");
        let elapsed = std::time::Instant::now();
        assert!(crate::testutil::pump_until(&ctx, 2, || elapsed.elapsed() > AGENT_EDIT_DELAY * 2));
        assert_eq!(
            window.imp().agent_profile_now(Driver::Codex).command,
            final_command
        );
        assert_eq!(super::super::threads::scan_generation_for_test(), before);

        status.set_subtitle("Checking…");
        field.set_text("/nonexistent/another-draft/codex");
        field.set_text(final_command);
        assert!(crate::testutil::pump_until(&ctx, 10, || status
            .subtitle()
            .is_some_and(|s| s != "Checking…")));
        assert_eq!(
            window.imp().agent_profile_now(Driver::Codex).command,
            final_command
        );
        assert_eq!(super::super::threads::scan_generation_for_test(), before);
        window.destroy();
    }

    pub(crate) fn help_texts_are_valid_markup() {
        for text in [
            COMMAND_HELP,
            ARGS_HELP,
            ENV_FILE_HELP,
            PERMISSION_SOURCES_HELP,
        ] {
            gtk4::pango::parse_markup(text, '\0').unwrap();
        }
    }

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

    /// Every row and group title under `root` is non-empty and, when Pango reads it as markup,
    /// valid markup: a bare `&` makes the parse fail and the title renders blank (F20).
    pub(crate) fn assert_titles_render(root: &impl IsA<gtk4::Widget>, what: &str) {
        let mut checked = 0;
        for widget in crate::testutil::descendants(root.upcast_ref()) {
            let titled = if let Some(row) = widget.downcast_ref::<adw::PreferencesRow>() {
                Some((row.title().to_string(), row.uses_markup()))
            } else {
                widget
                    .downcast_ref::<adw::PreferencesGroup>()
                    .map(|g| (g.title().to_string(), true))
            };
            if let Some((title, markup)) = titled {
                // EntryRows and some container rows are titled by their own child widgets.
                if !title.is_empty() || widget.is::<adw::ActionRow>() {
                    assert!(!title.is_empty(), "{what}: an untitled row");
                    if markup {
                        assert!(
                            gtk4::pango::parse_markup(&title, '\0').is_ok(),
                            "{what}: {title:?} is invalid Pango markup and would render blank"
                        );
                    }
                    checked += 1;
                }
            }
        }
        assert!(checked > 0, "{what}: no titled rows found");
    }

    /// A dropdown row shows its whole selected value, even in a narrow window beside a long
    /// subtitle (ui-audit #9: "Agent T…", "Auto…").
    #[test]
    #[ignore = "native GTK layout; run alone in an isolated display"]
    fn native_dropdown_rows_show_their_whole_value() {
        gtk4::init().expect("GTK must be available for the native gate");
        adw::init().expect("libadwaita init");
        crate::load_css();
        let ctx = glib::MainContext::default();
        let config_home = tempfile::tempdir().unwrap();
        std::env::set_var("XDG_CONFIG_HOME", config_home.path());
        let app = adw::Application::builder()
            .application_id("org.test.Dropdowns")
            .flags(gtk4::gio::ApplicationFlags::NON_UNIQUE)
            .build();
        app.register(None::<&gtk4::gio::Cancellable>)
            .expect("registering a non-unique application");
        let win = super::super::super::AgentTerminalWindow::new(&app);
        let rows: Vec<adw::ComboRow> = vec![win.imp().default_agent_row()];
        for width in [360, 1400] {
            for row in &rows {
                let window = gtk4::Window::new();
                window.set_default_size(width, 300);
                let list = gtk4::ListBox::new();
                list.append(row);
                window.set_child(Some(&list));
                window.present();
                crate::testutil::settle(&ctx);
                let selected = row
                    .selected_item()
                    .and_downcast::<gtk4::StringObject>()
                    .expect("a selected item")
                    .string();
                let value = crate::testutil::descendants(row.upcast_ref())
                    .into_iter()
                    .filter_map(|w| w.downcast::<gtk4::Label>().ok())
                    .find(|l| l.text() == selected && l.is_mapped())
                    .expect("the value label");
                assert!(
                    !value.layout().is_ellipsized(),
                    "{selected:?} is truncated at {width}px in {:?}",
                    row.title()
                );
                list.remove(row);
                window.destroy();
            }
        }
    }

    /// Called by the window smoke test on GTK's thread.
    pub(crate) fn pages_have_renderable_titles(imp: &AgentTerminalWindow) {
        let prefs = adw::PreferencesDialog::new();
        imp.add_agents_page(&prefs);
        let page = prefs.visible_page().expect("the Agents page");
        assert_titles_render(&page, "Settings -> Agents");
        let setup = adw::Dialog::new();
        let page = imp.setup_agents_page(&setup);
        assert_titles_render(&page, "setup walkthrough");
    }

    /// D6: a Settings removal never takes the store lock on the GTK thread. While another holder
    /// keeps the lock, the call returns at once and the loop stays free; the removal finishes
    /// once the lock is released.
    #[test]
    fn a_settings_removal_never_blocks_the_main_thread_on_the_store_lock() {
        use crate::always_allow::{AlwaysRules, Context, Rule, FILE_NAME};
        crate::testutil::in_loop(|ctx| {
            let dir = tempfile::tempdir().expect("tmp");
            let path = dir.path().join(FILE_NAME);
            let c = Context {
                driver: "agy",
                workspace: std::path::Path::new("/w"),
                cwd: Some("/w"),
                home: None,
            };
            let rule = Rule::new("*", "Bash", "ls -la")
                .and_then(|r| r.complete(&c))
                .expect("rule");
            AlwaysRules::remember(&path, rule.clone()).expect("saved");
            let lock = std::fs::OpenOptions::new()
                .write(true)
                .open(dir.path().join(format!("{FILE_NAME}.lock")))
                .expect("lock file");
            lock.lock().expect("hold the lock");

            let done: Rc<std::cell::Cell<Option<bool>>> = Rc::default();
            let seen = done.clone();
            let started = std::time::Instant::now();
            AgentTerminalWindow::forget_rule(path.clone(), rule, move |result| {
                seen.set(Some(result.unwrap_or(false)));
            });
            assert!(
                started.elapsed() < std::time::Duration::from_millis(200),
                "the call returned at once"
            );
            for _ in 0..20 {
                ctx.iteration(false);
            }
            assert_eq!(
                done.get(),
                None,
                "still waiting for the lock, off the main thread"
            );
            lock.unlock().expect("release");
            assert!(crate::testutil::pump_until(ctx, 10, || done
                .get()
                .is_some()));
            assert_eq!(done.get(), Some(true));
            assert!(AlwaysRules::open(&path).expect("open").rules.is_empty());
        });
    }

    /// E4: a Settings import's completion (the toast, the sync indicator, `on_changed`) runs
    /// only once the background job has finished: not while it waits for the store lock.
    #[test]
    fn import_completion_waits_for_the_background_job() {
        use crate::always_allow::{AlwaysRules, FILE_NAME};
        crate::testutil::in_loop(|ctx| {
            let dir = tempfile::tempdir().expect("tmp");
            let path = dir.path().join(FILE_NAME);
            let source = dir.path().join("permissions.toml");
            std::fs::write(&source, "[allow]\nshell = [\n    \"ls\",\n]\n").expect("source");
            AlwaysRules::default().save(&path).expect("store");
            let lock = std::fs::OpenOptions::new()
                .write(true)
                .open(dir.path().join(format!("{FILE_NAME}.lock")))
                .expect("lock file");
            lock.lock().expect("hold the lock");

            let finished: Rc<std::cell::Cell<Option<usize>>> = Rc::default();
            let seen = finished.clone();
            let target = path.clone();
            AgentTerminalWindow::store_job(
                move || AlwaysRules::import_into(&target, &[source], None),
                move |result| seen.set(Some(result.and_then(Result::ok).map_or(0, |i| i.added))),
            );
            for _ in 0..20 {
                ctx.iteration(false);
            }
            assert_eq!(finished.get(), None, "nothing runs before the job finishes");
            lock.unlock().expect("release");
            assert!(crate::testutil::pump_until(ctx, 10, || finished
                .get()
                .is_some()));
            assert_eq!(finished.get(), Some(1));
            assert_eq!(AlwaysRules::open(&path).expect("open").rules.len(), 1);
        });
    }

    #[test]
    fn the_default_mode_dropdown_lists_every_mode_once() {
        // Exhaustive: a new mode does not compile until it is placed in `MODES` (or kept out).
        let slot = |m: Mode| match m {
            Mode::Ask | Mode::AcceptEdits | Mode::Plan => MODES.iter().position(|(x, _)| *x == m),
            // Session-only: never a default.
            Mode::Bypass => None,
        };
        for m in [Mode::Ask, Mode::AcceptEdits, Mode::Plan] {
            let i = slot(m).expect("listed");
            assert_eq!(MODES[i].0, m);
        }
        assert!(MODES.iter().all(|(m, _)| *m != Mode::Bypass));
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
                Mode::Bypass => panic!("dangerous permissive mode is never a default"),
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
