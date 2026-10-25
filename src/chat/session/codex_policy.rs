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
//!
//! A session grant names a path, and Codex resolves that name again on every later command. So
//! a name whose folder Codex can write (the project under Accept edits, or a place an earlier
//! session grant covers) is turn-only: Codex could swap it for a link later. A hard-linked file
//! is turn-only too. A folder is looked into one level deep only: the project root, or a folder
//! whose own entries include `.git`, agent instructions or tool configs, is turn-only; deeper
//! contents are not listed.
//!
//! Assumption: the app and Codex see the same filesystem (same user, same machine, same mount
//! namespace). A Codex run in another namespace or container would resolve names differently,
//! and these checks would not describe what it opens.

use std::ffi::OsString;
use std::path::{Path, PathBuf};

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

/// `path` when it names one place as written ([`agent_core::codex::is_plain_absolute`]).
fn plain_absolute(path: PathBuf) -> Option<PathBuf> {
    agent_core::codex::is_plain_absolute(&path).then_some(path)
}

/// `HOME` and `CODEX_HOME` as the Codex child sees them, through the one resolver the rules
/// lane uses too ([`crate::codex_home`]): `inherited` is the app's own environment, `env` the
/// thread's (its profile's variables and what `clear_env` removes). A relative value, or one
/// with `.` or `..`, is unknown; `CODEX_HOME` unset is `$HOME/.codex`.
pub(super) fn child_homes(env: &AgentEnv, inherited: Vec<(String, String)>) -> ChildHomes {
    let child = crate::codex_home::child_env(&crate::codex_home::Inputs {
        inherited,
        shell_path: None,
        env_file: env.env.clone(),
        clear: env.unset.clone(),
    });
    let home = crate::codex_home::get(&child, "HOME").and_then(|h| plain_absolute(h.into()));
    let codex = crate::codex_home::resolve(&child)
        .ok()
        .and_then(plain_absolute);
    ChildHomes { home, codex }
}

/// What a card is judged against: the card's `input` (the adapter's shape) and the thread as
/// it is now.
#[derive(Debug, Clone)]
pub(super) struct Check {
    pub input: Value,
    /// The thread's folder.
    pub workspace: PathBuf,
    pub homes: ChildHomes,
    /// The agent may write inside the project without asking (Accept edits: `workspaceWrite`).
    pub project_writable: bool,
    /// Where earlier session grants already let Codex write (resolved).
    pub granted: Vec<PathBuf>,
}

/// What [`judge`] decided for one card. Two verdicts for the same card compare equal only if
/// nothing a grant depends on moved in between (see the answer-time re-check).
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub(super) struct Verdict {
    pub session_allowed: bool,
    pub warning: Option<String>,
    /// For each entry of `input.entries`, where it really leads when that differs from what it
    /// says (a symlink on the way).
    pub resolved: Vec<Option<String>>,
    /// For each entry, the identity of what it really names on disk (device, inode, link count)
    /// when it exists.
    pub marks: Vec<Option<(u64, u64, u64)>>,
    /// Literal path entries (`kind: "path"`) that lead elsewhere: as written → as resolved. The
    /// grant names them as resolved, which the card showed.
    pub renames: Vec<(String, String)>,
    /// The resolved targets of write entries, for later parent checks.
    pub writes: Vec<PathBuf>,
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
    /// A granted file has hard links elsewhere.
    linked: bool,
    /// Codex can write the folder holding a granted name, so it could swap that name later.
    renameable: bool,
}

/// Said when a grant takes in the project's own settings and history. Only a folder's own
/// entries are looked at, so it says "directly".
const SETTINGS_WARNING: &str = "This includes the project's own settings and history (.git, \
     agent instructions) kept directly in this folder.";

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

/// Said when Codex could swap a granted name for something else later.
const RENAME_NOTE: &str =
    "Codex could rename this place later, so it is allowed for this turn only.";
/// Said when a granted file has hard links.
const LINKED_NOTE: &str = "This file has other names elsewhere on disk.";

/// Where the card's own write entries would let Codex write, judged before any target (Codex
/// peer check): a literal path or the project covers itself (as written and as resolved), a
/// write glob covers the folder it starts from, a special place covers its folder. A special
/// place with no known folder covers everything (`true`).
fn card_writes(entries: &[Value]) -> (Vec<PathBuf>, bool) {
    let mut covered = Vec::new();
    for entry in entries {
        if entry.get("access").and_then(Value::as_str) != Some("write") {
            continue;
        }
        let target = &entry["target"];
        let place = match target.get("kind").and_then(Value::as_str) {
            Some("glob") => Some(glob_base(target["pattern"].as_str().unwrap_or("/"))),
            Some("path" | "special") => target
                .get("path")
                .and_then(Value::as_str)
                .map(PathBuf::from),
            _ => None,
        };
        let Some(place) = place else {
            return (covered, true);
        };
        covered.extend(real(&place));
        covered.push(place);
    }
    (covered, false)
}

/// [`judge_check`] for a thread in Ask mode with no earlier session grants.
#[cfg(test)]
pub(super) fn judge(input: &Value, workspace: &Path, homes: &ChildHomes) -> Verdict {
    judge_check(&Check {
        input: input.clone(),
        workspace: workspace.to_owned(),
        homes: homes.clone(),
        project_writable: false,
        granted: Vec::new(),
    })
}

/// Judges a card. Blocking: it reads the filesystem, in the app's own mount namespace, which is
/// assumed to be the one Codex sees (both run as the same user on the same machine).
pub(super) fn judge_check(check: &Check) -> Verdict {
    let mut f = Findings::default();
    let entries = check
        .input
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
    let homes_known = both(&check.homes.home);
    let codex_known = both(&check.homes.codex);
    if check.homes.home.is_none() {
        f.scope("agent-terminal cannot tell which home folder Codex uses");
    }
    if check.homes.codex.is_none() {
        f.scope("agent-terminal cannot tell where Codex keeps its settings");
    }
    let workspace_real = real(&check.workspace);
    if workspace_real.is_none() {
        f.scope("the thread's folder could not be checked");
    }
    let (covered, covers_all) = card_writes(entries);
    let mut v = Verdict::default();
    for entry in entries {
        v.resolved.push(None);
        v.marks.push(None);
        if entry.get("access").and_then(Value::as_str) == Some("deny") {
            continue; // a deny restricts
        }
        let write = entry.get("access").and_then(Value::as_str) == Some("write");
        let target = &entry["target"];
        let kind = target.get("kind").and_then(Value::as_str);
        let path = target.get("path").and_then(Value::as_str);
        match (kind, target.get("name").and_then(Value::as_str)) {
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
                let place = crate::chat::view::permission::special_words(name).unwrap_or("a place");
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
        let meta = std::fs::metadata(&actual).ok();
        if let (Some(mark), Some(meta)) = (v.marks.last_mut(), &meta) {
            use std::os::unix::fs::MetadataExt;
            // A folder's link count moves with its sub-folders; only a file's says it has
            // other names.
            let links = if meta.is_file() { meta.nlink() } else { 0 };
            *mark = Some((meta.dev(), meta.ino(), links));
        }
        if write {
            v.writes.push(actual.clone());
        }
        let inside = workspace_real
            .as_ref()
            .is_some_and(|w| actual.starts_with(w));
        if actual != lexical {
            if let Some(last) = v.resolved.last_mut() {
                *last = Some(actual.to_string_lossy().into_owned());
            }
            if kind == Some("path") {
                v.renames.push((
                    lexical.to_string_lossy().into_owned(),
                    actual.to_string_lossy().into_owned(),
                ));
            }
            if !inside {
                f.danger(format!(
                    "{}, which leads to {} outside the project",
                    lexical.display(),
                    actual.display()
                ));
            }
        }
        let is_file = meta.as_ref().is_some_and(|m| m.is_file());
        if !inside && !is_file {
            f.scope(format!(
                "{} is outside the project and is not a single existing file",
                lexical.display()
            ));
        }
        if is_file
            && meta.as_ref().is_some_and(|m| {
                use std::os::unix::fs::MetadataExt;
                m.nlink() > 1
            })
        {
            f.linked = true;
        }
        // Whoever can write the folder holding the granted name can swap that name later.
        let parent = actual.parent().unwrap_or(Path::new("/"));
        let parent_in_project = workspace_real
            .as_ref()
            .is_some_and(|w| parent.starts_with(w));
        // That includes what this same card would let Codex write.
        if (check.project_writable && parent_in_project)
            || check.granted.iter().any(|g| parent.starts_with(g))
            || covers_all
            || covered.iter().any(|c| parent.starts_with(c))
        {
            f.renameable = true;
        }
        // A folder holding the project's settings or history directly (one level deep): the
        // project root itself, or one whose own entries include .git or agent instructions.
        if meta.as_ref().is_some_and(|m| m.is_dir()) {
            let root = workspace_real.as_ref().is_some_and(|w| *w == actual);
            match holds_settings(&actual) {
                Some(false) if !root => {}
                Some(_) => f.settings = true,
                None => f.scope(format!("{} could not be listed", lexical.display())),
            }
        }
    }
    let mut parts: Vec<String> = Vec::new();
    if f.settings {
        parts.push(SETTINGS_WARNING.to_owned());
    }
    if !f.danger.is_empty() {
        parts.push(format!(
            "This reaches {}. Allow it for this turn only if you trust it.",
            agent_core::codex::sentence(&f.danger)
        ));
    }
    if f.linked {
        parts.push(LINKED_NOTE.to_owned());
    }
    if f.renameable {
        parts.push(RENAME_NOTE.to_owned());
    }
    if !f.scope.is_empty() {
        parts.push(format!(
            "Allow for session is not offered: {}.",
            agent_core::codex::sentence(&f.scope)
        ));
    } else if !parts.is_empty() && !f.renameable {
        parts.push("Allow for session is not offered.".to_owned());
    }
    v.session_allowed = parts.is_empty();
    v.warning = (!parts.is_empty()).then(|| agent_core::codex::clip(&parts.join(" "), MAX_WARNING));
    v
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
        // The app's own environment.
        let app = |codex: Option<&str>| -> Vec<(String, String)> {
            let mut vars = vec![("HOME".to_owned(), "/home/gui".to_owned())];
            vars.extend(codex.map(|c| ("CODEX_HOME".to_owned(), c.to_owned())));
            vars
        };
        assert_eq!(
            child_homes(&env(&[], &[]), app(None)),
            ChildHomes {
                home: Some("/home/gui".into()),
                codex: Some("/home/gui/.codex".into())
            }
        );
        assert_eq!(
            child_homes(&env(&[], &[]), app(Some("/opt/gui-codex"))).codex,
            Some("/opt/gui-codex".into())
        );
        assert_eq!(
            child_homes(
                &env(&[("HOME", "/home/p"), ("CODEX_HOME", "/p/codex")], &[]),
                app(Some("/opt/gui-codex"))
            ),
            ChildHomes {
                home: Some("/home/p".into()),
                codex: Some("/p/codex".into())
            }
        );
        // clear_env removes the inherited value: back to the default under the child's home.
        assert_eq!(
            child_homes(&env(&[], &["CODEX_HOME"]), app(Some("/opt/gui-codex"))).codex,
            Some("/home/gui/.codex".into())
        );
        // Relative, `..` or a missing home: unknown, never guessed.
        for codex in ["rel/codex", "/opt/x/../codex"] {
            assert_eq!(
                child_homes(&env(&[("CODEX_HOME", codex)], &[]), app(None)).codex,
                None,
                "{codex}"
            );
        }
        assert_eq!(
            child_homes(&env(&[], &["HOME"]), app(None)),
            ChildHomes {
                home: None,
                codex: None
            }
        );
        assert_eq!(
            child_homes(&env(&[("HOME", "")], &[]), Vec::new()).home,
            None
        );
    }

    fn check(input: Value, project: &Path, home: &Path) -> Check {
        Check {
            input,
            workspace: project.to_owned(),
            homes: homes(home),
            project_writable: false,
            granted: Vec::new(),
        }
    }

    /// Red-team #400: a hard-linked file names the same data as another path.
    #[test]
    fn a_hard_linked_file_is_turn_only() {
        let (_dir, home, project) = tree();
        let file = project.join("src/data.txt");
        std::fs::write(&file, "x").expect("file");
        let plain = judge(&one("read", path(&file)), &project, &homes(&home));
        assert!(plain.session_allowed, "{plain:?}");
        std::fs::hard_link(&file, home.join("other-name")).expect("hard link");
        let v = judge(&one("read", path(&file)), &project, &homes(&home));
        assert!(!v.session_allowed);
        assert!(v.warning.is_some_and(|w| w.contains(LINKED_NOTE)));
        assert_ne!(
            plain.marks, v.marks,
            "the link count is part of the verdict"
        );
    }

    /// Red-team #399: Codex re-resolves a granted name on every later command, so a name in a
    /// folder Codex may write (Accept edits in the project, or an earlier session grant) could
    /// be swapped for a link later.
    #[test]
    fn a_name_codex_could_rename_is_turn_only() {
        let (_dir, home, project) = tree();
        let target = one("write", path(&project.join("src/gen")));
        assert!(judge_check(&check(target.clone(), &project, &home)).session_allowed);
        let writable = Check {
            project_writable: true,
            ..check(target.clone(), &project, &home)
        };
        let v = judge_check(&writable);
        assert!(!v.session_allowed);
        assert_eq!(v.warning.as_deref(), Some(RENAME_NOTE));
        let granted = Check {
            granted: vec![real(&project.join("src")).expect("real")],
            ..check(target, &project, &home)
        };
        assert!(!judge_check(&granted).session_allowed);
        // Network only stays session-grantable in either case.
        let network = Check {
            project_writable: true,
            ..check(card(json!([])), &project, &home)
        };
        assert!(judge_check(&network).session_allowed);
    }

    /// A literal path that leads elsewhere is granted by where it leads.
    #[test]
    fn a_path_through_a_link_is_renamed_to_where_it_leads() {
        let (_dir, home, project) = tree();
        std::os::unix::fs::symlink(project.join("src"), project.join("alias")).expect("link");
        let v = judge(
            &one("write", path(&project.join("alias/x"))),
            &project,
            &homes(&home),
        );
        let real_x = real(&project.join("src/x")).expect("real");
        assert_eq!(
            v.renames,
            [(
                project.join("alias/x").to_string_lossy().into_owned(),
                real_x.to_string_lossy().into_owned()
            )]
        );
        assert_eq!(v.writes, [real_x]);
    }

    /// A folder's link count changes with its sub-folders: it is not part of the verdict, so a
    /// sub-folder made in a granted folder does not count as the place changing.
    #[test]
    fn a_new_sub_folder_does_not_change_a_folders_verdict() {
        let (_dir, home, project) = tree();
        let build = project.join("build");
        std::fs::create_dir_all(&build).expect("build");
        let card = one("write", path(&build));
        let before = judge(&card, &project, &homes(&home));
        assert!(before.session_allowed, "{before:?}");
        std::fs::create_dir_all(build.join("sub")).expect("sub");
        assert_eq!(judge(&card, &project, &homes(&home)), before);
        assert!(matches!(before.marks[0], Some((_, _, 0))));
    }

    /// Codex peer check: a write on the same card lets Codex replace the names below it, so
    /// those names are turn-only even in Ask mode with no earlier grant.
    #[test]
    fn a_write_on_the_same_card_makes_the_names_below_it_turn_only() {
        let (_dir, home, project) = tree();
        let h = homes(&home);
        let sub = project.join("src");
        let file = sub.join("main.rs");
        std::fs::write(&file, "x").expect("file");
        let entry = |access: &str, target: Value| json!({"access": access, "target": target});
        // The folder alone is judged as before.
        assert!(judge(&one("write", path(&sub)), &project, &h).session_allowed);
        for (entries, why) in [
            (
                json!([entry("write", path(&sub)), entry("read", path(&file))]),
                "a write of the folder holding a read name",
            ),
            (
                json!([entry("write", path(&sub)), entry("write", path(&file))]),
                "a write of the folder holding a written name",
            ),
            (
                json!([
                    entry(
                        "write",
                        json!({"kind": "glob", "pattern": format!("{}/**", project.display())})
                    ),
                    entry("read", path(&file)),
                ]),
                "a write glob above the name",
            ),
            (
                json!([
                    entry(
                        "write",
                        json!({"kind": "special", "name": "temp", "path": null})
                    ),
                    entry("read", path(&file)),
                ]),
                "a write to a place with no known folder",
            ),
        ] {
            let v = judge(&card(entries), &project, &h);
            assert!(!v.session_allowed, "{why}");
            assert!(
                v.warning
                    .as_deref()
                    .is_some_and(|w| w.contains(RENAME_NOTE)),
                "{why}: {v:?}"
            );
        }
        // A read of the folder does not cover the names below it.
        let v = judge(
            &card(json!([
                entry("read", path(&sub)),
                entry("read", path(&file))
            ])),
            &project,
            &h,
        );
        assert!(v.session_allowed, "{v:?}");
    }
}
