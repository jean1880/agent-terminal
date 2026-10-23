//! Preferences → Diff Tool: the external viewer the "Open in …" buttons launch.
//!
//! A preset dropdown (Meld, KDiff3, VS Code, git difftool, Beyond Compare, Custom) and an
//! editable argv field. The field is validated as it is typed ([`agent_kit::difftool::validate`])
//! and an invalid value is never persisted. Both apply on change. Presets whose program is not
//! installed are marked (found off the main thread, absolute locations only).

use std::cell::Cell;
use std::rc::Rc;

use agent_kit::difftool::{self, DiffTool, PRESETS};

use super::*;
use crate::diff_tool::DiffTools;

/// Dropdown rows: "None", every preset, then "Custom".
const NONE_INDEX: u32 = 0;
const CUSTOM_INDEX: u32 = PRESETS.len() as u32 + 1;

/// An argv as one editable line: a word is quoted only when it has to be (whitespace, quotes or
/// a backslash), so `{old}` and `--diff` read as they are.
pub(super) fn format_argv(argv: &[String]) -> String {
    argv.iter()
        .map(|a| {
            let plain = !a.is_empty()
                && !a
                    .chars()
                    .any(|c| c.is_whitespace() || matches!(c, '\'' | '"' | '\\'));
            if plain {
                a.clone()
            } else {
                format!("'{}'", a.replace('\'', "'\\''"))
            }
        })
        .collect::<Vec<_>>()
        .join(" ")
}

/// The argv a line holds, split as a shell would (quotes and backslashes, nothing is expanded).
pub(super) fn parse_argv(text: &str) -> Result<Vec<String>, String> {
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

/// The Command field's title while its text is wrong. Row titles are Pango markup, and `why`
/// can quote what was typed (`&`, `<`, an unbalanced quote), so it is escaped.
fn command_error_title(why: &str) -> String {
    format!("Command — {}", gtk4::glib::markup_escape_text(why))
}

/// The tool a field's text describes: a preset when the argv is exactly one, else a custom tool
/// named after its program. `Err` is the reason to show; `Ok(None)` is an empty field.
pub(super) fn tool_from_text(text: &str) -> Result<Option<DiffTool>, String> {
    let argv = parse_argv(text)?;
    if argv.is_empty() {
        return Ok(None);
    }
    let name = |argv: &[String]| {
        let tool = DiffTool {
            name: String::new(),
            argv: argv.to_vec(),
        };
        match difftool::preset_index(&tool) {
            Some(i) => PRESETS[i].name.to_owned(),
            None => std::path::Path::new(&argv[0])
                .file_name()
                .map_or_else(|| argv[0].clone(), |n| n.to_string_lossy().into_owned()),
        }
    };
    let tool = DiffTool {
        name: name(&argv),
        argv,
    };
    difftool::validate(&tool)?;
    Ok(Some(tool))
}

/// Which dropdown row shows `tool`.
pub(super) fn row_of(tool: Option<&DiffTool>) -> u32 {
    match tool {
        None => NONE_INDEX,
        Some(t) => difftool::preset_index(t).map_or(CUSTOM_INDEX, |i| i as u32 + 1),
    }
}

impl AgentTerminalWindow {
    /// Adds the "Diff Tool" group to the Terminal page.
    pub(super) fn add_diff_tool_group(&self, page: &adw::PreferencesPage) {
        let obj = self.obj();
        let current = self.config.borrow().diff_tool.clone();

        let group = adw::PreferencesGroup::builder()
            .title("Diff Tool")
            .description(
                "Opened from a file edit's “Open in …” button and the diff panel. Placeholders: \
                 {old} (the file before the turn), {new}, {path}, {repo}, {rev}. Never run \
                 through a shell. The git difftool preset runs whatever tool your git config \
                 and the repository's own .git/config name.",
            )
            .build();

        let mut names: Vec<String> = vec!["None".to_owned()];
        names.extend(PRESETS.iter().map(|p| p.name.to_owned()));
        names.push("Custom".to_owned());
        let name_refs: Vec<&str> = names.iter().map(String::as_str).collect();
        let model = gtk4::StringList::new(&name_refs);
        let preset_row = adw::ComboRow::builder()
            .title("Viewer")
            .subtitle("Choose a preset, or edit the command below")
            .model(&model)
            .selected(row_of(current.as_ref()))
            .build();

        let command_row = adw::EntryRow::builder()
            .title("Command")
            .tooltip_text(
                "The viewer's command line, with the placeholders above, e.g. meld {old} {new}",
            )
            .text(
                current
                    .as_ref()
                    .map(|t| format_argv(&t.argv))
                    .unwrap_or_default(),
            )
            .build();
        let expand_row = self.config_switch(
            "Expand Diffs",
            "Show a file edit's diff as soon as the edit completes",
            |c| c.diffs_expanded,
            SaveWhen::Owned,
            Self::set_diffs_expanded,
        );
        group.add(&expand_row);
        group.add(&preset_row);
        group.add(&command_row);
        page.add(&group);

        // Set by code that moves one control because the other changed, so the move is not
        // taken for the user's own edit.
        let syncing = Rc::new(Cell::new(false));

        preset_row.connect_selected_notify(glib::clone!(
            #[weak]
            obj,
            #[weak]
            command_row,
            #[strong]
            syncing,
            move |row| {
                if syncing.get() {
                    return;
                }
                let imp = obj.imp();
                match row.selected() {
                    NONE_INDEX => {
                        syncing.set(true);
                        command_row.set_text("");
                        syncing.set(false);
                        command_row.set_title("Command");
                        command_row.remove_css_class("error");
                        imp.set_diff_tool(None);
                    }
                    CUSTOM_INDEX => {
                        // Keep what is typed; a custom tool is whatever the field holds.
                        command_row.grab_focus();
                    }
                    n => {
                        let Some(preset) = PRESETS.get((n - 1) as usize) else {
                            return;
                        };
                        let tool = preset.tool();
                        syncing.set(true);
                        command_row.set_text(&format_argv(&tool.argv));
                        syncing.set(false);
                        command_row.set_title("Command");
                        command_row.remove_css_class("error");
                        imp.set_diff_tool(Some(tool));
                    }
                }
            }
        ));

        // The field says at once whether it is valid, but saves only once the edit is over (the
        // next idle turn), from what it then holds. Replacing the text (select all, paste) passes
        // through an empty field, which is valid ("no tool"): saved at once, it cleared the
        // tool even when the pasted command was then refused.
        let save_pending = Rc::new(Cell::new(false));
        command_row.connect_changed(glib::clone!(
            #[weak]
            obj,
            #[weak]
            preset_row,
            #[strong]
            syncing,
            #[strong]
            save_pending,
            move |entry| {
                if syncing.get() {
                    return;
                }
                match tool_from_text(&entry.text()) {
                    Err(why) => {
                        // Inline, and not saved: the field says what is wrong.
                        entry.add_css_class("error");
                        entry.set_title(&command_error_title(&why));
                    }
                    Ok(_) => {
                        entry.remove_css_class("error");
                        entry.set_title("Command");
                    }
                }
                if save_pending.replace(true) {
                    return;
                }
                let (entry, save_pending, syncing) =
                    (entry.clone(), save_pending.clone(), syncing.clone());
                glib::idle_add_local_once(glib::clone!(
                    #[weak]
                    obj,
                    #[weak]
                    preset_row,
                    move || {
                        save_pending.set(false);
                        // Still invalid when the edit is over: nothing is saved.
                        let Ok(tool) = tool_from_text(&entry.text()) else {
                            return;
                        };
                        // A typed argv that is a preset's selects it; otherwise it is Custom.
                        let want = if tool.is_none() {
                            NONE_INDEX
                        } else {
                            row_of(tool.as_ref())
                        };
                        if preset_row.selected() != want {
                            syncing.set(true);
                            preset_row.set_selected(want);
                            syncing.set(false);
                        }
                        obj.imp().set_diff_tool(tool);
                    }
                ));
            }
        ));

        // Mark presets whose program is not installed, once the (off-thread) probe answers.
        glib::MainContext::default().spawn_local(glib::clone!(
            #[weak]
            model,
            #[weak]
            preset_row,
            async move {
                let installed = crate::diff_tool::installed_presets().await;
                if installed.len() != PRESETS.len() {
                    return;
                }
                let mut missing = Vec::new();
                for (i, present) in installed.iter().enumerate() {
                    if !present {
                        missing.push(PRESETS[i].name);
                        model.splice(
                            i as u32 + 1,
                            1,
                            &[&format!("{} (not installed)", PRESETS[i].name)],
                        );
                    }
                }
                if !missing.is_empty() {
                    preset_row
                        .set_subtitle(&format!("Not found on this system: {}", missing.join(", ")));
                }
            }
        ));
    }

    /// Applies a validated tool (or none): the config, its save, and every card and panel row.
    pub(super) fn set_diff_tool(&self, tool: Option<DiffTool>) {
        if self.config.borrow().diff_tool == tool {
            return;
        }
        self.config.borrow_mut().diff_tool = tool.clone();
        DiffTools::shared().set(tool);
        self.schedule_config_save();
    }

    /// Settings → "Expand Diffs": applies to the next edit that completes, and is saved.
    pub(super) fn set_diffs_expanded(&self, on: bool) {
        if self.config.borrow().diffs_expanded == on {
            return;
        }
        self.config.borrow_mut().diffs_expanded = on;
        DiffTools::shared().set_expand_by_default(on);
        self.schedule_config_save();
    }
}

#[cfg(test)]
pub(super) mod tests {
    use super::*;

    /// The Diff Tool group, driven as a user drives it: what is typed or chosen is what is
    /// saved, and an invalid command is shown and never saved. Run from the window test.
    pub(in crate::window) fn the_diff_tool_group_saves_only_what_is_valid(
        window: &crate::window::AgentTerminalWindow,
    ) {
        let imp = window.imp();
        let page = adw::PreferencesPage::new();
        imp.add_diff_tool_group(&page);
        let widgets = crate::testutil::descendants(page.upcast_ref());
        let find = |pred: &dyn Fn(&gtk4::Widget) -> bool| {
            widgets.iter().find(|w| pred(w)).cloned().expect("row")
        };
        let combo = find(&|w| w.is::<adw::ComboRow>())
            .downcast::<adw::ComboRow>()
            .expect("combo");
        let entry = find(&|w| w.is::<adw::EntryRow>())
            .downcast::<adw::EntryRow>()
            .expect("entry");
        let expand = find(&|w| w.is::<adw::SwitchRow>())
            .downcast::<adw::SwitchRow>()
            .expect("switch");
        let saved = || imp.config.borrow().diff_tool.clone();
        // A typed edit is saved once it is over (the next idle turn).
        let ctx = glib::MainContext::default();
        let type_in = |text: &str| {
            entry.set_text(text);
            crate::testutil::settle(&ctx);
        };

        // A preset typed out selects it and is saved.
        type_in("meld {old} {new}");
        assert_eq!(combo.selected(), 1);
        assert_eq!(saved().map(|t| t.name).as_deref(), Some("Meld"));
        // An edit that breaks it (the last brace deleted) is shown on the field, and the tool
        // saved before it is left alone.
        let end = entry.text().chars().count() as i32;
        entry.delete_text(end - 1, end);
        crate::testutil::settle(&ctx);
        assert!(entry.has_css_class("error"));
        assert_ne!(entry.title(), "Command", "the title says what is wrong");
        assert_eq!(saved().map(|t| t.name).as_deref(), Some("Meld"));
        // Replacing the whole command with an invalid one (select all, paste) passes through an
        // empty field; the saved tool survives it. The placeholder is named in the title.
        type_in("meld {old} {new}");
        type_in("meld {old} {nwe}");
        assert!(entry.title().contains("{nwe}"), "{}", entry.title());
        assert_eq!(
            saved().map(|t| t.name).as_deref(),
            Some("Meld"),
            "a refused paste keeps the saved tool"
        );
        // Clearing the field on purpose is "no tool", and is saved.
        type_in("");
        assert_eq!(saved(), None);
        assert_eq!(combo.selected(), NONE_INDEX);
        // Anything else valid is a custom tool, named after its program.
        type_in("/opt/bin/kompare {old} {new}");
        assert!(!entry.has_css_class("error"));
        assert_eq!(entry.title(), "Command");
        assert_eq!(combo.selected(), CUSTOM_INDEX);
        assert_eq!(saved().map(|t| t.name).as_deref(), Some("kompare"));
        // Choosing a preset writes its command into the field and saves it.
        combo.set_selected(2);
        let second = PRESETS[1].tool();
        assert_eq!(entry.text(), format_argv(&second.argv));
        assert_eq!(saved(), Some(second));
        // Custom keeps what is there; None clears both.
        combo.set_selected(CUSTOM_INDEX);
        assert_eq!(saved(), Some(PRESETS[1].tool()));
        combo.set_selected(NONE_INDEX);
        assert_eq!(entry.text(), "");
        assert_eq!(saved(), None);

        // Expand Diffs is saved as it is switched.
        let before = imp.config.borrow().diffs_expanded;
        expand.set_active(!before);
        assert_eq!(imp.config.borrow().diffs_expanded, !before);
        expand.set_active(before);
        assert_eq!(imp.config.borrow().diffs_expanded, before);

        // Before or after the installed-programs probe answers, a preset is listed under its own
        // name or marked as not installed, never anything else.
        crate::testutil::settle(&glib::MainContext::default());
        let model = combo
            .model()
            .and_then(|m| m.downcast::<gtk4::StringList>().ok())
            .expect("list");
        for (i, preset) in PRESETS.iter().enumerate() {
            let label = model.string(i as u32 + 1).expect("label").to_string();
            assert!(
                label == preset.name || label == format!("{} (not installed)", preset.name),
                "{label}"
            );
        }
    }

    fn argv(s: &[&str]) -> Vec<String> {
        s.iter().map(|a| (*a).to_owned()).collect()
    }

    #[test]
    fn the_error_title_is_valid_markup_whatever_the_reason_quotes() {
        for why in ["unbalanced quote 'a & b", "<{old}> & </x>", "plain"] {
            let title = command_error_title(why);
            assert!(
                gtk4::pango::parse_markup(&title, '\0').is_ok(),
                "{title:?} would render blank"
            );
        }
    }

    #[test]
    fn an_argv_round_trips_through_its_editable_line() {
        for a in [
            argv(&["meld", "{old}", "{new}"]),
            argv(&["code", "--diff", "{old}", "{new}"]),
            argv(&["tool", "two words", "it's", "back\\slash", ""]),
            argv(&[
                "git",
                "-C",
                "{repo}",
                "difftool",
                "--no-prompt",
                "{rev}",
                "--",
                "{path}",
            ]),
        ] {
            let line = format_argv(&a);
            assert_eq!(parse_argv(&line).expect("parses"), a, "{line}");
        }
        assert_eq!(
            format_argv(&argv(&["meld", "{old}", "{new}"])),
            "meld {old} {new}"
        );
    }

    #[test]
    fn typing_a_preset_selects_it_and_anything_else_is_a_custom_tool_named_after_its_program() {
        let t = tool_from_text("meld {old} {new}")
            .expect("ok")
            .expect("tool");
        assert_eq!(t.name, "Meld");
        assert_eq!(row_of(Some(&t)), 1);
        let t = tool_from_text("/opt/bin/kompare {old} {new}")
            .expect("ok")
            .expect("tool");
        assert_eq!(t.name, "kompare");
        assert_eq!(row_of(Some(&t)), CUSTOM_INDEX);
        assert_eq!(tool_from_text("  ").expect("ok"), None);
        assert_eq!(row_of(None), NONE_INDEX);
        assert_eq!(CUSTOM_INDEX as usize, PRESETS.len() + 1);
    }

    #[test]
    fn a_bad_field_says_why_and_is_not_a_tool() {
        assert!(tool_from_text("meld {old} {nwe}")
            .expect_err("placeholder")
            .contains("{nwe}"));
        assert!(tool_from_text("meld").is_err(), "names no file");
        assert!(
            tool_from_text("{old} {new}").is_err(),
            "the program is fixed"
        );
        assert!(
            tool_from_text("meld 'unclosed").is_err(),
            "unbalanced quote"
        );
    }
}
