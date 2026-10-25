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
use crate::availability::{AgentAvailability, Availability};
use crate::config::{FieldChange, Profile};
use crate::model_catalog::ModelCatalog;

mod delegation;

/// The default-mode dropdown, by index both ways.
const MODES: [Mode; 3] = [Mode::Ask, Mode::AcceptEdits, Mode::Plan];

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

/// The setup summary line for the agents that are `ready` (installed, enabled, and shown to be
/// signed in), and whether it is a warning. While `checking` (a scan or an account probe is
/// out), an empty list is not yet a verdict and a short one may grow.
fn ready_summary(ready: &[&str], checking: bool) -> (String, bool) {
    if !ready.is_empty() {
        let more = if checking {
            ". Still checking others…"
        } else {
            ""
        };
        (format!("Ready: {}{more}", ready.join(", ")), false)
    } else if checking {
        ("Looking for agents…".to_owned(), false)
    } else {
        (
            "No agent is ready yet: threads need an agent that is installed and signed in."
                .to_owned(),
            true,
        )
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

/// What the approvals group is about, on Settings → Agents and in setup.
pub(super) const APPROVAL_GROUP_HELP: &str =
    "How saved rules are used. Each rule is one you chose with Always allow; broad patterns \
     such as 'cargo *' approve more than you may expect.";

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

    /// Keeps `then` until [`Self::flush`] (a commit, the dialog or the window closing) with no
    /// timer: for a row that persists only on commit.
    fn hold(&self, then: impl FnOnce() + 'static) {
        self.cancel();
        *self.pending.borrow_mut() = Some(std::boxed::Box::new(then));
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

/// What an agent's Detected row last found for the saved command, shared with its Enabled row
/// so the two rows never contradict each other ([`enabled_subtitle`]).
#[derive(Default)]
struct DetectedLink {
    /// `None` while a check runs.
    detected: RefCell<Option<Result<String, String>>>,
    /// Redraws the Enabled row; gone once its dialog closed.
    refresh: RefCell<Option<std::rc::Weak<dyn Fn()>>>,
}

impl DetectedLink {
    fn set(&self, found: Option<Result<String, String>>) {
        *self.detected.borrow_mut() = found;
        let refresh = self
            .refresh
            .borrow()
            .as_ref()
            .and_then(std::rc::Weak::upgrade);
        if let Some(refresh) = refresh {
            refresh();
        }
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

// Splits an arguments field as a shell would, as the diff tool's command line is split.
use super::diff_prefs::parse_argv as parse_args;

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

/// Which way [`AgentTerminalWindow::run_rules_sync`] copies rules.
#[derive(Debug, Clone, Copy)]
enum RulesSync {
    /// From the agents' files into the rules file.
    Import,
    /// From the rules file out to the agents' files.
    Export,
}

/// What one read of the rules sees ([`AgentTerminalWindow::read_rules_snapshot`]).
struct RulesRead {
    path: std::path::PathBuf,
    /// Empty when the file cannot be used; `blocked` then says why.
    rules: crate::always_allow::AlwaysRules,
    blocked: Option<String>,
    /// The permission files named in Settings.
    sources: Vec<std::path::PathBuf>,
    /// Codex's folder as Codex threads resolve it, or why it cannot be known.
    codex: Result<crate::always_allow::CodexTarget, String>,
}

/// What the permissions group shows from one read: the rule count, why the rules file cannot
/// be used (if it cannot), the sync status, whether Codex's folder is there, the export
/// confirmation's text and what the Codex row says.
type SyncRead = (
    usize,
    Option<String>,
    crate::always_allow::PermissionSyncStatus,
    bool,
    String,
    String,
);

/// A rounded button for a settings row's suffix.
fn pill_button(label: &str) -> Button {
    Button::builder()
        .label(label)
        .valign(gtk4::Align::Center)
        .css_classes(["pill"])
        .build()
}

/// A settings entry with its hidden "Not Saved" row below it, shown with `help` until a value
/// is refused ([`show_validation`]).
fn validated_entry(title: &str, help: &str, text: &str) -> (adw::EntryRow, adw::ActionRow) {
    let entry = adw::EntryRow::builder()
        .title(title)
        .tooltip_text(help)
        .text(text)
        .build();
    let error = validation_row();
    show_validation(&entry, &error, help, None);
    (entry, error)
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

/// A sync button's label and whether it can be pressed.
#[derive(Debug, PartialEq, Eq)]
struct SyncButton {
    label: &'static str,
    sensitive: bool,
}

/// The import and export buttons for a status. "Synced" is only claimed for what was
/// checked: when Codex's rules could not be read, the import side says "Not Checked".
fn sync_button_states(
    status: &crate::always_allow::PermissionSyncStatus,
) -> (SyncButton, SyncButton) {
    let import = if status.unimported_count > 0 {
        SyncButton {
            label: "Import Now",
            sensitive: true,
        }
    } else if status.is_unknown() {
        SyncButton {
            label: "Not Checked",
            sensitive: false,
        }
    } else {
        SyncButton {
            label: "Synced",
            sensitive: false,
        }
    };
    let export = if status.unexported_count > 0 {
        SyncButton {
            label: "Export Now",
            sensitive: true,
        }
    } else {
        SyncButton {
            label: "Synced",
            sensitive: false,
        }
    };
    (import, export)
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

    /// Imports the agent configurations into the rules file, or exports the global rules to
    /// them (the caller has shown [`crate::always_allow::export_warning`]), off the main thread
    /// (it takes the store lock and reads files). Codex's file is included when Codex's folder
    /// resolves; an export says why when it does not. An unusable rules file is named and left
    /// untouched. `done` runs on the main thread once the job has finished (and its toast is
    /// shown), whatever the outcome.
    fn run_rules_sync(
        &self,
        path: &std::path::Path,
        sync: RulesSync,
        done: impl FnOnce() + 'static,
    ) {
        let path = path.to_owned();
        let sources = self.permission_sources();
        let obj = self.obj().downgrade();
        let target = path.clone();
        let codex_inputs = self.codex_inputs();
        glib::spawn_future_local(async move {
            let codex = codex_target(codex_inputs).await;
            let unresolved = match sync {
                RulesSync::Import => None,
                RulesSync::Export => codex.as_ref().err().cloned(),
            };
            let codex = codex.ok();
            Self::rules_job(
                move || {
                    let codex = codex.as_ref();
                    match sync {
                        RulesSync::Import => {
                            crate::always_allow::AlwaysRules::import_into(&target, &sources, codex)
                                .map(|imported| imported.summary())
                        }
                        RulesSync::Export => {
                            crate::always_allow::AlwaysRules::export_from(&target, &sources, codex)
                                .map(|exported| exported.summary())
                        }
                    }
                },
                move |result| {
                    if let Some(obj) = obj.upgrade() {
                        let imp = obj.imp();
                        match result {
                            Some(Ok(mut text)) => {
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
                            None => imp.show_toast(match sync {
                                RulesSync::Import => "The import did not finish",
                                RulesSync::Export => "The export did not finish",
                            }),
                        }
                    }
                    done();
                },
            );
        });
    }

    /// Resolves Codex's folder (as Codex threads do), then reads the rules file at `path` and
    /// runs `read` on what it holds, both off the main thread (the sync checks read files too);
    /// `done` gets the result on the main thread. Nothing runs if the read panicked.
    fn read_rules_snapshot<T: Send + 'static>(
        &self,
        path: std::path::PathBuf,
        read: impl FnOnce(RulesRead) -> T + Send + 'static,
        done: impl FnOnce(T) + 'static,
    ) {
        let sources = self.permission_sources();
        let codex_inputs = self.codex_inputs();
        glib::spawn_future_local(async move {
            let codex = codex_target(codex_inputs).await;
            let computed = gtk4::gio::spawn_blocking(move || {
                // An unusable file is named, never shown as "0 rules".
                let (rules, blocked) = open_rules_for_display(&path);
                read(RulesRead {
                    path,
                    rules,
                    blocked,
                    sources,
                    codex,
                })
            })
            .await;
            if let Ok(value) = computed {
                done(value);
            }
        });
    }

    /// Wires an Import or Export button: once the click stands (an export is confirmed first,
    /// with `export_text`), it says "Syncing…" and runs the job a moment later; the button and
    /// the sync indicator (`on_sync`) follow the finished job.
    fn connect_sync_button(
        &self,
        button: &Button,
        sync: RulesSync,
        export_text: Rc<RefCell<String>>,
        on_sync: Rc<dyn Fn()>,
    ) {
        let obj = self.obj();
        let button_weak = button.downgrade();
        button.connect_clicked(glib::clone!(
            #[weak]
            obj,
            move |clicked| {
                let Some(btn) = button_weak.upgrade() else {
                    return;
                };
                if matches!(sync, RulesSync::Export)
                    && !confirm_export_first(clicked, &export_text.borrow())
                {
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
                    // The button and the sync indicator follow the finished job.
                    let on_sync = on_sync.clone();
                    let done = move || mark_synced(&btn_weak, move || on_sync());
                    match crate::always_allow::path() {
                        Some(path) => imp.run_rules_sync(&path, sync, done),
                        None => done(),
                    }
                });
            }
        ));
    }

    /// What resolving Codex's folder needs from the config: the Codex profile (its command and
    /// env file) and `clear_env`.
    fn codex_inputs(&self) -> (Profile, Vec<String>) {
        let config = self.config.borrow();
        (
            super::threads::agent_profile_or_new(&config, Driver::Codex),
            config.clear_env.clone(),
        )
    }

    /// Runs a rules-store `job` on a worker thread (it may wait for the store lock) and
    /// `finish` on the main thread with its result (`None`: the worker failed). Nothing that
    /// depends on the job's outcome (a toast, the sync indicator) runs before `finish`.
    fn rules_job<T: Send + 'static>(
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
            let result = blocking_or(
                move || crate::always_allow::AlwaysRules::forget(&path, &rule),
                Err(crate::always_allow::StoreError::Write(
                    "the removal did not finish".to_owned(),
                )),
            )
            .await;
            done(result);
        });
    }

    /// Applies `edit` to `driver`'s profile (created when missing, and recorded as `driver`'s so
    /// a command edit never detaches it) and schedules the save.
    fn edit_agent_profile(&self, driver: Driver, edit: impl FnOnce(&mut Profile)) {
        let profile_id = {
            let mut config = self.config.borrow_mut();
            let index = config.agent_profile_slot(driver);
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
        let name = if MODES.contains(&mode) {
            mode.label()
        } else {
            "This mode"
        };
        self.action_toast(
            &format!("{name} for this thread"),
            &format!("Make Default for {}", driver.info().long_label),
            8,
            adw::ToastPriority::Normal,
            move |imp| {
                // Ask is the built-in default, stored as none (as the Settings row does).
                imp.edit_agent_profile(driver, |p| {
                    p.default_mode = Some(mode).filter(|m| *m != Mode::Ask);
                });
                imp.show_toast(&format!(
                    "New {} threads start in {name}",
                    driver.info().long_label
                ));
            },
        );
    }

    fn agent_profile_now(&self, driver: Driver) -> Profile {
        super::threads::agent_profile_or_new(&self.config.borrow(), driver)
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
        let (delegation, switches) = self.delegation_group(dialog);
        page.add(&delegation);

        page.add(&self.permissions_group());

        for driver in Driver::ALL {
            page.add(&self.agent_group(driver, dialog, &switches));
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

    /// A dropdown over a fixed list of (value, label) `choices`, by index both ways: it starts at
    /// `current` (else the first) and hands `changed` the chosen value (none past the end).
    pub(super) fn choice_row<T: Copy + PartialEq + 'static>(
        &self,
        title: &str,
        subtitle: &str,
        choices: Vec<(T, String)>,
        current: T,
        changed: impl Fn(&Self, Option<T>) + 'static,
    ) -> adw::ComboRow {
        let obj = self.obj();
        let labels: Vec<&str> = choices.iter().map(|(_, n)| n.as_str()).collect();
        let row = adw::ComboRow::builder()
            .title(title)
            .subtitle(subtitle)
            .model(&gtk4::StringList::new(&labels))
            .selected(choices.iter().position(|(v, _)| *v == current).unwrap_or(0) as u32)
            .build();
        row.connect_selected_notify(glib::clone!(
            #[weak]
            obj,
            move |row| {
                let choice = choices.get(row.selected() as usize).map(|(v, _)| *v);
                changed(obj.imp(), choice);
            }
        ));
        row
    }

    /// The "Default Agent for New Threads" dropdown. Applies on change.
    pub(super) fn default_agent_row(&self) -> adw::ComboRow {
        let current = self.config.borrow().default_agent;
        let default_row = self.choice_row(
            "Default Agent for New Threads",
            "Automatic: the default profile's agent, else the first installed",
            default_agent_choices()
                .into_iter()
                .map(|(d, n)| (d, n.to_owned()))
                .collect(),
            current,
            |imp, choice| {
                let choice = choice.flatten();
                if imp.config.borrow().default_agent != choice {
                    imp.config.borrow_mut().default_agent = choice;
                    imp.schedule_config_save();
                }
            },
        );
        Self::keep_value_whole(&default_row);
        default_row
    }

    /// Writes an approval switch now, and says so if the settings could not be saved.
    pub(super) fn save_approval_switch(&self) {
        if !self.save_config_now() {
            self.show_toast("Your settings could not be saved. The change lasts until you quit.");
        }
    }

    /// "Automatic Pattern Approvals": whether saved Always-allow rules approve matching requests.
    /// Shared by Settings → Agents and setup; applies on change.
    pub(super) fn pattern_approval_row(&self) -> adw::SwitchRow {
        // The config is shared by every window, so this is the one copy. A security switch is
        // written at once, not debounced, so a stale read or a crash in the debounce cannot
        // bring it back on.
        self.config_switch(
            "Automatic Pattern Approvals",
            "Saved Always-allow rules approve matching requests without asking. Off: every \
             request asks, except what you allow for this session. Rules exported to an \
             agent's own settings are that agent's to apply.",
            |c| c.pattern_auto_approval,
            SaveWhen::Now(|c, on| c.pattern_auto_approval = on),
            // Process-wide, last writer wins; the settings file monitor keeps windows in step.
            |_, on| crate::always_allow::set_saved_rules_enabled(on),
        )
    }

    /// "Confirm and Customise Patterns on Save", shared like [`Self::pattern_approval_row`].
    pub(super) fn confirm_rule_row(&self) -> adw::SwitchRow {
        self.config_switch(
            "Confirm and Customise Patterns on Save",
            "Prompt with an interactive dialogue to loosen or tighten patterns when choosing 'Always allow'",
            |c| c.confirm_rule_modification,
            SaveWhen::Now(|c, on| c.confirm_rule_modification = on),
            // Process-wide, last writer wins; the settings file monitor keeps windows in step.
            |_, on| crate::always_allow::set_confirm_rule_modification(on),
        )
    }

    /// The "In-App Approvals and Pattern Matching" preferences group.
    pub(super) fn permissions_group(&self) -> adw::PreferencesGroup {
        let group = adw::PreferencesGroup::builder()
            .title("In-App Approvals and Pattern Matching")
            .description(APPROVAL_GROUP_HELP)
            .build();

        group.add(&self.pattern_approval_row());
        group.add(&self.confirm_rule_row());
        let obj = self.obj();

        // Extra permission files to import from and export to; none unless the user names them.
        let (sources_row, sources_error) = validated_entry(
            "Permission Sources",
            PERMISSION_SOURCES_HELP,
            &self.config.borrow().permission_sources.join(":"),
        );
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
        let view_diff_btn = pill_button("View Differences…");
        warning_row.add_suffix(&view_diff_btn);
        group.add(&warning_row);

        // Import permissions row
        let import_row = adw::ActionRow::builder()
            .title("Import Agent Permissions")
            .subtitle("One-way import: pull allowlisted commands and tools from Claude, Antigravity, and permissions.toml")
            .build();
        let import_btn = pill_button("Import Now");

        // Export permissions row
        let export_row = adw::ActionRow::builder()
            .title("Export Agent Permissions")
            .subtitle("One-way export: push Agent Terminal's saved rules to permissions.toml and agent configurations")
            .build();
        let export_btn = pill_button("Export Now");

        // Manage rules row. The rules file is read off the main thread: the row says so until
        // the count is in, and the full check below (`sync_cb`) has the last word.
        let rule_count = |count: usize| {
            format!(
                "{count} active rules stored in {}",
                crate::always_allow::FILE_NAME
            )
        };
        let reading = format!(
            "Reading the rules stored in {}…",
            crate::always_allow::FILE_NAME
        );
        let rules_path = crate::always_allow::path();
        let manage_row = adw::ActionRow::builder()
            .title("Saved Permission Rules")
            .subtitle(if rules_path.is_some() {
                reading.clone()
            } else {
                rule_count(0)
            })
            .build();
        if let Some(p) = rules_path {
            let row = manage_row.downgrade();
            glib::spawn_future_local(async move {
                let count = blocking_or(
                    move || crate::always_allow::AlwaysRules::load(&p).rules.len(),
                    0,
                )
                .await;
                if let Some(row) = row
                    .upgrade()
                    .filter(|r| r.subtitle().as_deref() == Some(reading.as_str()))
                {
                    row.set_subtitle(&rule_count(count));
                }
            });
        }
        let manage_btn = pill_button("Manage Rules…");

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
        let sync_generation = Rc::new(crate::window::Generation::new());
        let sync_cb: Rc<dyn Fn()> = Rc::new(glib::clone!(
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
                let current = sync_generation.next();
                let sync_generation = sync_generation.clone();
                let export_text = export_text.clone();
                obj.imp().read_rules_snapshot(
                    path,
                    |read| {
                        let target = read.codex.as_ref().ok();
                        let status = read.rules.check_sync_status(None, &read.sources, target);
                        let available = target.is_some_and(|t| t.available());
                        let warning = read.rules.export_warning_for(target);
                        let mut described = match &read.codex {
                            Ok(t) => t.describe(),
                            Err(reason) => format!("Codex's rules folder is unknown: {reason}"),
                        };
                        if let Some(reason) = &status.codex_refused {
                            described.push_str(&format!(
                                ". Codex's own rules are not imported: {reason}"
                            ));
                        }
                        (
                            read.rules.rules.len(),
                            read.blocked,
                            status,
                            available,
                            warning,
                            described,
                        )
                    },
                    glib::clone!(
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
                        move |computed: SyncRead| {
                            let (count, blocked, status, available, warning, described) = computed;
                            if !sync_generation.is_current(current) {
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
                    ),
                );
            }
        ));

        // Initial sync state check
        sync_cb();

        // Applied once valid, ~400 ms after typing stops (the file checks run off the main
        // thread) or at once when focus leaves. An invalid draft stays in the entry with its
        // error shown inline, and the previously saved list stays in use; nothing reverts.
        let on_sources = sync_cb.clone();
        let typing = Rc::new(crate::window::Generation::new());
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
                let current = typing.next();
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
                            if !typing.is_current(current) {
                                return;
                            }
                        }
                        let checked = gtk4::gio::spawn_blocking(move || {
                            crate::config::parse_permission_sources(&text)
                        })
                        .await;
                        let Ok(checked) = checked else { return };
                        if !typing.is_current(current) {
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

        self.connect_sync_button(
            &import_btn,
            RulesSync::Import,
            export_text.clone(),
            sync_cb.clone(),
        );
        import_row.add_suffix(&import_btn);
        group.add(&import_row);

        self.connect_sync_button(
            &export_btn,
            RulesSync::Export,
            export_text.clone(),
            sync_cb.clone(),
        );
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
        let (import_state, export_state) = sync_button_states(status);
        if status.needs_attention() {
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
            } else if status.is_unknown() {
                import_row.add_css_class("warning");
                import_row.set_subtitle(&format!(
                    "Codex's rules could not be read, so nothing was checked for import from {}",
                    source_names(codex)
                ));
            } else {
                import_row.remove_css_class("warning");
                import_row.set_subtitle(&import_idle);
            }
            import_btn.set_sensitive(import_state.sensitive);
            import_btn.set_label(import_state.label);

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
            } else {
                export_row.remove_css_class("warning");
                export_row.set_subtitle(export_idle);
            }
            export_btn.set_sensitive(export_state.sensitive);
            export_btn.set_label(export_state.label);
        } else {
            warning_row.set_visible(false);
            import_row.remove_css_class("warning");
            import_row.set_subtitle(&import_idle);
            import_btn.set_sensitive(import_state.sensitive);
            import_btn.set_label(import_state.label);

            export_row.remove_css_class("warning");
            export_row.set_subtitle(export_idle);
            export_btn.set_sensitive(export_state.sensitive);
            export_btn.set_label(export_state.label);
        }
    }

    /// Lists the differences between the rules file and every agent's own (Codex's folder
    /// included when it resolves), read off the main thread.
    fn show_sync_differences_dialog(&self, parent: &gtk4::Widget, _on_sync: Rc<dyn Fn()>) {
        let Some(path) = crate::always_allow::path() else {
            return;
        };
        let parent = parent.downgrade();
        self.read_rules_snapshot(
            path,
            |read| {
                let codex = read.codex.as_ref().ok();
                let available = codex.is_some_and(|t| t.available());
                (
                    read.rules.check_sync_status(None, &read.sources, codex),
                    read.blocked,
                    available,
                )
            },
            move |(status, blocked, codex)| {
                let Some(parent) = parent.upgrade() else {
                    return;
                };
                Self::present_sync_differences(&parent, &status, blocked, codex);
            },
        );
    }

    /// The differences dialog. A rules file that cannot be used is named in its body, and the
    /// rules are then never called synchronised.
    fn present_sync_differences(
        parent: &gtk4::Widget,
        status: &crate::always_allow::PermissionSyncStatus,
        blocked: Option<String>,
        codex: bool,
    ) {
        let content = gtk4::Box::new(gtk4::Orientation::Vertical, 16);
        content.set_margin_top(12);
        content.set_margin_bottom(12);
        content.set_margin_start(16);
        content.set_margin_end(16);

        differences_section(
            &content,
            &format!(
                "Unimported Rules from Agent Configs ({})",
                status.unimported_rules.len()
            ),
            &format!(
                "These rules exist in {} but are missing in Agent Terminal:",
                source_names(codex)
            ),
            &status.unimported_rules,
            "at-go-down-symbolic",
        );
        differences_section(
            &content,
            &format!(
                "Unexported Rules from Agent Terminal ({})",
                status.unexported_rules.len()
            ),
            &format!("What the next export changes in {}:", source_names(codex)),
            &status.unexported_rules,
            "at-go-up-symbolic",
        );

        if blocked.is_none()
            && status.unimported_rules.is_empty()
            && status.unexported_rules.is_empty()
        {
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

        let body = blocked.unwrap_or_else(|| status.summary());
        let dialog =
            adw::AlertDialog::new(Some("Permission Synchronization Differences"), Some(&body));
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
        let parent = parent.downgrade();
        let obj = self.obj().downgrade();
        self.read_rules_snapshot(
            path,
            |read| {
                let codex = read.codex.as_ref().ok();
                let status = read.rules.check_sync_status(None, &read.sources, codex);
                let warning = read.rules.export_warning_for(codex);
                (
                    read.path,
                    RulesView {
                        rules: read.rules,
                        blocked: read.blocked,
                        status,
                        warning,
                    },
                )
            },
            move |(path, view)| {
                let (Some(parent), Some(obj)) = (parent.upgrade(), obj.upgrade()) else {
                    return;
                };
                obj.imp()
                    .present_rules_dialog(&parent, path, view, on_changed);
            },
        );
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
        } else if status.needs_attention() {
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
                        // Refreshed once the import has finished.
                        imp.run_rules_sync(&p, RulesSync::Import, move || {
                            if let Some(f) = on_changed {
                                f();
                            }
                        });
                    }
                    "export" => {
                        let confirm = export_confirmation(&warning);
                        if confirm.choose_future(root.as_ref()).await == "export" {
                            // Refreshed once the export has finished.
                            imp.run_rules_sync(&p, RulesSync::Export, move || {
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
        switches: &Rc<delegation::Switches>,
    ) -> adw::PreferencesGroup {
        let profile = self.agent_profile_now(driver);
        let group = adw::PreferencesGroup::builder()
            .title(driver.info().long_label)
            .description(format!("Profile “{}”", profile.name))
            .build();
        let (enabled, detected) = self.agent_enabled_row_linked(driver, dialog.upcast_ref());
        group.add(&enabled);
        group.add(&self.agent_account_row(driver, dialog.upcast_ref()));
        let subtitle = if driver == Driver::Agy {
            "This agent only (the switch under Cross-agent delegation sets every agent). Turning it on reviews Antigravity's managed registration; turning it off offers to remove it."
        } else {
            "This agent only (the switch under Cross-agent delegation sets every agent)."
        };
        let delegation = adw::SwitchRow::builder()
            .title("Available for delegation")
            .subtitle(subtitle)
            .active(profile.available_for_delegation)
            .build();
        switches.add_agent(driver, &delegation);
        let obj = self.obj();
        let switches = switches.clone();
        delegation.connect_active_notify(glib::clone!(
            #[weak]
            obj,
            move |row| {
                if !switches.quiet() {
                    obj.imp()
                        .set_agent_delegation(driver, row.is_active(), &switches);
                }
            }
        ));
        group.add(&delegation);
        self.add_agent_detail_rows_linked(&group, driver, &profile, dialog, Some(detected));
        group
    }

    /// The top of the setup's agents page: who is ready to run a thread (installed, enabled and
    /// not known to be signed out), and Check Again to scan once more. Follows detection and the
    /// account checks until `dialog` closes.
    pub(super) fn agents_ready_group(&self, dialog: &adw::Dialog) -> adw::PreferencesGroup {
        let obj = self.obj();
        let summary = adw::ActionRow::builder()
            .title("Ready to Use")
            .use_markup(false)
            .build();
        let check = gtk4::Button::builder()
            .label("Check Again")
            .valign(gtk4::Align::Center)
            .tooltip_text("Look for the agents again, for one you just installed")
            .build();
        check.connect_clicked(glib::clone!(
            #[weak]
            obj,
            move |_| {
                // As check_sign_in does: a signed-out agy is probed again only when asked to.
                let account = crate::account_status::AccountStatus::shared();
                for driver in Driver::ALL {
                    if AgentAvailability::shared().is_ready(driver) {
                        account.recheck(driver);
                    }
                }
                obj.imp().refresh_agent_data(super::threads::Probes::Force);
            }
        ));
        summary.add_suffix(&check);
        let show = glib::clone!(
            #[weak]
            summary,
            move || {
                let availability = AgentAvailability::shared();
                let account = crate::account_status::AccountStatus::shared();
                // Ready needs positive evidence of a sign-in: an account the probe returned. An
                // installed agent whose probe is still out leaves the answer open.
                let mut ready: Vec<&str> = Vec::new();
                let mut checking = availability.any_detecting();
                for driver in Driver::ALL {
                    if !availability.is_ready(driver) {
                        continue;
                    }
                    let snap = account.snapshot(driver);
                    if snap.account.is_some() && !snap.signed_out {
                        ready.push(driver.info().label);
                    } else if !snap.signed_out && (snap.probing || account.is_rechecking(driver)) {
                        checking = true;
                    }
                }
                let (text, error) = ready_summary(&ready, checking);
                summary.set_subtitle(&text);
                if error {
                    summary.add_css_class("warning");
                } else {
                    summary.remove_css_class("warning");
                }
            }
        );
        show();
        let ready_id = AgentAvailability::shared().connect_changed(show.clone());
        let account_id = crate::account_status::AccountStatus::shared().connect_changed(show);
        dialog.connect_closed(move |_| {
            AgentAvailability::shared().disconnect(ready_id);
            crate::account_status::AccountStatus::shared().disconnect(account_id);
        });
        let group = adw::PreferencesGroup::new();
        group.add(&summary);
        group
    }

    /// `driver`'s on/off switch, saying what detection found (ready and where, missing and how to
    /// install, or off). Follows the scan until `dialog` closes.
    pub(super) fn agent_enabled_row(&self, driver: Driver, dialog: &adw::Dialog) -> adw::SwitchRow {
        self.agent_enabled_row_linked(driver, dialog).0
    }

    /// [`Self::agent_enabled_row`], with what the Detected row found (set it, then call the
    /// returned refresh) so the two never disagree.
    fn agent_enabled_row_linked(
        &self,
        driver: Driver,
        dialog: &adw::Dialog,
    ) -> (adw::SwitchRow, Rc<DetectedLink>) {
        let obj = self.obj();
        let link = Rc::new(DetectedLink::default());
        let profile = self.agent_profile_now(driver);
        let enabled = adw::SwitchRow::builder()
            .title("Enabled")
            .subtitle(AgentAvailability::shared().get(driver).describe(driver))
            .active(!profile.disabled)
            .build();
        let brand = gtk4::Image::from_gicon(&crate::icons::driver_icon(driver));
        brand.set_pixel_size(20);
        enabled.add_prefix(&brand);
        // A link to the agent's install page, shown only while detection found nothing.
        let get_it = gtk4::LinkButton::builder()
            .uri(driver.info().install_url)
            .label("Get It")
            .valign(gtk4::Align::Center)
            .tooltip_text(driver.info().install_url)
            .visible(AgentAvailability::shared().get(driver) == Availability::Missing)
            .build();
        enabled.add_suffix(&get_it);
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
        // The row says what detection found: ready (where), missing (how to install), or off;
        // the Detected row's own check of the saved command wins when the two disagree.
        let show: Rc<dyn Fn()> = Rc::new(glib::clone!(
            #[weak]
            enabled,
            #[weak]
            get_it,
            #[strong]
            link,
            move || {
                let state = AgentAvailability::shared().get(driver);
                let detected = link.detected.borrow();
                let text = enabled_subtitle(driver, &state, detected.as_ref());
                get_it.set_visible(text == Availability::Missing.describe(driver));
                enabled.set_subtitle(&text);
            }
        ));
        *link.refresh.borrow_mut() = Some(Rc::downgrade(&show));
        show();
        let state_id = AgentAvailability::shared().connect_changed({
            let show = show.clone();
            move || show()
        });
        dialog.connect_closed(move |_| AgentAvailability::shared().disconnect(state_id));
        // The availability handler keeps `show` (and with it the link) alive with the dialog.
        (enabled, link)
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
                let started = obj.imp().sign_in_was_started(driver);
                // After Sign In the button turns into Check Again: say so where it can be read.
                let text = if signed_out && started {
                    "Not signed in. Finish signing in in the terminal tab that opened, then \
                     choose Check Again."
                        .to_owned()
                } else {
                    text
                };
                account.set_subtitle(&text);
                sign_in.set_visible(signed_out);
                // The same two steps as a thread's banner: open the sign-in, then check.
                sign_in.set_label(if started { "Check Again" } else { "Sign In…" });
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
    /// A debounce for one agent field's edits, flushed (and the config written) when `dialog`
    /// closes, and by [`Self::flush_agent_edits`] while it lives.
    fn agent_edit_timer(&self, dialog: &adw::PreferencesDialog) -> Rc<EditDebounce> {
        let obj = self.obj();
        let timer = Rc::new(EditDebounce::default());
        self.agent_edits.borrow_mut().push(Rc::downgrade(&timer));
        dialog.connect_closed(glib::clone!(
            #[strong]
            timer,
            #[weak]
            obj,
            move |_| {
                timer.flush();
                obj.imp().flush_pending_save();
            }
        ));
        timer
    }

    /// The detail rows alone, with no Enabled row to tell: for tests.
    #[cfg(test)]
    fn add_agent_detail_rows(
        &self,
        group: &adw::PreferencesGroup,
        driver: Driver,
        profile: &Profile,
        dialog: &adw::PreferencesDialog,
    ) {
        self.add_agent_detail_rows_linked(group, driver, profile, dialog, None);
    }

    /// [`Self::add_agent_detail_rows`], telling the agent's Enabled row (through `enabled`)
    /// what the Detected row finds.
    fn add_agent_detail_rows_linked(
        &self,
        group: &adw::PreferencesGroup,
        driver: Driver,
        profile: &Profile,
        dialog: &adw::PreferencesDialog,
        enabled: Option<Rc<DetectedLink>>,
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
        let (command, command_error) =
            validated_entry("Command or Path", COMMAND_HELP, &profile.command);
        let generation = Rc::new(crate::window::Generation::new());
        let detect = Rc::new(glib::clone!(
            #[weak]
            status,
            #[strong]
            generation,
            move |command: String| {
                let current = generation.next();
                status.set_subtitle("Checking…");
                if let Some(link) = &enabled {
                    link.set(None);
                }
                let generation = generation.clone();
                let enabled = enabled.clone();
                glib::MainContext::default().spawn_local(async move {
                    let found = off_main("the check", move || detect_agent(&command)).await;
                    if !generation.is_current(current) {
                        return;
                    }
                    if let Some(link) = &enabled {
                        // A check that failed to run (it panicked) found nothing either way.
                        let answer = found.as_ref().map_or_else(
                            |e| e.starts_with("Not found").then(|| found.clone()),
                            |_| Some(found.clone()),
                        );
                        link.set(answer);
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
        let command_timer = self.agent_edit_timer(dialog);
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

        let (args, args_error) = validated_entry(
            "Extra Arguments",
            ARGS_HELP,
            &profile
                .args
                .iter()
                .map(|a| crate::utils::shell_quote(a))
                .collect::<Vec<_>>()
                .join(" "),
        );
        // Validated on every change, saved only on commit (Enter, the apply button, focus
        // leaving the row, or the dialog or window closing): a half-typed argument list is
        // never written, and never restarts the agent.
        args.set_show_apply_button(true);
        let args_timer = self.agent_edit_timer(dialog);
        let commit_args = Rc::new(glib::clone!(
            #[weak]
            obj,
            #[weak]
            args,
            #[weak]
            args_error,
            move || {
                let imp = obj.imp();
                let saved = imp.agent_profile_now(driver).args;
                match crate::config::field_change(parse_args(&args.text()), &saved, true) {
                    FieldChange::Invalid(reason) => {
                        show_validation(&args, &args_error, ARGS_HELP, Some(&reason));
                    }
                    FieldChange::Keep => {}
                    FieldChange::Persist(parsed) => {
                        imp.edit_agent_profile(driver, |p| p.args = parsed);
                    }
                }
            }
        ));
        args.connect_changed(glib::clone!(
            #[weak]
            obj,
            #[weak]
            args_error,
            #[strong]
            args_timer,
            #[strong]
            commit_args,
            move |row| {
                let saved = obj.imp().agent_profile_now(driver).args;
                match crate::config::field_change(parse_args(&row.text()), &saved, false) {
                    FieldChange::Invalid(reason) => {
                        args_timer.cancel();
                        show_validation(row, &args_error, ARGS_HELP, Some(&reason));
                    }
                    FieldChange::Keep | FieldChange::Persist(_) => {
                        show_validation(row, &args_error, ARGS_HELP, None);
                        let commit = commit_args.clone();
                        args_timer.hold(move || commit());
                    }
                }
            }
        ));
        let commit_now = glib::clone!(
            #[strong]
            args_timer,
            move || args_timer.flush()
        );
        args.connect_apply({
            let commit_now = commit_now.clone();
            move |_| commit_now()
        });
        args.connect_entry_activated({
            let commit_now = commit_now.clone();
            move |_| commit_now()
        });
        let focus = gtk4::EventControllerFocus::new();
        focus.connect_leave(move |_| commit_now());
        args.add_controller(focus);
        group.add(&args);
        group.add(&args_error);

        let (env_file, env_error) = validated_entry(
            "Environment File",
            ENV_FILE_HELP,
            profile.env_file.as_deref().unwrap_or(""),
        );
        let env_timer = self.agent_edit_timer(dialog);
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

        let mode_row = self.choice_row(
            "Default Mode",
            if driver == Driver::Agy {
                "New threads start here; a thread keeps its own. Without the approval hook, \
                 Ask before edits runs as Plan"
            } else {
                "New threads start here; a thread keeps its own"
            },
            MODES.iter().map(|m| (*m, m.label().to_owned())).collect(),
            profile.default_mode.unwrap_or_default(),
            move |imp, mode| {
                imp.edit_agent_profile(driver, |p| {
                    p.default_mode = mode.filter(|m| *m != Mode::Ask);
                });
            },
        );
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
            group.add(&always_allowed_rows());
        }
    }

    /// agy's approval hook: whether hooks.json installs it, the entry to add, and a Copy button.
    /// Read-only towards `~/.gemini`: the app never writes the hook file. Shared by Settings →
    /// Agents and setup.
    pub(super) fn add_hook_rows(&self, group: &adw::PreferencesGroup) {
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

        glib::MainContext::default().spawn_local(glib::clone!(
            #[weak]
            hook,
            async move {
                // Settings checks the home-level installation, never a thread's launch proof.
                let target = super::threads::agy_hook_target(None).await;
                let (probe, bound) = blocking_or(
                    move || {
                        (
                            crate::hook_config::probe_settings(&target),
                            crate::approval_server::socket_bindable(),
                        )
                    },
                    (
                        crate::hook_config::HookProbe::Unreadable(
                            "the hooks check panicked".into(),
                        ),
                        false,
                    ),
                )
                .await;
                let badge = crate::hook_config::badge(&probe, bound);
                crate::hook_config::log_verdict(badge, &probe, bound);
                let (text, class) = match badge {
                    crate::hook_config::HookBadge::Ready => (
                        "Ready: Hook entry present and runtime directory available.",
                        None,
                    ),
                    crate::hook_config::HookBadge::Missing => (
                        "Not installed: Antigravity cannot ask before acting until the entry below is added.",
                        Some("error"),
                    ),
                    crate::hook_config::HookBadge::Unreadable => (
                        "Could not verify the approval hook for a thread. Treat Antigravity as ungated.",
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

/// One side of the differences dialog: a heading, what it lists, and one row per rule, each
/// with `icon`. Nothing when there are no rules.
fn differences_section(
    content: &gtk4::Box,
    heading: &str,
    description: &str,
    rules: &[String],
    icon: &str,
) {
    if rules.is_empty() {
        return;
    }
    let label = gtk4::Label::new(Some(heading));
    label.add_css_class("heading");
    label.set_halign(gtk4::Align::Start);
    content.append(&label);

    let desc = gtk4::Label::new(Some(description));
    desc.set_wrap(true);
    desc.set_halign(gtk4::Align::Start);
    desc.add_css_class("dim-label");
    content.append(&desc);

    let list = gtk4::ListBox::new();
    list.add_css_class("boxed-list");
    for r in rules {
        let row = adw::ActionRow::builder().use_markup(false).title(r).build();
        row.add_prefix(&gtk4::Image::from_icon_name(icon));
        list.append(&row);
    }
    content.append(&list);
}

/// Shows a sync button as done ("✓ Synced", then "Synced" a second later) and then runs
/// `refresh` (the sync indicator). Called from the background job's completion.
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

/// The question an export asks first: `warning` ([`crate::always_allow::export_warning`]),
/// answered `export` or `cancel`.
fn export_confirmation(warning: &str) -> adw::AlertDialog {
    confirm_dialog(
        "Export rules outside agent-terminal?",
        warning,
        ("cancel", "Cancel"),
        ("export", "Export"),
        adw::ResponseAppearance::Destructive,
        false,
    )
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
    let dialog = export_confirmation(warning);
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

/// The rules file for display: its rules, and why it cannot be used, if it cannot (an unusable
/// file is named, never shown as no rules). Blocking: run it off the main thread.
fn open_rules_for_display(
    path: &std::path::Path,
) -> (crate::always_allow::AlwaysRules, Option<String>) {
    match crate::always_allow::AlwaysRules::open(path) {
        Ok(rules) => (rules, None),
        Err(e) => (
            Default::default(),
            Some(crate::always_allow::blocked_notice(path, &e)),
        ),
    }
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

/// The Enabled row's subtitle: the shared scan's `state`, unless the Detected row's own check
/// of the saved command (`detected`, `None` while it runs) disagrees with it. The two rows
/// never contradict each other.
fn enabled_subtitle(
    driver: Driver,
    state: &Availability,
    detected: Option<&Result<String, String>>,
) -> String {
    match (state, detected) {
        (Availability::Disabled, _) | (_, None) => state.describe(driver),
        (Availability::Ready(_), Some(Err(_))) => Availability::Missing.describe(driver),
        (Availability::Missing | Availability::Detecting, Some(Ok(found))) => {
            format!("Ready: {}", found.trim_start_matches("Installed at "))
        }
        _ => state.describe(driver),
    }
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

    /// The first `T` among `widgets` titled `title`.
    fn find_titled<T: IsA<adw::PreferencesRow> + IsA<gtk4::Widget>>(
        widgets: &[gtk4::Widget],
        title: &str,
    ) -> T {
        widgets
            .iter()
            .find_map(|w| {
                w.downcast_ref::<T>()
                    .filter(|r| r.title() == title)
                    .cloned()
            })
            .unwrap_or_else(|| panic!("no row titled {title}"))
    }

    #[test]
    fn sync_buttons_never_claim_synced_for_unread_codex_rules() {
        use crate::always_allow::PermissionSyncStatus;
        let label = |s: &PermissionSyncStatus| {
            let (i, e) = sync_button_states(s);
            (i.label, i.sensitive, e.label, e.sensitive)
        };
        assert_eq!(
            label(&PermissionSyncStatus::default()),
            ("Synced", false, "Synced", false)
        );
        let refused = PermissionSyncStatus {
            codex_refused: Some("unreadable".to_owned()),
            ..Default::default()
        };
        assert_eq!(label(&refused), ("Not Checked", false, "Synced", false));
        let counted = PermissionSyncStatus {
            unimported_count: 1,
            unexported_count: 2,
            ..refused
        };
        assert_eq!(label(&counted), ("Import Now", true, "Export Now", true));
    }

    #[test]
    fn ready_summary_names_the_ready_agents_or_warns() {
        assert_eq!(
            ready_summary(&["Claude", "Codex"], false),
            ("Ready: Claude, Codex".to_owned(), false)
        );
        assert_eq!(
            ready_summary(&["Claude"], true).0,
            "Ready: Claude. Still checking others…"
        );
        let (text, warn) = ready_summary(&[], true);
        assert!(!warn, "checking is not a verdict");
        assert_eq!(text, "Looking for agents…");
        let (text, warn) = ready_summary(&[], false);
        assert!(warn && text.starts_with("No agent is ready yet"), "{text}");
    }

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
    fn the_enabled_row_never_says_ready_when_detected_says_not_found() {
        let driver = Driver::Claude;
        let ready = Availability::Ready("/usr/bin/claude".into());
        let not_found: Result<String, String> = Err("Not found: /nonexistent/claude".into());
        // The scan still holds the old command's result; Detected checked the saved one.
        let text = enabled_subtitle(driver, &ready, Some(&not_found));
        assert!(!text.starts_with("Ready"), "{text}");
        assert!(text.starts_with("Not found"), "{text}");
        // Detected found it while the scan says missing: not "Not found" either.
        let found: Result<String, String> = Ok("Installed at /opt/claude · 2.1".into());
        let text = enabled_subtitle(driver, &Availability::Missing, Some(&found));
        assert!(!text.starts_with("Not found"), "{text}");
        assert!(text.contains("/opt/claude"), "{text}");
        // Agreement, a check still running, or an agent turned off: the scan's own words.
        assert_eq!(
            enabled_subtitle(driver, &ready, Some(&found)),
            ready.describe(driver)
        );
        assert_eq!(enabled_subtitle(driver, &ready, None), ready.describe(driver));
        assert_eq!(
            enabled_subtitle(driver, &Availability::Disabled, Some(&not_found)),
            Availability::Disabled.describe(driver)
        );
    }

    #[test]
    fn a_held_commit_runs_only_on_flush_and_an_invalid_edit_drops_it() {
        let commits = Rc::new(std::cell::Cell::new(0));
        let timer = EditDebounce::default();
        let seen = commits.clone();
        timer.hold(move || seen.set(seen.get() + 1));
        assert_eq!(commits.get(), 0, "a draft is not saved while typing");
        timer.flush(); // Enter, focus leaving, or the dialog or window closing
        assert_eq!(commits.get(), 1);
        timer.flush();
        assert_eq!(commits.get(), 1, "one commit per draft");
        let seen = commits.clone();
        timer.hold(move || seen.set(seen.get() + 1));
        timer.cancel(); // the text turned invalid
        drop(timer);
        assert_eq!(commits.get(), 1);
    }

    #[test]
    #[ignore = "needs a display; run alone in an isolated display"]
    fn closing_settings_and_window_flushes_agent_edits() {
        gtk4::init().unwrap();
        adw::init().unwrap();
        let app = crate::window::imp::test_support::test_app("org.test.SettingsFlush", false);
        let env_file = app.config_dir().join("agent.env");
        std::fs::write(&env_file, "TEST_FLAG=yes\n").unwrap();
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
        let field = |title: &str| find_titled::<adw::EntryRow>(&widgets, title);
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
        let app = crate::window::imp::test_support::test_app("org.test.InvalidDetection", false);
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
        let command = find_titled::<adw::EntryRow>(&widgets, "Command or Path");
        let status = find_titled::<adw::ActionRow>(&widgets, "Detected");
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
        let app = crate::window::imp::test_support::test_app("org.test.AgentDebounce", false);
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
        let field = find_titled::<adw::EntryRow>(&widgets, "Command or Path");
        let status = find_titled::<adw::ActionRow>(&widgets, "Detected");
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
        let mut fields = Vec::new();
        let mut feedback = Vec::new();
        for widget in crate::testutil::descendants(group.upcast_ref()) {
            if let Some(row) = widget.downcast_ref::<adw::EntryRow>() {
                fields.push(row.clone());
            }
            if let Some(row) = widget.downcast_ref::<adw::ActionRow>() {
                if row.title() == "Not Saved" {
                    feedback.push(row.clone());
                }
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
    /// valid markup: a bare `&` makes the parse fail and the title renders blank.
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
    /// subtitle (for example, "Agent T…" or "Auto…").
    #[test]
    #[ignore = "native GTK layout; run alone in an isolated display"]
    fn native_dropdown_rows_show_their_whole_value() {
        gtk4::init().expect("GTK must be available for the native gate");
        adw::init().expect("libadwaita init");
        crate::load_css();
        let ctx = glib::MainContext::default();
        let app = crate::window::imp::test_support::test_app("org.test.Dropdowns", false);
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
    /// The master switch turns every enabled agent's delegation on and off, and their own
    /// switches follow it. Antigravity is disabled here, so no registration review opens.
    pub(crate) fn the_master_switch_sets_every_enabled_agent(imp: &AgentTerminalWindow) {
        let original = imp.config.borrow().profiles.clone();
        imp.edit_agent_profile(Driver::Agy, |p| p.disabled = true);
        for d in [Driver::Claude, Driver::Codex] {
            imp.edit_agent_profile(d, |p| p.available_for_delegation = false);
        }
        let dialog = adw::PreferencesDialog::new();
        let (group, switches) = imp.delegation_group(&dialog);
        let agent_groups: Vec<_> = Driver::ALL
            .into_iter()
            .map(|d| imp.agent_group(d, &dialog, &switches))
            .collect();
        let switch_rows = |root: &gtk4::Widget| -> Vec<adw::SwitchRow> {
            crate::testutil::descendants(root)
                .into_iter()
                .filter_map(|w| w.downcast::<adw::SwitchRow>().ok())
                .collect()
        };
        let master = switch_rows(group.upcast_ref())
            .into_iter()
            .find(|r| r.title() == "Agents can use each other as sub-agents")
            .expect("the master switch");
        let own = |d: Driver| {
            let i = Driver::ALL.iter().position(|x| *x == d).unwrap();
            switch_rows(agent_groups[i].upcast_ref())
                .into_iter()
                .find(|r| r.title() == "Available for delegation")
                .expect("the agent's own switch")
        };
        let available = |d: Driver| imp.agent_profile_now(d).available_for_delegation;
        // The master switch moves on the next idle (see `refresh_master_switch`).
        let settle = || {
            let ctx = glib::MainContext::default();
            while ctx.iteration(false) {}
        };
        settle();
        assert!(!master.is_active());

        master.set_active(true);
        settle();
        for d in [Driver::Claude, Driver::Codex] {
            assert!(available(d), "{d:?} saved on");
            assert!(own(d).is_active(), "{d:?}'s own switch follows");
        }
        assert!(!available(Driver::Agy), "a disabled agent is left alone");
        assert!(master.is_active(), "every enabled agent is on");

        // One agent off on its own: the master goes off and says who is on.
        own(Driver::Codex).set_active(false);
        settle();
        assert!(!available(Driver::Codex));
        assert!(!master.is_active());
        assert!(master
            .subtitle()
            .is_some_and(|s| s.starts_with("Custom: on for Claude")));

        master.set_active(true);
        settle();
        assert!(available(Driver::Codex) && master.is_active());
        master.set_active(false);
        settle();
        for d in [Driver::Claude, Driver::Codex] {
            assert!(!available(d), "{d:?} saved off");
            assert!(!own(d).is_active());
        }
        imp.config.borrow_mut().profiles = original;
    }

    pub(crate) fn pages_have_renderable_titles(imp: &AgentTerminalWindow) {
        let prefs = adw::PreferencesDialog::new();
        imp.add_agents_page(&prefs);
        let page = prefs.visible_page().expect("the Agents page");
        assert_titles_render(&page, "Settings -> Agents");
        let setup = adw::Dialog::new();
        let page = imp.setup_agents_page(&setup);
        assert_titles_render(&page, "setup walkthrough");
    }

    /// A Settings removal never takes the store lock on the GTK thread. While another holder
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

    /// A Settings import's completion (the toast, the sync indicator, `on_changed`) runs
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
            AgentTerminalWindow::rules_job(
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
    fn the_differences_read_names_an_unusable_rules_file() {
        use crate::always_allow::{AlwaysRules, FILE_NAME};
        let dir = tempfile::tempdir().expect("tmp");
        let path = dir.path().join(FILE_NAME);
        let (rules, blocked) = open_rules_for_display(&path);
        assert!(rules.rules.is_empty());
        assert_eq!(blocked, None, "no file yet is no rules, not a problem");
        std::fs::write(&path, "not a rules file {").expect("write");
        let (rules, blocked) = open_rules_for_display(&path);
        assert!(rules.rules.is_empty());
        let err = AlwaysRules::open(&path).expect_err("unusable");
        assert_eq!(
            blocked,
            Some(crate::always_allow::blocked_notice(&path, &err))
        );
    }

    #[test]
    fn the_default_mode_dropdown_lists_every_mode_once() {
        // Exhaustive: a new mode does not compile until it is placed in `MODES` (or kept out).
        let slot = |m: Mode| match m {
            Mode::Ask | Mode::AcceptEdits | Mode::Plan => MODES.iter().position(|x| *x == m),
            // Session-only: never a default.
            Mode::Bypass => None,
        };
        for m in [Mode::Ask, Mode::AcceptEdits, Mode::Plan] {
            let i = slot(m).expect("listed");
            assert_eq!(MODES[i], m);
        }
        assert!(MODES.iter().all(|m| *m != Mode::Bypass));
        let mut seen: Vec<Mode> = MODES.to_vec();
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
        for (i, mode) in MODES.iter().enumerate() {
            match mode {
                Mode::Ask | Mode::AcceptEdits | Mode::Plan => {}
                Mode::Bypass => panic!("dangerous permissive mode is never a default"),
            }
            assert_eq!(MODES.iter().position(|m| m == mode), Some(i));
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
