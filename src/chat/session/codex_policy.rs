//! Whether a Codex permission request may be allowed for the rest of the Codex session, judged
//! on what its targets really are on disk (red-team #382–#384, Codex peer #3/#4/#8).
//!
//! The adapter types the request lexically; this module looks at the filesystem. It runs off
//! the main thread ([`judge`] does blocking I/O). Allow for session is offered only when every
//! read and write target is a literal path (or the thread's project) that resolves, through any
//! symlinks, to a place inside the thread's folder, or to one existing file outside it; and
//! nothing it reaches is the filesystem root, the home folder or above, Codex's own folder, or
//! a path the sensitive list names in any component. Globs and places Codex resolves itself
//! (the temp folder, `/tmp`, system libraries, the whole filesystem) get Allow for this turn
//! only. Anything that cannot be checked withholds it (fail closed).

use std::ffi::OsString;
use std::path::{Component, Path, PathBuf};

use serde_json::Value;

use crate::agent_proc::AgentEnv;

/// The longest warning shown on a card.
const MAX_WARNING: usize = 600;

/// The folders the Codex child sees, as far as they can be known. `None` is "cannot tell":
/// no session grant is offered then.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct ChildHomes {
    pub home: Option<PathBuf>,
    pub codex: Option<PathBuf>,
}

/// The value of `name` in the Codex child's environment: the profile's env file wins, then a
/// `clear_env` removal, then what the app inherited. Empty counts as unset (as Codex reads it).
fn child_var(env: &AgentEnv, name: &str, inherited: Option<OsString>) -> Option<OsString> {
    let value = match env.env.iter().rev().find(|(k, _)| k == name) {
        Some((_, v)) => Some(OsString::from(v)),
        None if env.unset.iter().any(|k| k == name) => None,
        None => inherited,
    };
    value.filter(|v| !v.is_empty())
}

/// An absolute path with no `.` or `..` component: one that names one place as written.
fn plain_absolute(path: PathBuf) -> Option<PathBuf> {
    (path.is_absolute()
        && path
            .components()
            .all(|c| matches!(c, Component::RootDir | Component::Normal(_))))
    .then_some(path)
}

/// `HOME` and `CODEX_HOME` as the Codex child sees them (`inherited_*`: the app's own values).
/// A relative value, or one with `.` or `..`, is unknown; `CODEX_HOME` unset is `$HOME/.codex`.
/// The same order as the rules lane's `codex_home::resolve`, which this can be swapped for.
pub(super) fn child_homes(
    env: &AgentEnv,
    inherited_home: Option<OsString>,
    inherited_codex: Option<OsString>,
) -> ChildHomes {
    let home = child_var(env, "HOME", inherited_home).and_then(|h| plain_absolute(h.into()));
    let codex = match child_var(env, "CODEX_HOME", inherited_codex) {
        Some(value) => plain_absolute(value.into()),
        None => home.as_ref().map(|h| h.join(".codex")),
    };
    ChildHomes { home, codex }
}

/// What [`judge`] decided for one card.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub(super) struct Verdict {
    pub session_allowed: bool,
    pub warning: Option<String>,
    /// For each entry of `input.entries`, where it really leads when that differs from what it
    /// says (a symlink on the way).
    pub resolved: Vec<Option<String>>,
}

/// `path` with every symlink resolved: the nearest existing ancestor is canonicalised and the
/// missing rest appended. `None` when that fails for any reason but "not found", or when a
/// component exists but cannot be followed (a dangling or looping link).
pub(super) fn real(path: &Path) -> Option<PathBuf> {
    let mut existing = path.to_path_buf();
    let mut rest: Vec<OsString> = Vec::new();
    loop {
        match std::fs::symlink_metadata(&existing) {
            Ok(_) => {
                let mut out = std::fs::canonicalize(&existing).ok()?;
                out.extend(rest.iter().rev());
                return Some(out);
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                rest.push(existing.file_name()?.to_owned());
                if !existing.pop() {
                    return None;
                }
            }
            Err(_) => return None,
        }
    }
}

/// The folder an absolute glob starts from: everything up to the last `/` before its first
/// wildcard.
fn glob_base(pattern: &str) -> PathBuf {
    let fixed = &pattern[..pattern.find(['*', '?', '[', '{']).unwrap_or(pattern.len())];
    let dir = fixed.rfind('/').map_or("/", |i| &fixed[..=i]);
    PathBuf::from(if dir.is_empty() { "/" } else { dir })
}

/// The literal (wildcard-free) segments of a glob, as one path, for the sensitive-name check.
fn glob_literals(pattern: &str) -> PathBuf {
    let mut out = PathBuf::from("/");
    for segment in pattern.split('/') {
        if !segment.is_empty() && !segment.contains(['*', '?', '[', '{']) {
            out.push(segment);
        }
    }
    out
}

/// Collects what withholds Allow for session: `danger` (places too broad or sensitive to grant
/// even once without a warning) and `scope` (places a session grant is not given for).
#[derive(Default)]
struct Findings {
    danger: Vec<String>,
    scope: Vec<String>,
    /// A granted folder holds the project's settings or history (or is the project root).
    settings: bool,
}

/// Said when a grant takes in the project's own settings and history.
const SETTINGS_WARNING: &str =
    "This includes the project's own settings and history (.git, agent instructions).";

/// Most entries listed when looking for settings in a folder; a larger one is not vouched for.
const MAX_LISTED: usize = 10_000;

/// Whether `dir`'s own entries include a name the sensitive list holds (`.git`, `.github`,
/// agent instruction files, tool configs) or an `.env` file. `None` when it cannot be listed in
/// full.
fn holds_settings(dir: &Path) -> Option<bool> {
    let mut listed = 0;
    for entry in std::fs::read_dir(dir).ok()? {
        let entry = entry.ok()?;
        listed += 1;
        if listed > MAX_LISTED {
            return None;
        }
        let name = entry.file_name().to_string_lossy().to_ascii_lowercase();
        if name.starts_with(".env")
            || name == ".github"
            || crate::approval_server::sensitive_anywhere(&Path::new("/").join(&name), None)
        {
            return Some(true);
        }
    }
    Some(false)
}

impl Findings {
    fn danger(&mut self, what: impl Into<String>) {
        let what = what.into();
        if !self.danger.contains(&what) {
            self.danger.push(what);
        }
    }

    fn scope(&mut self, what: impl Into<String>) {
        let what = what.into();
        if !self.scope.contains(&what) {
            self.scope.push(what);
        }
    }

    /// The broad and sensitive places `path` reaches, judged on `path` as given.
    fn reaches(&mut self, path: &Path, homes: &[PathBuf], codex: &[PathBuf], everything: bool) {
        if path == Path::new("/") {
            self.danger("the whole filesystem");
        }
        if homes.iter().any(|h| h.starts_with(path)) {
            self.danger(if everything {
                "everything under your home folder"
            } else {
                "your home folder"
            });
        }
        if codex
            .iter()
            .any(|c| c.starts_with(path) || path.starts_with(c))
        {
            self.danger("Codex's own settings folder");
        }
        if homes.is_empty() && crate::approval_server::sensitive_anywhere(path, None)
            || homes
                .iter()
                .any(|h| crate::approval_server::sensitive_anywhere(path, Some(h)))
        {
            self.danger(
                "files that can run code or hold credentials (such as .git, agent instructions \
                 or dotfiles)",
            );
        }
    }
}

/// Judges a card's `input` (the adapter's shape) for a thread in `workspace`, with the child's
/// `homes`. Blocking: it reads the filesystem.
pub(super) fn judge(input: &Value, workspace: &Path, homes: &ChildHomes) -> Verdict {
    let mut f = Findings::default();
    let entries = input
        .get("entries")
        .and_then(Value::as_array)
        .map(Vec::as_slice)
        .unwrap_or_default();
    // Every name a place is known by: as configured and as it really is.
    let both = |p: &Option<PathBuf>| -> Vec<PathBuf> {
        p.iter()
            .flat_map(|p| [Some(p.clone()), real(p)])
            .flatten()
            .collect()
    };
    let homes_known = both(&homes.home);
    let codex_known = both(&homes.codex);
    if homes.home.is_none() {
        f.scope("agent-terminal cannot tell which home folder Codex uses");
    }
    if homes.codex.is_none() {
        f.scope("agent-terminal cannot tell where Codex keeps its settings");
    }
    let workspace_real = real(workspace);
    if workspace_real.is_none() {
        f.scope("the thread's folder could not be checked");
    }
    let mut resolved = Vec::with_capacity(entries.len());
    for entry in entries {
        resolved.push(None);
        if entry.get("access").and_then(Value::as_str) == Some("deny") {
            continue; // a deny restricts
        }
        let target = &entry["target"];
        let path = target.get("path").and_then(Value::as_str);
        match (
            target.get("kind").and_then(Value::as_str),
            target.get("name").and_then(Value::as_str),
        ) {
            (Some("glob"), _) => {
                let pattern = target["pattern"].as_str().unwrap_or_default();
                let base = glob_base(pattern);
                let everything = pattern.contains("**");
                f.reaches(&base, &homes_known, &codex_known, everything);
                if let Some(real_base) = real(&base) {
                    f.reaches(&real_base, &homes_known, &codex_known, everything);
                }
                f.reaches(&glob_literals(pattern), &homes_known, &codex_known, false);
                f.scope(format!(
                    "{pattern} is a pattern that reaches everything matching it below {}",
                    base.display()
                ));
                continue;
            }
            (Some("special"), Some(name)) if name != "project" => {
                if let Some(p) = path {
                    f.reaches(Path::new(p), &homes_known, &codex_known, false);
                }
                let place = match name {
                    "temp" => "the temp folder",
                    "slash_tmp" => "/tmp",
                    "minimal" => "system libraries",
                    "root" => "the whole filesystem",
                    _ => "a place",
                };
                f.scope(format!("{place} is a place Codex resolves itself"));
                continue;
            }
            (Some("path" | "special"), _) => {}
            _ => {
                f.scope("part of the request could not be read");
                continue;
            }
        }
        let Some(lexical) = path.map(PathBuf::from) else {
            f.scope("part of the request names no folder");
            continue;
        };
        f.reaches(&lexical, &homes_known, &codex_known, false);
        let Some(actual) = real(&lexical) else {
            f.scope(format!("{} could not be checked", lexical.display()));
            continue;
        };
        f.reaches(&actual, &homes_known, &codex_known, false);
        let inside = workspace_real
            .as_ref()
            .is_some_and(|w| actual.starts_with(w));
        if actual != lexical {
            if let Some(last) = resolved.last_mut() {
                *last = Some(actual.to_string_lossy().into_owned());
            }
            if !inside {
                f.danger(format!(
                    "{}, which leads to {} outside the project",
                    lexical.display(),
                    actual.display()
                ));
            }
        }
        if !inside && !std::fs::metadata(&actual).is_ok_and(|m| m.is_file()) {
            f.scope(format!(
                "{} is outside the project and is not a single existing file",
                lexical.display()
            ));
        }
        // A folder holding the project's settings or history: the project root itself, or one
        // whose own entries include .git, agent instructions or tool configs.
        if std::fs::metadata(&actual).is_ok_and(|m| m.is_dir()) {
            let root = workspace_real.as_ref().is_some_and(|w| *w == actual);
            match holds_settings(&actual) {
                Some(false) if !root => {}
                Some(_) => f.settings = true,
                None => f.scope(format!("{} could not be listed", lexical.display())),
            }
        }
    }
    let mut warning = String::new();
    if f.settings {
        warning.push_str(SETTINGS_WARNING);
    }
    if !f.danger.is_empty() {
        if !warning.is_empty() {
            warning.push(' ');
        }
        warning.push_str(&format!(
            "This reaches {}. Allow it for this turn only if you trust it.",
            agent_core::codex::sentence(&f.danger)
        ));
    }
    if !f.scope.is_empty() {
        if !warning.is_empty() {
            warning.push(' ');
        }
        warning.push_str(&format!(
            "Allow for session is not offered: {}.",
            agent_core::codex::sentence(&f.scope)
        ));
    } else if !f.danger.is_empty() || f.settings {
        warning.push_str(" Allow for session is not offered.");
    }
    Verdict {
        session_allowed: f.danger.is_empty() && f.scope.is_empty() && !f.settings,
        warning: (!warning.is_empty()).then(|| agent_core::codex::clip(&warning, MAX_WARNING)),
        resolved,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn homes(home: &Path) -> ChildHomes {
        ChildHomes {
            home: Some(home.to_owned()),
            codex: Some(home.join(".codex")),
        }
    }

    fn card(entries: Value) -> Value {
        json!({"entries": entries, "network": null})
    }

    fn one(access: &str, target: Value) -> Value {
        card(json!([{"access": access, "target": target}]))
    }

    fn path(p: &Path) -> Value {
        json!({"kind": "path", "path": p})
    }

    /// A home folder with a project in it, both real directories.
    fn tree() -> (tempfile::TempDir, PathBuf, PathBuf) {
        let dir = tempfile::tempdir().expect("tempdir");
        let home = dir.path().join("home");
        let project = home.join("proj");
        std::fs::create_dir_all(project.join("src")).expect("tree");
        (dir, home, project)
    }

    #[test]
    fn literal_paths_inside_the_project_are_session_grantable() {
        let (_dir, home, project) = tree();
        let v = judge(
            &one("write", path(&project.join("src/new.rs"))),
            &project,
            &homes(&home),
        );
        assert!(v.session_allowed, "{v:?}");
        assert_eq!(v.warning, None);
        let v = judge(
            &one(
                "write",
                json!({"kind": "special", "name": "project", "path": project.join("build")}),
            ),
            &project,
            &homes(&home),
        );
        assert!(v.session_allowed, "{v:?}");
        let v = judge(&card(json!([])), &project, &homes(&home));
        assert!(v.session_allowed, "network only");
    }

    /// Red-team #382: a link inside the project that leads to `~/.ssh` is judged where it leads.
    #[test]
    fn a_symlink_is_judged_by_where_it_leads_and_shown_resolved() {
        let (_dir, home, project) = tree();
        let ssh = home.join(".ssh");
        std::fs::create_dir_all(&ssh).expect("ssh");
        std::os::unix::fs::symlink(&ssh, project.join("out")).expect("link");
        let v = judge(
            &one("write", path(&project.join("out/keys"))),
            &project,
            &homes(&home),
        );
        assert!(!v.session_allowed);
        let real_ssh = std::fs::canonicalize(&ssh).expect("real");
        assert_eq!(
            v.resolved,
            [Some(real_ssh.join("keys").to_string_lossy().into_owned())]
        );
        let w = v.warning.expect("warning");
        assert!(w.contains("outside the project"), "{w}");
        assert!(w.contains("dotfiles"), "{w}");
        // A dangling link cannot be checked: withheld.
        std::os::unix::fs::symlink(home.join("gone"), project.join("dangling")).expect("link");
        let v = judge(
            &one("write", path(&project.join("dangling"))),
            &project,
            &homes(&home),
        );
        assert!(!v.session_allowed);
        assert!(v
            .warning
            .is_some_and(|w| w.contains("could not be checked")));
    }

    /// Red-team #384, Codex peer #3/#8: only literal, contained targets get a session grant.
    #[test]
    fn globs_specials_and_folders_outside_the_project_are_turn_only() {
        let (_dir, home, project) = tree();
        let outside = home.join("notes.txt");
        std::fs::write(&outside, "x").expect("file");
        let h = homes(&home);
        for (target, says) in [
            (
                json!({"kind": "glob", "pattern": format!("{}/**/.git/hooks/*", project.display())}),
                "pattern",
            ),
            (
                json!({"kind": "glob", "pattern": format!("{}/src/*/x", home.display())}),
                "pattern",
            ),
            (
                json!({"kind": "special", "name": "temp", "path": null}),
                "the temp folder",
            ),
            (
                json!({"kind": "special", "name": "slash_tmp", "path": "/tmp"}),
                "/tmp",
            ),
            (
                json!({"kind": "special", "name": "minimal", "path": null}),
                "system libraries",
            ),
            (path(&home.join("git")), "not a single existing file"),
        ] {
            let v = judge(&one("write", target.clone()), &project, &h);
            assert!(!v.session_allowed, "{target}");
            assert!(
                v.warning.as_deref().is_some_and(|w| w.contains(says)),
                "{target}: {v:?}"
            );
        }
        // One existing file outside the project is a literal, single target.
        let v = judge(&one("read", path(&outside)), &project, &h);
        assert!(v.session_allowed, "{v:?}");
        // Sensitive names after the first wildcard are still named.
        let v = judge(
            &one(
                "write",
                json!({"kind": "glob", "pattern": format!("{}/*/.git/hooks/*", project.display())}),
            ),
            &project,
            &h,
        );
        assert!(v.warning.is_some_and(|w| w.contains(".git")));
    }

    #[test]
    fn root_home_codex_and_sensitive_places_are_named() {
        let (_dir, home, project) = tree();
        let h = homes(&home);
        let warn = |target: Value| {
            judge(&one("write", target), &project, &h)
                .warning
                .unwrap_or_default()
        };
        assert!(
            warn(json!({"kind": "special", "name": "root", "path": "/"}))
                .contains("the whole filesystem")
        );
        assert!(warn(path(&home)).contains("your home folder"));
        assert!(warn(path(home.parent().expect("parent"))).contains("your home folder"));
        assert!(
            warn(json!({"kind": "glob", "pattern": format!("{}/**/*.txt", home.display())}))
                .contains("everything under your home folder")
        );
        assert!(warn(path(&home.join(".codex/rules"))).contains("Codex's own settings folder"));
        for sensitive in [
            project.join(".git/hooks"),
            home.join(".bashrc"),
            project.join("AGENTS.md"),
            project.join(".github/workflows/ci.yml"),
        ] {
            assert!(
                warn(path(&sensitive)).contains("dotfiles"),
                "{}",
                sensitive.display()
            );
        }
        // Deny entries restrict: never a reason.
        assert!(judge(&one("deny", path(Path::new("/"))), &project, &h).session_allowed);
    }

    /// A folder grant takes in everything below it: the project root, or any folder whose own
    /// entries hold its settings or history, is for this turn only.
    #[test]
    fn folders_holding_the_projects_settings_are_turn_only() {
        let (_dir, home, project) = tree();
        let h = homes(&home);
        let settings = |target: &Path| {
            let v = judge(&one("write", path(target)), &project, &h);
            (v.session_allowed, v.warning.unwrap_or_default())
        };
        let (allowed, warning) = settings(&project);
        assert!(!allowed);
        assert!(warning.starts_with(SETTINGS_WARNING), "{warning}");
        assert!(
            warning.contains("Allow for session is not offered"),
            "{warning}"
        );
        for name in [".git", "AGENTS.md", ".env.local", ".github", ".vscode"] {
            let sub = project.join(format!("sub-{}", name.trim_start_matches('.')));
            std::fs::create_dir_all(&sub).expect("sub");
            let marker = sub.join(name);
            if name.contains('.') && !name.starts_with('.') || name.starts_with(".env") {
                std::fs::write(&marker, "x").expect("file");
            } else {
                std::fs::create_dir_all(&marker).expect("dir");
            }
            let (allowed, warning) = settings(&sub);
            assert!(!allowed, "{name}");
            assert!(
                warning.contains(".git, agent instructions"),
                "{name}: {warning}"
            );
        }
        // A plain sub-folder of the project is session-grantable.
        let (allowed, warning) = settings(&project.join("src"));
        assert!(allowed, "{warning}");
    }

    #[test]
    fn unknown_homes_withhold_session_grants() {
        let (_dir, home, project) = tree();
        for homes in [
            ChildHomes {
                home: None,
                codex: Some(home.join(".codex")),
            },
            ChildHomes {
                home: Some(home.clone()),
                codex: None,
            },
        ] {
            let v = judge(&card(json!([])), &project, &homes);
            assert!(!v.session_allowed, "{homes:?}");
            assert!(v.warning.is_some_and(|w| w.contains("cannot tell")));
        }
    }

    /// Codex peer #4, red-team #391: the child's environment decides, not the app's.
    #[test]
    fn the_childs_home_and_codex_home_are_resolved_like_its_environment() {
        let env = |pairs: &[(&str, &str)], unset: &[&str]| AgentEnv {
            env: pairs
                .iter()
                .map(|(k, v)| ((*k).into(), (*v).into()))
                .collect(),
            unset: unset.iter().map(|k| (*k).into()).collect(),
        };
        let gui_home = Some(OsString::from("/home/gui"));
        let gui_codex = Some(OsString::from("/opt/gui-codex"));
        assert_eq!(
            child_homes(&env(&[], &[]), gui_home.clone(), None),
            ChildHomes {
                home: Some("/home/gui".into()),
                codex: Some("/home/gui/.codex".into())
            }
        );
        assert_eq!(
            child_homes(&env(&[], &[]), gui_home.clone(), gui_codex.clone()).codex,
            Some("/opt/gui-codex".into())
        );
        assert_eq!(
            child_homes(
                &env(&[("HOME", "/home/p"), ("CODEX_HOME", "/p/codex")], &[]),
                gui_home.clone(),
                gui_codex.clone()
            ),
            ChildHomes {
                home: Some("/home/p".into()),
                codex: Some("/p/codex".into())
            }
        );
        // clear_env removes the inherited value: back to the default under the child's home.
        assert_eq!(
            child_homes(
                &env(&[], &["CODEX_HOME"]),
                gui_home.clone(),
                gui_codex.clone()
            )
            .codex,
            Some("/home/gui/.codex".into())
        );
        // Relative, `..` or a missing home: unknown, never guessed.
        assert_eq!(
            child_homes(
                &env(&[("CODEX_HOME", "rel/codex")], &[]),
                gui_home.clone(),
                None
            )
            .codex,
            None
        );
        assert_eq!(
            child_homes(
                &env(&[("CODEX_HOME", "/opt/x/../codex")], &[]),
                gui_home.clone(),
                None
            )
            .codex,
            None
        );
        assert_eq!(
            child_homes(&env(&[], &["HOME"]), gui_home.clone(), None),
            ChildHomes {
                home: None,
                codex: None
            }
        );
        assert_eq!(
            child_homes(&env(&[("HOME", "")], &[]), None, None).home,
            None
        );
    }
}
