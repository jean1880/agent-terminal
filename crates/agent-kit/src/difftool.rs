//! The external diff tool: its configuration shape, the presets, and the argv template.
//!
//! A tool is a name and an argv template. Placeholders are substituted per ELEMENT of the argv,
//! and the result is handed to the process as an argv: no shell ever sees it, so a path with
//! spaces, quotes or `$(…)` in it is just a path.
//!
//! | Placeholder | Becomes |
//! |---|---|
//! | `{old}` | an absolute path: a temp file with the pre-turn content |
//! | `{new}` | an absolute path: the working file |
//! | `{path}` | the repo-relative path (`./name` when it would start with `-`) |
//! | `{repo}` | the absolute repo root |
//! | `{rev}` | the pre-turn checkpoint: a hex commit or tree id |
//!
//! `{{` and `}}` are literal braces; any other `{name}` is rejected when the tool is saved.
//!
//! # Leading `-`
//!
//! Choice: every value that could be read as an option is neutralised by construction rather
//! than by inserting `--` (tools differ on whether `--` means anything). `{old}`, `{new}` and
//! `{repo}` are absolute (they start with `/`), `{rev}` is hex, and a repo-relative `{path}` that
//! starts with `-` is passed as `./-name`. The program (the first element) takes no placeholder,
//! so repo content can never choose what runs. A template that wants `--` before a value (the
//! `git difftool` preset has one) writes it itself.

use std::ffi::OsString;
use std::path::Path;

use serde::{Deserialize, Serialize};

/// A configured external diff tool.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DiffTool {
    /// What the buttons call it ("Open in Meld").
    pub name: String,
    /// The program, then its arguments, with placeholders.
    pub argv: Vec<String>,
}

/// The placeholders, in documentation order.
pub const PLACEHOLDERS: [&str; 5] = ["old", "new", "path", "repo", "rev"];

/// A ready-made tool the Settings dropdown offers.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Preset {
    pub name: &'static str,
    pub argv: &'static [&'static str],
}

/// The presets, in dropdown order (the dropdown adds "Custom" after them). Guarded by
/// `presets_are_valid_and_found_by_argv`.
pub const PRESETS: [Preset; 5] = [
    Preset {
        name: "Meld",
        argv: &["meld", "{old}", "{new}"],
    },
    Preset {
        name: "KDiff3",
        argv: &["kdiff3", "{old}", "{new}"],
    },
    Preset {
        name: "VS Code",
        argv: &["code", "--diff", "{old}", "{new}"],
    },
    Preset {
        name: "git difftool",
        argv: &[
            "git",
            "-C",
            "{repo}",
            "difftool",
            "--no-prompt",
            "{rev}",
            "--",
            "{path}",
        ],
    },
    Preset {
        name: "Beyond Compare",
        argv: &["bcompare", "{old}", "{new}"],
    },
];

impl Preset {
    pub fn tool(&self) -> DiffTool {
        DiffTool {
            name: self.name.to_owned(),
            argv: self.argv.iter().map(|s| (*s).to_owned()).collect(),
        }
    }

    /// The program that must be installed for this preset to work.
    pub fn program(&self) -> &'static str {
        self.argv[0]
    }

    /// Whether the program is installed. `locate` is the app's `SystemProbe::locate`; only an
    /// absolute answer counts (a bare name would be looked up again at launch, by whatever PATH
    /// the process has then).
    pub fn is_installed(&self, locate: impl Fn(&str) -> Option<String>) -> bool {
        locate(self.program()).is_some_and(|p| Path::new(&p).is_absolute())
    }
}

/// The index of the preset whose argv is exactly `tool`'s, if any (the dropdown's selection).
pub fn preset_index(tool: &DiffTool) -> Option<usize> {
    PRESETS.iter().position(|p| {
        p.argv
            .iter()
            .copied()
            .eq(tool.argv.iter().map(String::as_str))
    })
}

/// One piece of an argv element.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Piece<'a> {
    Text(String),
    Holder(&'a str),
}

/// Splits `element` into literal text and placeholders. `Err` names the problem.
fn pieces(element: &str) -> Result<Vec<Piece<'_>>, String> {
    let mut out = Vec::new();
    let mut text = String::new();
    let mut rest = element;
    while let Some(i) = rest.find(['{', '}']) {
        text.push_str(&rest[..i]);
        let tail = &rest[i..];
        if tail.starts_with("{{") || tail.starts_with("}}") {
            text.push_str(&tail[..1]);
            rest = &tail[2..];
        } else if tail.starts_with('}') {
            return Err(format!("a lone }} in “{element}” (write }}}} for a brace)"));
        } else {
            let Some(close) = tail.find('}') else {
                return Err(format!("an unclosed {{ in “{element}”"));
            };
            let name = &tail[1..close];
            let Some(known) = PLACEHOLDERS.iter().find(|p| **p == name) else {
                return Err(format!(
                    "unknown placeholder {{{name}}} (use {})",
                    PLACEHOLDERS.map(|p| format!("{{{p}}}")).join(", ")
                ));
            };
            if !text.is_empty() {
                out.push(Piece::Text(std::mem::take(&mut text)));
            }
            out.push(Piece::Holder(known));
            rest = &tail[close + 1..];
        }
    }
    text.push_str(rest);
    if !text.is_empty() {
        out.push(Piece::Text(text));
    }
    Ok(out)
}

/// Checks a tool as it is typed: a name, a program, no placeholder in the program, only known
/// placeholders elsewhere, and at least one of `{old}`/`{new}`/`{path}`/`{rev}` so the tool is
/// told which file it is opening.
pub fn validate(tool: &DiffTool) -> Result<(), String> {
    if tool.name.trim().is_empty() {
        return Err("Give the tool a name".to_owned());
    }
    let Some(program) = tool.argv.first().filter(|p| !p.trim().is_empty()) else {
        return Err("Name the program to run".to_owned());
    };
    if program.starts_with('-') {
        return Err("The program cannot start with “-”".to_owned());
    }
    if pieces(program)?
        .iter()
        .any(|p| matches!(p, Piece::Holder(_)))
    {
        return Err("The program itself cannot be a placeholder".to_owned());
    }
    let mut names_a_file = false;
    for element in &tool.argv[1..] {
        for piece in pieces(element)? {
            if let Piece::Holder(h) = piece {
                names_a_file |= h != "repo";
            }
        }
    }
    if !names_a_file {
        return Err(
            "Use {old} and {new} (or {path} / {rev}) so the tool knows what to open".to_owned(),
        );
    }
    Ok(())
}

/// What the placeholders become.
#[derive(Debug, Clone)]
pub struct Values {
    pub old: OsString,
    pub new: OsString,
    pub path: String,
    pub repo: OsString,
    pub rev: String,
}

/// The argv for `tool` with its placeholders filled, element by element. The program is
/// returned as given (see the module docs). Refuses a value that could be read as an option.
pub fn substitute(tool: &DiffTool, values: &Values) -> Result<Vec<OsString>, String> {
    validate(tool)?;
    let absolute = |name: &str, v: &OsString| -> Result<(), String> {
        if Path::new(v).is_absolute() {
            Ok(())
        } else {
            Err(format!("{{{name}}} is not an absolute path"))
        }
    };
    absolute("old", &values.old)?;
    absolute("new", &values.new)?;
    absolute("repo", &values.repo)?;
    if values.rev.is_empty() || !values.rev.bytes().all(|b| b.is_ascii_hexdigit()) {
        return Err("{rev} is not a commit or tree id".to_owned());
    }
    // A repo-relative path that would read as an option is made explicit.
    let path = if values.path.starts_with('-') {
        format!("./{}", values.path)
    } else {
        values.path.clone()
    };
    let mut out = Vec::with_capacity(tool.argv.len());
    for (i, element) in tool.argv.iter().enumerate() {
        if i == 0 {
            out.push(OsString::from(element));
            continue;
        }
        let mut arg = OsString::new();
        for piece in pieces(element)? {
            match piece {
                Piece::Text(t) => arg.push(t),
                Piece::Holder("old") => arg.push(&values.old),
                Piece::Holder("new") => arg.push(&values.new),
                Piece::Holder("path") => arg.push(&path),
                Piece::Holder("repo") => arg.push(&values.repo),
                Piece::Holder(_) => arg.push(&values.rev),
            }
        }
        out.push(arg);
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tool(argv: &[&str]) -> DiffTool {
        DiffTool {
            name: "T".into(),
            argv: argv.iter().map(|s| (*s).to_owned()).collect(),
        }
    }

    fn values(path: &str) -> Values {
        Values {
            old: "/run/user/1000/agent-terminal/diff/1/a b.rs".into(),
            new: "/home/me/my repo/src/a b.rs".into(),
            path: path.into(),
            repo: "/home/me/my repo".into(),
            rev: "0123abcd".into(),
        }
    }

    #[test]
    fn presets_are_valid_and_found_by_argv() {
        for (i, p) in PRESETS.iter().enumerate() {
            validate(&p.tool()).unwrap_or_else(|e| panic!("{}: {e}", p.name));
            assert_eq!(preset_index(&p.tool()), Some(i));
        }
        assert_eq!(preset_index(&tool(&["meld", "{new}", "{old}"])), None);
        assert_eq!(PRESETS[3].tool().argv[..2], ["git", "-C"]);
    }

    #[test]
    fn placeholders_fill_per_element_and_a_path_with_spaces_stays_one_argument() {
        let argv =
            substitute(&tool(&["meld", "{old}", "{new}"]), &values("src/a b.rs")).expect("ok");
        assert_eq!(argv.len(), 3);
        assert_eq!(
            argv[1],
            OsString::from("/run/user/1000/agent-terminal/diff/1/a b.rs")
        );
        assert_eq!(argv[2], OsString::from("/home/me/my repo/src/a b.rs"));
        // Mixed text and placeholders inside one element.
        let argv = substitute(
            &tool(&["tool", "--left={old}", "--title", "{path} @ {rev}"]),
            &values("src/a b.rs"),
        )
        .expect("ok");
        assert_eq!(
            argv[1],
            OsString::from("--left=/run/user/1000/agent-terminal/diff/1/a b.rs")
        );
        assert_eq!(argv[3], OsString::from("src/a b.rs @ 0123abcd"));
    }

    #[test]
    fn nothing_is_run_through_a_shell_so_metacharacters_are_plain_text() {
        let v = Values {
            new: "/w/$(touch pwned);`x`.rs".into(),
            ..values("a")
        };
        let argv = substitute(&tool(&["meld", "{old}", "{new}"]), &v).expect("ok");
        assert_eq!(argv[2], OsString::from("/w/$(touch pwned);`x`.rs"));
    }

    #[test]
    fn a_unknown_placeholder_or_a_stray_brace_is_rejected() {
        for bad in [
            &["meld", "{old}", "{nwe}"][..],
            &["meld", "{old", "{new}"],
            &["meld", "old}", "{new}"],
            &["meld", "{}", "{new}"],
        ] {
            assert!(validate(&tool(bad)).is_err(), "{bad:?}");
        }
        assert!(validate(&tool(&["meld", "{{literal}}", "{new}"])).is_ok());
        let argv = substitute(&tool(&["tool", "{{x}}", "{new}"]), &values("a")).expect("ok");
        assert_eq!(argv[1], OsString::from("{x}"));
    }

    #[test]
    fn the_program_is_fixed_and_the_tool_must_name_a_file() {
        assert!(validate(&tool(&["{old}", "{new}"])).is_err());
        assert!(validate(&tool(&["-x", "{new}"])).is_err());
        assert!(validate(&tool(&[""])).is_err());
        assert!(validate(&tool(&[])).is_err());
        assert!(validate(&tool(&["meld"])).is_err(), "names no file");
        assert!(
            validate(&tool(&["git", "-C", "{repo}"])).is_err(),
            "repo alone names no file"
        );
        assert!(validate(&DiffTool {
            name: " ".into(),
            argv: vec!["meld".into(), "{new}".into()]
        })
        .is_err());
    }

    #[test]
    fn a_leading_dash_in_the_relative_path_is_made_explicit_and_the_rest_are_absolute() {
        let argv = substitute(&PRESETS[3].tool(), &values("-rf.txt")).expect("ok");
        assert_eq!(argv.last(), Some(&OsString::from("./-rf.txt")));
        // `--` stays where the template put it.
        assert_eq!(argv[argv.len() - 2], OsString::from("--"));
        // An absolute-path value that is not absolute, or a rev that is not hex, is refused.
        let bad = Values {
            new: "relative".into(),
            ..values("a")
        };
        assert!(substitute(&tool(&["meld", "{old}", "{new}"]), &bad).is_err());
        let bad = Values {
            rev: "--output=x".into(),
            ..values("a")
        };
        assert!(substitute(&PRESETS[3].tool(), &bad).is_err());
    }

    #[test]
    fn only_an_absolute_location_counts_as_installed() {
        let meld = &PRESETS[0];
        assert!(meld.is_installed(|c| (c == "meld").then(|| "/usr/bin/meld".to_owned())));
        assert!(!meld.is_installed(|_| None));
        assert!(
            !meld.is_installed(|c| Some(c.to_owned())),
            "a bare name is not a location"
        );
    }
}
