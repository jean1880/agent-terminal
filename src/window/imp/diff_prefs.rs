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
                 through a shell.",
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
            .text(
                current
                    .as_ref()
                    .map(|t| format_argv(&t.argv))
                    .unwrap_or_default(),
            )
            .build();
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

        command_row.connect_changed(glib::clone!(
            #[weak]
            obj,
            #[weak]
            preset_row,
            #[strong]
            syncing,
            move |entry| {
                if syncing.get() {
                    return;
                }
                match tool_from_text(&entry.text()) {
                    Err(why) => {
                        // Inline, and not saved: the field says what is wrong.
                        entry.add_css_class("error");
                        entry.set_title(&format!("Command — {why}"));
                    }
                    Ok(tool) => {
                        entry.remove_css_class("error");
                        entry.set_title("Command");
                        let want = row_of(tool.as_ref());
                        // A typed argv that is a preset's selects it; otherwise it is Custom.
                        let want = if tool.is_none() { NONE_INDEX } else { want };
                        if preset_row.selected() != want {
                            syncing.set(true);
                            preset_row.set_selected(want);
                            syncing.set(false);
                        }
                        obj.imp().set_diff_tool(tool);
                    }
                }
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
}

#[cfg(test)]
mod tests {
    use super::*;

    fn argv(s: &[&str]) -> Vec<String> {
        s.iter().map(|a| (*a).to_owned()).collect()
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
