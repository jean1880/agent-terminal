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
    (Mode::Plan, "Plan (read-only)"),
];

/// Effort levels offered when the catalogue lists none for the agent.
const FALLBACK_EFFORTS: [&str; 3] = ["low", "medium", "high"];

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

    fn agent_profile_now(&self, driver: Driver) -> Profile {
        self.config
            .borrow()
            .agent_profile(driver)
            .cloned()
            .unwrap_or_else(|| crate::config::new_agent_profile(driver))
    }

    pub(super) fn add_agents_page(&self, dialog: &adw::PreferencesDialog) {
        let obj = self.obj();
        let page = adw::PreferencesPage::builder()
            .title("Agents")
            .icon_name("system-users-symbolic")
            .build();

        let general = adw::PreferencesGroup::builder()
            .title("Chat Threads")
            .description(
                "New threads start on this agent; switch agents and models from a thread's header.",
            )
            .build();
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
        general.add(&default_row);
        page.add(&general);

        for driver in Driver::ALL {
            page.add(&self.agent_group(driver, dialog));
        }
        dialog.add(&page);
    }

    fn agent_group(
        &self,
        driver: Driver,
        dialog: &adw::PreferencesDialog,
    ) -> adw::PreferencesGroup {
        let obj = self.obj();
        let profile = self.agent_profile_now(driver);
        let group = adw::PreferencesGroup::builder()
            .title(driver.info().long_label)
            .description(format!("Profile “{}”", profile.name))
            .build();

        let enabled = adw::SwitchRow::builder()
            .title("Enabled")
            .subtitle(AgentAvailability::shared().get(driver).describe(driver))
            .active(!profile.disabled)
            .build();
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
        group.add(&enabled);
        // The row says what detection found: ready (where), missing (how to install), or off.
        let state_id = AgentAvailability::shared().connect_changed(glib::clone!(
            #[weak]
            enabled,
            move || {
                enabled.set_subtitle(&AgentAvailability::shared().get(driver).describe(driver));
            }
        ));
        dialog.connect_closed(move |_| AgentAvailability::shared().disconnect(state_id));

        // Command, with what detection makes of it.
        let status = adw::ActionRow::builder()
            .title("Detected")
            .subtitle("Checking…")
            .subtitle_selectable(true)
            .build();
        let command = adw::EntryRow::builder()
            .title("Command or Path")
            .text(&profile.command)
            .build();
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
            move |row| {
                let text = row.text().trim().to_owned();
                if !command_ok(&text) {
                    row.add_css_class("error");
                    row.set_tooltip_text(Some("Enter a command name or a path (not an option)"));
                    return;
                }
                row.remove_css_class("error");
                row.set_tooltip_text(None);
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
        group.add(&status);

        let args = adw::EntryRow::builder()
            .title("Extra Arguments")
            .text(
                profile
                    .args
                    .iter()
                    .map(|a| crate::utils::shell_quote(a))
                    .collect::<Vec<_>>()
                    .join(" "),
            )
            .build();
        args.connect_changed(glib::clone!(
            #[weak]
            obj,
            move |row| match parse_args(&row.text()) {
                Ok(parsed) => {
                    row.remove_css_class("error");
                    row.set_tooltip_text(None);
                    let imp = obj.imp();
                    if imp.agent_profile_now(driver).args != parsed {
                        imp.edit_agent_profile(driver, |p| p.args = parsed);
                    }
                }
                Err(reason) => {
                    row.add_css_class("error");
                    row.set_tooltip_text(Some(&reason));
                }
            }
        ));
        group.add(&args);

        let env_file = adw::EntryRow::builder()
            .title("Environment File")
            .text(profile.env_file.as_deref().unwrap_or(""))
            .build();
        env_file.connect_changed(glib::clone!(
            #[weak]
            obj,
            move |row| {
                let text = row.text().trim().to_owned();
                if !env_file_ok(&text) {
                    row.add_css_class("error");
                    row.set_tooltip_text(Some("That file does not exist"));
                    return;
                }
                row.remove_css_class("error");
                row.set_tooltip_text(None);
                let value = (!text.is_empty()).then_some(text);
                let imp = obj.imp();
                if imp.agent_profile_now(driver).env_file != value {
                    imp.edit_agent_profile(driver, |p| p.env_file = value);
                    imp.refresh_agent_data();
                }
            }
        ));
        group.add(&env_file);

        // Default model, from the live catalogue; the saved one stays listed even when the
        // catalogue does not have it (yet).
        let model_row = adw::ComboRow::builder()
            .title("Default Model")
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
        if driver == Driver::Agy {
            mode_row.set_subtitle("Without the approval hook agy always runs in Plan");
        }
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
            self.add_hook_rows(&group);
        }
        group
    }

    /// agy's approval hook: whether hooks.json installs it, the entry to add, and a Copy button.
    fn add_hook_rows(&self, group: &adw::PreferencesGroup) {
        let entry = crate::hook_config::install_entry_json();
        let hook = adw::ActionRow::builder()
            .title("Approval Hook")
            .subtitle("Checking ~/.gemini/config/hooks.json…")
            .build();
        let copy = Button::builder()
            .icon_name("edit-copy-symbolic")
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
                 installed, agy runs read-only (plan mode).",
            )
            .build();
        expander.add_row(&json);
        group.add(&expander);

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
                            "Not installed: agy runs read-only until the entry below is added",
                        );
                        hook.add_css_class("error");
                    }
                }
            }
        ));
    }
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
mod tests {
    use super::*;

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
