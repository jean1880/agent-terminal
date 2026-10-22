//! Removing what agent-terminal put on this machine: its own folders, the entries it added to
//! other tools' configuration, its checkpoint refs in the repositories its threads ran in, and
//! the program itself. [`plan`] only looks; [`apply`] removes. Blocking I/O: run it off the main
//! thread, and only once the app has stopped (its store writer would recreate what is deleted).
//!
//! Never touched: worktrees and branches (they hold the user's work), and agy's `hooks.json`,
//! whose approval-hook entry the user pasted in themselves ([`Plan::notes`] says it is there).

use std::collections::BTreeSet;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};

use crate::delegation_config::{self, Removal};
use crate::git::{self, QUERY_TIMEOUT_SECS, REF_ROOT};

/// The app's folder under each XDG base directory.
pub const APP_DIR: &str = "agent-terminal";
/// The pre-2.0 configuration folder, kept by the 2.0 migration.
pub const LEGACY_CONFIG_DIR: &str = "antigravity-terminal";
/// The application ID: the desktop entry and icon names.
pub const APP_ID: &str = "ca.nuvek.AgentTerminal";
/// The package name in every distribution's package.
pub const PACKAGE: &str = "agent-terminal";
/// The binary's name.
pub const BINARY: &str = "agent-terminal";
/// The agy registration's recovery folder, under a profile's home.
const DELEGATION_STATE: &str = ".config/agent-terminal/delegation";
/// How long a package manager has to say who owns the binary.
const OWNER_TIMEOUT_SECS: u64 = 10;

/// What the plan is made from. The caller reads the environment and the profiles; nothing here
/// does, so tests never see the real home.
#[derive(Debug, Clone, Default)]
pub struct Inputs {
    /// The user's home folder (absolute).
    pub home: PathBuf,
    /// The values of `XDG_CONFIG_HOME`, `XDG_STATE_HOME`, `XDG_CACHE_HOME`, `XDG_DATA_HOME` and
    /// `XDG_RUNTIME_DIR`, as the app sees them.
    pub xdg_config: Option<String>,
    pub xdg_state: Option<String>,
    pub xdg_cache: Option<String>,
    pub xdg_data: Option<String>,
    pub xdg_runtime: Option<String>,
    /// The homes agy runs in (the user's, and any an agy profile sets): each may hold the
    /// managed registration.
    pub agy_homes: Vec<PathBuf>,
    /// The Codex homes the Codex profiles use: each may hold the exported rules file.
    pub codex_homes: Vec<PathBuf>,
    /// The running executable, to tell how the app was installed.
    pub exe: Option<PathBuf>,
    /// The uid that must own every folder removed.
    pub uid: u32,
}

/// One thing the uninstall removes with the user's data.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Item {
    /// One of the app's own folders, removed whole.
    Folder { path: PathBuf, bytes: u64 },
    /// The managed agy registration under this home: the `agent_terminal` MCP server and the
    /// allow rules the app added, through the registration's own journalled Remove.
    AgyRegistration { home: PathBuf },
    /// The Codex rules file the app's export writes, whole.
    CodexRules { file: PathBuf },
    /// Checkpoint refs (`refs/agent-terminal/*`) and the private index folder in a repository.
    Checkpoints {
        toplevel: PathBuf,
        refs: Vec<String>,
        scratch: Option<PathBuf>,
    },
}

impl Item {
    /// One line for a list, with `home` written as `~`.
    pub fn describe(&self, home: &Path) -> String {
        match self {
            Item::Folder { path, bytes } => {
                format!("{} ({})", tilde(path, home), human_size(*bytes))
            }
            Item::AgyRegistration { home: agy } => format!(
                "Antigravity: the agent_terminal MCP server and the allow rules Agent Terminal \
                 added, in {}",
                tilde(&agy.join(".gemini"), home)
            ),
            Item::CodexRules { file } => format!("Codex: {}", tilde(file, home)),
            Item::Checkpoints { toplevel, refs, .. } => format!(
                "Checkpoints in {}: {} hidden ref{}",
                tilde(toplevel, home),
                refs.len(),
                if refs.len() == 1 { "" } else { "s" }
            ),
        }
    }
}

/// How the program itself is installed, and so how it is removed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Install {
    /// `make install`: these files under `~/.local`.
    Local { files: Vec<PathBuf> },
    /// A distribution package.
    Package { manager: Manager, name: String },
    /// Neither (a build run from its tree): the program is left where it is.
    Unmanaged { exe: PathBuf },
}

/// The package manager that owns the binary.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Manager {
    Apt,
    Dnf,
    Rpm,
    Pacman,
}

/// How a removal command gets root.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Elevation {
    /// Already root (a container).
    None,
    /// `sudo`, asking on the terminal.
    Sudo,
    /// `pkexec`, asking through polkit's dialog (no terminal).
    Pkexec,
}

impl Install {
    /// The command that removes the program, `None` when it is not a package (a local install is
    /// removed file by file, an unmanaged one not at all). Non-interactive: the user has already
    /// confirmed, and `pkexec` has no terminal to answer a prompt on. The name passed
    /// [`valid_package`] (no leading `-`), so no `--` is needed, and not every manager takes one.
    pub fn removal_command(&self, elevation: Elevation) -> Option<Vec<String>> {
        let Install::Package { manager, name } = self else {
            return None;
        };
        let mut argv: Vec<String> = match elevation {
            Elevation::None => vec![],
            Elevation::Sudo => vec!["sudo".into()],
            Elevation::Pkexec => vec!["pkexec".into()],
        };
        let tail: &[&str] = match manager {
            Manager::Apt => &["apt-get", "remove", "-y"],
            Manager::Dnf => &["dnf", "remove", "-y"],
            Manager::Rpm => &["rpm", "-e"],
            Manager::Pacman => &["pacman", "-R", "--noconfirm"],
        };
        argv.extend(tail.iter().map(|s| (*s).to_owned()));
        argv.push(name.clone());
        Some(argv)
    }

    /// One line for the summary.
    pub fn describe(&self, home: &Path) -> String {
        match self {
            Install::Local { .. } => format!(
                "The program installed by make install, in {}",
                tilde(&home.join(".local"), home)
            ),
            Install::Package { manager, name } => {
                let tool = match manager {
                    Manager::Apt => "apt",
                    Manager::Dnf => "dnf",
                    Manager::Rpm => "rpm",
                    Manager::Pacman => "pacman",
                };
                format!("The {name} package ({tool})")
            }
            Install::Unmanaged { exe } => format!(
                "Nothing: {} was not installed by a package or make install, so it stays",
                tilde(exe, home)
            ),
        }
    }
}

/// What an uninstall would remove.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Plan {
    pub install: Install,
    /// Removed only when the user chooses to remove their data.
    pub items: Vec<Item>,
    /// Things left in place that the user may want to know about.
    pub notes: Vec<String>,
    /// Another installed copy of the app that stays and uses the same configuration and data
    /// (they are per user, not per copy). When set, keeping the data is the default answer.
    pub shared_with: Option<PathBuf>,
}

/// The other copy that stays installed and shares the user's data: the package's binary when a
/// `make install` copy is removed, and the reverse. `exists` is passed in so tests need neither.
pub fn other_copy(
    install: &Install,
    home: &Path,
    exists: impl Fn(&Path) -> bool,
) -> Option<PathBuf> {
    let packaged = Path::new("/usr/bin").join(BINARY);
    let local = home.join(".local/bin").join(BINARY);
    match install {
        Install::Local { .. } => Some(packaged),
        Install::Package { .. } => Some(local),
        Install::Unmanaged { .. } => None,
    }
    .filter(|p| exists(p))
}

impl Plan {
    /// The bytes the folders hold.
    pub fn bytes(&self) -> u64 {
        self.items
            .iter()
            .map(|i| match i {
                Item::Folder { bytes, .. } => *bytes,
                _ => 0,
            })
            .sum()
    }
}

/// What [`apply`] did, item by item.
#[derive(Debug, Default)]
pub struct Report {
    pub done: Vec<String>,
    pub failed: Vec<String>,
}

/// The base directory for one XDG variable: its value when absolute (relative and empty values
/// are ignored, per the spec), else `home/<fallback>`. Both are returned when they differ, so data
/// written under an override that is no longer set is found too.
fn bases(value: Option<&str>, home: &Path, fallback: &str) -> Vec<PathBuf> {
    let mut out = vec![];
    if let Some(v) = value.filter(|v| Path::new(v).is_absolute()) {
        out.push(PathBuf::from(v));
    }
    let default = home.join(fallback);
    if !out.contains(&default) {
        out.push(default);
    }
    out
}

/// Every folder of the app's that could exist for these inputs, existing or not.
pub fn candidate_folders(inputs: &Inputs) -> Vec<PathBuf> {
    let home = inputs.home.as_path();
    let mut out = vec![];
    for base in bases(inputs.xdg_config.as_deref(), home, ".config") {
        out.push(base.join(APP_DIR));
        out.push(base.join(LEGACY_CONFIG_DIR));
    }
    for (value, fallback) in [
        (inputs.xdg_state.as_deref(), ".local/state"),
        (inputs.xdg_cache.as_deref(), ".cache"),
        (inputs.xdg_data.as_deref(), ".local/share"),
    ] {
        out.extend(
            bases(value, home, fallback)
                .into_iter()
                .map(|b| b.join(APP_DIR)),
        );
    }
    if let Some(runtime) = inputs
        .xdg_runtime
        .as_deref()
        .filter(|v| Path::new(v).is_absolute())
    {
        out.push(Path::new(runtime).join(APP_DIR));
    }
    // An agy profile with a home of its own keeps its registration journal there.
    for agy in &inputs.agy_homes {
        let state = agy.join(DELEGATION_STATE);
        if !out.iter().any(|f| state.starts_with(f)) {
            out.push(state);
        }
    }
    let mut seen = BTreeSet::new();
    out.retain(|p| seen.insert(p.clone()));
    out
}

/// Whether `path` may be removed whole: a real folder (never a link), owned by `uid`, named as
/// one of the app's own. `Err` says why not.
pub fn removable_folder(path: &Path, uid: u32) -> Result<(), String> {
    let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("");
    let parent_ours = path
        .parent()
        .and_then(Path::file_name)
        .is_some_and(|n| n == APP_DIR);
    if !(name == APP_DIR || name == LEGACY_CONFIG_DIR || (name == "delegation" && parent_ours)) {
        return Err(format!(
            "{} is not one of Agent Terminal's folders",
            path.display()
        ));
    }
    let meta = std::fs::symlink_metadata(path)
        .map_err(|e| format!("cannot read {}: {e}", path.display()))?;
    if meta.file_type().is_symlink() || !meta.is_dir() {
        return Err(format!(
            "{} is not a folder (a link is never followed)",
            path.display()
        ));
    }
    if meta.uid() != uid {
        return Err(format!("{} belongs to another user", path.display()));
    }
    Ok(())
}

/// The bytes under `path`, never following a link.
fn folder_bytes(path: &Path) -> u64 {
    let Ok(entries) = std::fs::read_dir(path) else {
        return 0;
    };
    entries
        .flatten()
        .map(|entry| match entry.file_type() {
            Ok(t) if t.is_dir() => folder_bytes(&entry.path()),
            Ok(t) if t.is_file() => entry.metadata().map(|m| m.len()).unwrap_or(0),
            _ => 0,
        })
        .sum()
}

/// The thread store's file under these inputs (`<state>/agent-terminal/threads.db`), for each
/// state base that has one.
pub fn store_files(inputs: &Inputs) -> Vec<PathBuf> {
    bases(inputs.xdg_state.as_deref(), &inputs.home, ".local/state")
        .into_iter()
        .map(|b| b.join(APP_DIR).join("threads.db"))
        .filter(|p| p.is_file())
        .collect()
}

/// The folders threads ran in, read from the store without changing it (read-only, no
/// migration). `Err` when the store cannot be read.
pub fn thread_folders(store: &Path) -> Result<Vec<PathBuf>, String> {
    use rusqlite::{Connection, OpenFlags};
    let conn = Connection::open_with_flags(
        store,
        OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )
    .map_err(|e| format!("cannot open {}: {e}", store.display()))?;
    let mut stmt = conn
        .prepare("SELECT DISTINCT cwd FROM threads")
        .map_err(|e| format!("cannot read the threads: {e}"))?;
    let rows = stmt
        .query_map([], |row| row.get::<_, String>(0))
        .map_err(|e| format!("cannot read the threads: {e}"))?;
    Ok(rows.flatten().map(PathBuf::from).collect())
}

/// The checkpoint refs and private index folder in the repository `dir` is in, if it has any.
fn checkpoints_in(dir: &Path) -> Option<(PathBuf, PathBuf, Item)> {
    let repo = git::discover(dir).ok().flatten()?;
    let common = git::git_raw(
        &repo.toplevel,
        ["rev-parse", "--path-format=absolute", "--git-common-dir"],
        &[],
        QUERY_TIMEOUT_SECS,
    )
    .ok()
    .map(|o| PathBuf::from(String::from_utf8_lossy(&o).trim()))
    .unwrap_or_else(|| repo.git_dir.clone());
    let refs = git::git_raw(
        &repo.toplevel,
        ["for-each-ref", "--format=%(refname)", REF_ROOT],
        &[],
        QUERY_TIMEOUT_SECS,
    )
    .map(|o| {
        String::from_utf8_lossy(&o)
            .lines()
            .filter(|r| r.starts_with(&format!("{REF_ROOT}/")))
            .map(str::to_owned)
            .collect::<Vec<_>>()
    })
    .unwrap_or_default();
    let scratch = repo.git_dir.join(APP_DIR);
    let scratch = std::fs::symlink_metadata(&scratch)
        .is_ok_and(|m| m.is_dir())
        .then_some(scratch);
    Some((
        common,
        repo.git_dir.clone(),
        Item::Checkpoints {
            toplevel: repo.toplevel,
            refs,
            scratch,
        },
    ))
}

/// The running executable's path as a package manager knows it: Linux names a binary replaced
/// while it ran (an upgrade) `<path> (deleted)`, which no manager owns.
pub fn installed_path(exe: &Path) -> PathBuf {
    let text = exe.to_string_lossy();
    match text.strip_suffix(" (deleted)") {
        Some(path) => PathBuf::from(path),
        None => exe.to_path_buf(),
    }
}

/// Finds the program's install by asking each package manager who owns `exe`.
pub fn detect_install(exe: &Path, home: &Path) -> Install {
    let exe = &installed_path(exe);
    let local = home.join(".local/bin").join(BINARY);
    // `current_exe` is fully resolved; the home may be reached through a link.
    let same = |a: &Path, b: &Path| {
        a == b
            || matches!(
                (std::fs::canonicalize(a), std::fs::canonicalize(b)),
                (Ok(a), Ok(b)) if a == b
            )
    };
    if same(exe, &local) {
        return Install::Local {
            files: local_files(home),
        };
    }
    let owned = |argv: &[&str]| -> Option<String> {
        let mut cmd = std::process::Command::new(argv[0]);
        cmd.args(&argv[1..]).arg(exe);
        crate::exec::run_command(cmd, argv[0], OWNER_TIMEOUT_SECS)
            .ok()
            .filter(|o| o.status.success())
            .map(|o| String::from_utf8_lossy(&o.stdout).into_owned())
    };
    // `exe` is absolute, so it cannot be taken for an option.
    if let Some(name) = owned(&["dpkg-query", "-S"]).and_then(|o| parse_dpkg_owner(&o)) {
        return Install::Package {
            manager: Manager::Apt,
            name,
        };
    }
    if let Some(name) = owned(&["rpm", "-qf", "--qf", "%{NAME}\\n"]).and_then(|o| first_word(&o)) {
        let manager = if on_path("dnf") {
            Manager::Dnf
        } else {
            Manager::Rpm
        };
        return Install::Package { manager, name };
    }
    if let Some(name) = owned(&["pacman", "-Qqo"]).and_then(|o| first_word(&o)) {
        return Install::Package {
            manager: Manager::Pacman,
            name,
        };
    }
    Install::Unmanaged {
        exe: exe.to_path_buf(),
    }
}

/// Whether `cmdline` (`/proc/<pid>/cmdline`: NUL-separated) runs the app itself rather than one
/// of its short-lived helpers (agy's approval hook, the MCP bridge) or another uninstall.
pub fn is_app_instance(cmdline: &[u8]) -> bool {
    use std::os::unix::ffi::OsStrExt;
    let mut args = cmdline.split(|b| *b == 0).filter(|a| !a.is_empty());
    let Some(argv0) = args.next() else {
        return false;
    };
    if Path::new(std::ffi::OsStr::from_bytes(argv0)).file_name()
        != Some(std::ffi::OsStr::new(BINARY))
    {
        return false;
    }
    !matches!(
        args.next(),
        Some(b"--approval-hook" | b"--cross-agent-mcp" | b"--uninstall")
    )
}

/// How many processes of `uid`, other than this one, run the app ([`is_app_instance`]). Finds an
/// instance on another session bus (a desktop seen from an SSH or console login), which a D-Bus
/// name check cannot. Reads `/proc`; 0 when it cannot.
pub fn other_instances(uid: u32) -> usize {
    let me = std::process::id().to_string();
    let Ok(entries) = std::fs::read_dir("/proc") else {
        return 0;
    };
    entries
        .flatten()
        .filter(|e| {
            let name = e.file_name();
            let name = name.to_string_lossy();
            name != me.as_str() && name.bytes().all(|b| b.is_ascii_digit())
        })
        .filter(|e| e.metadata().is_ok_and(|m| m.uid() == uid))
        .filter(|e| std::fs::read(e.path().join("cmdline")).is_ok_and(|c| is_app_instance(&c)))
        .count()
}

/// The files `make install` puts under `home`.
pub fn local_files(home: &Path) -> Vec<PathBuf> {
    vec![
        home.join(".local/bin").join(BINARY),
        home.join(".local/share/applications")
            .join(format!("{APP_ID}.desktop")),
        home.join(".local/share/icons/hicolor/scalable/apps")
            .join(format!("{APP_ID}.svg")),
    ]
}

/// The package in `dpkg-query -S` output (`name[:arch]: /path`, possibly `diversion by …`
/// lines first).
pub fn parse_dpkg_owner(out: &str) -> Option<String> {
    out.lines()
        .filter(|l| !l.starts_with("diversion "))
        .find_map(|l| {
            l.split_once(": ")
                .filter(|(_, path)| path.starts_with('/'))
                .map(|(pkgs, _)| pkgs)
        })
        .and_then(|pkgs| pkgs.split(", ").next())
        .map(|p| p.split(':').next().unwrap_or(p).trim().to_owned())
        .filter(|p| valid_package(p))
}

fn first_word(out: &str) -> Option<String> {
    out.split_whitespace()
        .next()
        .map(str::to_owned)
        .filter(|p| valid_package(p))
}

/// A package name as the managers write them; anything else (an error message) is refused.
fn valid_package(name: &str) -> bool {
    !name.is_empty()
        && !name.starts_with('-')
        && name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"+-._".contains(&b))
}

fn on_path(program: &str) -> bool {
    std::env::var_os("PATH")
        .is_some_and(|path| std::env::split_paths(&path).any(|dir| dir.join(program).is_file()))
}

/// Looks at what is there. The repositories come from the thread store's folders, read before
/// anything is removed.
pub fn plan(inputs: &Inputs) -> Plan {
    let mut items = vec![];
    let mut notes = vec![];

    for agy in &inputs.agy_homes {
        if agy
            .join(DELEGATION_STATE)
            .join("delegation-registration.json")
            .is_file()
        {
            items.push(Item::AgyRegistration { home: agy.clone() });
        }
        let hooks = agy.join(".gemini/config/hooks.json");
        if std::fs::read_to_string(&hooks).is_ok_and(|t| t.contains("--approval-hook")) {
            notes.push(format!(
                "{} still has the approval-hook entry you added. It does nothing without Agent \
                 Terminal; remove it yourself if you like.",
                tilde(&hooks, &inputs.home)
            ));
        }
    }

    for codex in &inputs.codex_homes {
        let file = codex
            .join("rules")
            .join(agent_core::codex_rules::EXPORT_FILE);
        match std::fs::symlink_metadata(&file) {
            Ok(m) if m.is_file() => {
                let ours = std::fs::read_to_string(&file)
                    .is_ok_and(|t| t.lines().next() == Some(agent_core::codex_rules::HEADER));
                if ours {
                    items.push(Item::CodexRules { file });
                } else {
                    notes.push(format!(
                        "{} was edited after Agent Terminal wrote it, so it stays.",
                        tilde(&file, &inputs.home)
                    ));
                }
            }
            _ => {}
        }
    }

    let mut repos: Vec<Item> = vec![];
    let mut seen_common = BTreeSet::new();
    let mut seen_git_dir = BTreeSet::new();
    let mut seen_dir = BTreeSet::new();
    for store in store_files(inputs) {
        match thread_folders(&store) {
            Ok(dirs) => {
                for dir in dirs {
                    if !seen_dir.insert(dir.clone()) || !dir.is_dir() {
                        continue;
                    }
                    let Some((common, git_dir, item)) = checkpoints_in(&dir) else {
                        continue;
                    };
                    let Item::Checkpoints {
                        toplevel,
                        refs,
                        scratch,
                    } = item
                    else {
                        continue;
                    };
                    // Refs live in the common git folder, shared by linked worktrees.
                    let refs = if seen_common.insert(common) {
                        refs
                    } else {
                        vec![]
                    };
                    let scratch = scratch.filter(|_| seen_git_dir.insert(git_dir));
                    if !refs.is_empty() || scratch.is_some() {
                        repos.push(Item::Checkpoints {
                            toplevel,
                            refs,
                            scratch,
                        });
                    }
                }
            }
            Err(e) => notes.push(format!(
                "The thread list could not be read ({e}), so checkpoints in your repositories \
                 are not removed."
            )),
        }
    }
    items.extend(repos);

    for path in candidate_folders(inputs) {
        if removable_folder(&path, inputs.uid).is_ok() {
            let bytes = folder_bytes(&path);
            items.push(Item::Folder { path, bytes });
        } else if std::fs::symlink_metadata(&path).is_ok() {
            notes.push(format!(
                "{} stays: it is a link or belongs to another user.",
                tilde(&path, &inputs.home)
            ));
        }
    }

    let install = match &inputs.exe {
        Some(exe) => detect_install(exe, &inputs.home),
        None => Install::Unmanaged {
            exe: PathBuf::from(BINARY),
        },
    };
    // The data is per user, not per copy: removing it with one copy removes the other's threads
    // and settings too.
    let shared_with = other_copy(&install, &inputs.home, Path::exists);
    if let Some(other) = &shared_with {
        notes.push(format!(
            "{} stays installed and uses the same configuration and data, so keeping them is \
             the default.",
            tilde(other, &inputs.home)
        ));
    }
    Plan {
        install,
        items,
        notes,
        shared_with,
    }
}

/// Removes the plan's items, in an order that keeps what a later step needs: registrations and
/// rules first (their journal is in a folder removed last), then checkpoints, then folders.
/// Every item is attempted; failures are reported, not fatal. A registration that could not be
/// reverted keeps the folder holding its journal: without it, the entries could never again be
/// told apart from the user's own.
pub fn apply(plan: &Plan, uid: u32, home: &Path) -> Report {
    let mut report = Report::default();
    let mut journals_kept: Vec<PathBuf> = vec![];
    let mut record = |what: String, result: Result<(), String>| match result {
        Ok(()) => report.done.push(what),
        Err(e) => report.failed.push(format!("{what}: {e}")),
    };
    for item in &plan.items {
        let what = item.describe(home);
        match item {
            Item::AgyRegistration { home: agy } => {
                // The registration checks its files against the resolved home.
                let agy = std::fs::canonicalize(agy).unwrap_or_else(|_| agy.clone());
                let state = agy.join(DELEGATION_STATE);
                let result = match delegation_config::remove_owned(&agy, &state) {
                    Ok(Removal::Removed | Removal::NothingToRemove) => Ok(()),
                    Ok(Removal::NotOurs) => Err("not made by Agent Terminal; left alone".into()),
                    Err(e) => Err(e),
                };
                if result.is_err() {
                    journals_kept.push(state);
                }
                record(what, result);
            }
            Item::CodexRules { file } => {
                record(what, remove_file(file));
            }
            Item::Checkpoints {
                toplevel,
                refs,
                scratch,
            } => {
                let mut result = refs.iter().try_for_each(|r| delete_ref(toplevel, r));
                if let (Ok(()), Some(dir)) = (&result, scratch) {
                    result = remove_scratch(dir);
                }
                record(what, result);
            }
            Item::Folder { .. } => {}
        }
    }
    for item in &plan.items {
        if let Item::Folder { path, .. } = item {
            let resolved = std::fs::canonicalize(path).unwrap_or_else(|_| path.clone());
            if journals_kept
                .iter()
                .any(|state| state.starts_with(&resolved) || state.starts_with(path))
            {
                record(
                    item.describe(home),
                    Err(
                        "kept: it holds the record of the Antigravity registration that could \
                         not be removed; run the uninstall again once that is fixed"
                            .into(),
                    ),
                );
                continue;
            }
            let result = removable_folder(path, uid).and_then(|()| {
                std::fs::remove_dir_all(path)
                    .map_err(|e| format!("cannot remove {}: {e}", path.display()))
            });
            // A profile home's `.config/agent-terminal` held only the registration journal.
            if result.is_ok() && path.ends_with("delegation") {
                if let Some(parent) = path.parent() {
                    let _ = std::fs::remove_dir(parent);
                }
            }
            record(item.describe(home), result);
        }
    }
    report
}

/// Removes the files of a `make install`, then refreshes the desktop's caches (best effort).
pub fn remove_local(files: &[PathBuf], home: &Path) -> Report {
    let mut report = Report::default();
    for file in files {
        match remove_file(file) {
            Ok(()) => report.done.push(tilde(file, home)),
            Err(e) => report.failed.push(e),
        }
    }
    for argv in [
        vec![
            "update-desktop-database".to_owned(),
            home.join(".local/share/applications")
                .to_string_lossy()
                .into_owned(),
        ],
        vec![
            "gtk4-update-icon-cache".to_owned(),
            "-q".to_owned(),
            "-t".to_owned(),
            "-f".to_owned(),
            home.join(".local/share/icons/hicolor")
                .to_string_lossy()
                .into_owned(),
        ],
    ] {
        let mut cmd = std::process::Command::new(&argv[0]);
        cmd.args(&argv[1..]);
        let _ = crate::exec::run_command(cmd, &argv[0], OWNER_TIMEOUT_SECS);
    }
    report
}

/// Removes a file (never a folder; a link is removed, not followed). Gone already is success.
fn remove_file(path: &Path) -> Result<(), String> {
    match std::fs::symlink_metadata(path) {
        Ok(m) if m.is_dir() => Err(format!("{} is a folder", path.display())),
        Ok(_) => {
            std::fs::remove_file(path).map_err(|e| format!("cannot remove {}: {e}", path.display()))
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(format!("cannot read {}: {e}", path.display())),
    }
}

/// Deletes one checkpoint ref. Only names under [`REF_ROOT`]; gone already is success.
fn delete_ref(toplevel: &Path, refname: &str) -> Result<(), String> {
    if !refname.starts_with(&format!("{REF_ROOT}/")) {
        return Err(format!("{refname} is not a checkpoint ref"));
    }
    let output = git::git_output(
        toplevel,
        // `--no-deref`: a symbolic ref here must never take the branch it points at with it.
        ["update-ref", "--no-deref", "-d", "--", refname],
        &[],
        QUERY_TIMEOUT_SECS,
    )?;
    if output.status.success() {
        Ok(())
    } else {
        Err(String::from_utf8_lossy(&output.stderr).trim().to_owned())
    }
}

/// Removes `<git dir>/agent-terminal`, the checkpoints' private index folder, unless it is a
/// link.
fn remove_scratch(dir: &Path) -> Result<(), String> {
    match std::fs::symlink_metadata(dir) {
        Ok(m) if m.is_dir() && !m.file_type().is_symlink() => std::fs::remove_dir_all(dir)
            .map_err(|e| format!("cannot remove {}: {e}", dir.display())),
        Ok(_) => Err(format!("{} is not a folder", dir.display())),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(format!("cannot read {}: {e}", dir.display())),
    }
}

/// `path` with `home` written as `~`.
pub fn tilde(path: &Path, home: &Path) -> String {
    match path.strip_prefix(home) {
        Ok(rest) if !home.as_os_str().is_empty() => {
            if rest.as_os_str().is_empty() {
                "~".to_owned()
            } else {
                format!("~/{}", rest.display())
            }
        }
        _ => path.display().to_string(),
    }
}

/// A byte count for people: `0 B`, `12 KB`, `1.6 GB`.
pub fn human_size(bytes: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KB", "MB", "GB", "TB"];
    let mut value = bytes as f64;
    let mut unit = 0;
    while value >= 1000.0 && unit < UNITS.len() - 1 {
        value /= 1000.0;
        unit += 1;
    }
    if unit == 0 || value >= 10.0 {
        format!("{value:.0} {}", UNITS[unit])
    } else {
        format!("{value:.1} {}", UNITS[unit])
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn uid() -> u32 {
        // SAFETY: geteuid has no preconditions and cannot fail.
        unsafe { libc::geteuid() }
    }

    fn inputs(home: &Path) -> Inputs {
        Inputs {
            home: home.to_path_buf(),
            agy_homes: vec![home.to_path_buf()],
            codex_homes: vec![home.join(".codex")],
            uid: uid(),
            ..Inputs::default()
        }
    }

    #[test]
    fn candidates_cover_every_base_with_and_without_overrides() {
        let home = Path::new("/h");
        let mut i = inputs(home);
        i.xdg_config = Some("/x/config".into());
        i.xdg_state = Some("relative/state".into());
        i.xdg_runtime = Some("/run/user/1".into());
        let folders = candidate_folders(&i);
        for expected in [
            "/x/config/agent-terminal",
            "/x/config/antigravity-terminal",
            "/h/.config/agent-terminal",
            "/h/.config/antigravity-terminal",
            "/h/.local/state/agent-terminal",
            "/h/.cache/agent-terminal",
            "/h/.local/share/agent-terminal",
            "/run/user/1/agent-terminal",
        ] {
            assert!(folders.contains(&PathBuf::from(expected)), "{expected}");
        }
        // A relative override is not followed: it would name another folder from each place
        // the uninstall is run.
        assert!(!folders.iter().any(|f| f.starts_with("relative")));
        // The user's own home's registration journal is inside ~/.config/agent-terminal already.
        assert!(!folders.contains(&PathBuf::from("/h/.config/agent-terminal/delegation")));

        i.agy_homes.push(PathBuf::from("/p"));
        assert!(
            candidate_folders(&i).contains(&PathBuf::from("/p/.config/agent-terminal/delegation"))
        );
    }

    #[test]
    fn only_real_owned_app_folders_are_removable() {
        let tmp = tempfile::tempdir().unwrap();
        let ours = tmp.path().join(APP_DIR);
        fs::create_dir(&ours).unwrap();
        assert!(removable_folder(&ours, uid()).is_ok());
        assert!(removable_folder(&ours, uid() + 1).is_err());

        let other = tmp.path().join("something-else");
        fs::create_dir(&other).unwrap();
        assert!(removable_folder(&other, uid()).is_err());

        // A link named like ours is never followed, even to a folder of ours.
        let elsewhere = tmp.path().join("real");
        fs::create_dir_all(elsewhere.join(APP_DIR)).unwrap();
        let link_parent = tmp.path().join("links");
        fs::create_dir(&link_parent).unwrap();
        std::os::unix::fs::symlink(elsewhere.join(APP_DIR), link_parent.join(APP_DIR)).unwrap();
        assert!(removable_folder(&link_parent.join(APP_DIR), uid()).is_err());

        let delegation = ours.join("delegation");
        fs::create_dir(&delegation).unwrap();
        assert!(removable_folder(&delegation, uid()).is_ok());
        let stray = other.join("delegation");
        fs::create_dir(&stray).unwrap();
        assert!(removable_folder(&stray, uid()).is_err());
    }

    #[test]
    fn plan_and_apply_remove_the_app_folders_and_its_codex_rules_and_nothing_else() {
        let tmp = tempfile::tempdir().unwrap();
        let home = tmp.path();
        for dir in [
            ".config/agent-terminal",
            ".local/state/agent-terminal/handoffs",
            ".cache/agent-terminal",
            ".config/neighbour",
            ".codex/rules",
        ] {
            fs::create_dir_all(home.join(dir)).unwrap();
        }
        fs::write(home.join(".config/agent-terminal/config.json"), "{}").unwrap();
        fs::write(
            home.join(".local/state/agent-terminal/handoffs/b.md"),
            "brief",
        )
        .unwrap();
        fs::write(home.join(".config/neighbour/keep"), "x").unwrap();
        let rules = home
            .join(".codex/rules")
            .join(agent_core::codex_rules::EXPORT_FILE);
        fs::write(&rules, format!("{}\n", agent_core::codex_rules::HEADER)).unwrap();
        let users = home.join(".codex/rules/default.rules");
        fs::write(&users, "# mine\n").unwrap();
        // A hooks.json with the pasted entry is reported, never edited.
        fs::create_dir_all(home.join(".gemini/config")).unwrap();
        let hooks = home.join(".gemini/config/hooks.json");
        fs::write(&hooks, "{\"x\":\"agent-terminal --approval-hook\"}").unwrap();

        let plan = plan(&inputs(home));
        assert!(plan.items.contains(&Item::CodexRules {
            file: rules.clone()
        }));
        assert_eq!(
            plan.items
                .iter()
                .filter(|i| matches!(i, Item::Folder { .. }))
                .count(),
            3
        );
        assert!(plan.bytes() >= 7);
        assert!(plan.notes.iter().any(|n| n.contains("hooks.json")));

        let report = apply(&plan, uid(), home);
        assert!(report.failed.is_empty(), "{:?}", report.failed);
        assert!(!home.join(".config/agent-terminal").exists());
        assert!(!home.join(".local/state/agent-terminal").exists());
        assert!(!home.join(".cache/agent-terminal").exists());
        assert!(!rules.exists());
        assert!(users.exists());
        assert!(home.join(".config/neighbour/keep").exists());
        assert!(hooks.exists());
    }

    #[test]
    fn an_edited_codex_rules_file_stays() {
        let tmp = tempfile::tempdir().unwrap();
        let home = tmp.path();
        fs::create_dir_all(home.join(".codex/rules")).unwrap();
        let rules = home
            .join(".codex/rules")
            .join(agent_core::codex_rules::EXPORT_FILE);
        fs::write(&rules, "# my own now\n").unwrap();
        let plan = plan(&inputs(home));
        assert!(!plan
            .items
            .iter()
            .any(|i| matches!(i, Item::CodexRules { .. })));
        assert!(plan.notes.iter().any(|n| n.contains("edited")));
    }

    #[test]
    fn checkpoints_are_found_from_the_store_and_only_they_are_removed() {
        if !git::git_installed() {
            return;
        }
        let tmp = tempfile::tempdir().unwrap();
        let home = tmp.path().join("home");
        let repo = tmp.path().join("repo");
        fs::create_dir_all(&repo).unwrap();
        let sh = |args: &[&str]| {
            let out = std::process::Command::new("git")
                .args(["-c", "user.name=t", "-c", "user.email=t@t"])
                .args(args)
                .current_dir(&repo)
                .env("GIT_CONFIG_NOSYSTEM", "1")
                .env("GIT_CONFIG_GLOBAL", "/dev/null")
                .output()
                .unwrap();
            assert!(out.status.success(), "{args:?}");
            String::from_utf8_lossy(&out.stdout).trim().to_owned()
        };
        sh(&["init", "-q", "-b", "main"]);
        fs::write(repo.join("f"), "x").unwrap();
        sh(&["add", "f"]);
        sh(&["commit", "-q", "-m", "one"]);
        let head = sh(&["rev-parse", "HEAD"]);
        sh(&["update-ref", "refs/agent-terminal/1/0001", &head]);
        sh(&["update-ref", "refs/heads/agent-terminal/w_x", &head]);
        let git_dir = repo.join(".git");
        fs::create_dir_all(git_dir.join(APP_DIR)).unwrap();

        let state = home.join(".local/state/agent-terminal");
        fs::create_dir_all(&state).unwrap();
        let conn = rusqlite::Connection::open(state.join("threads.db")).unwrap();
        conn.execute_batch("CREATE TABLE threads (id TEXT PRIMARY KEY, cwd TEXT NOT NULL)")
            .unwrap();
        conn.execute(
            "INSERT INTO threads VALUES ('a', ?1), ('b', ?1), ('c', '/nowhere')",
            [repo.to_string_lossy()],
        )
        .unwrap();
        drop(conn);

        let plan = plan(&inputs(&home));
        let found: Vec<_> = plan
            .items
            .iter()
            .filter(|i| matches!(i, Item::Checkpoints { .. }))
            .collect();
        assert_eq!(found.len(), 1, "{found:?}");
        let report = apply(&plan, uid(), &home);
        assert!(report.failed.is_empty(), "{:?}", report.failed);
        assert_eq!(sh(&["for-each-ref", "refs/agent-terminal"]), "");
        assert!(!git_dir.join(APP_DIR).exists());
        // The user's branches, the sub-agent branch included, stay.
        assert!(!sh(&["for-each-ref", "refs/heads/agent-terminal/w_x"]).is_empty());
        assert!(!sh(&["for-each-ref", "refs/heads/main"]).is_empty());
    }

    #[test]
    fn removal_commands_are_fixed_argv_per_manager() {
        let package = |manager| Install::Package {
            manager,
            name: PACKAGE.into(),
        };
        assert_eq!(
            package(Manager::Apt)
                .removal_command(Elevation::Sudo)
                .unwrap(),
            ["sudo", "apt-get", "remove", "-y", "agent-terminal"]
        );
        assert_eq!(
            package(Manager::Dnf)
                .removal_command(Elevation::Pkexec)
                .unwrap(),
            ["pkexec", "dnf", "remove", "-y", "agent-terminal"]
        );
        assert_eq!(
            package(Manager::Rpm)
                .removal_command(Elevation::None)
                .unwrap(),
            ["rpm", "-e", "agent-terminal"]
        );
        assert_eq!(
            package(Manager::Pacman)
                .removal_command(Elevation::None)
                .unwrap(),
            ["pacman", "-R", "--noconfirm", "agent-terminal"]
        );
        assert!(Install::Local { files: vec![] }
            .removal_command(Elevation::Sudo)
            .is_none());
    }

    #[test]
    fn package_owners_are_parsed_and_errors_refused() {
        assert_eq!(
            parse_dpkg_owner("agent-terminal: /usr/bin/agent-terminal\n").as_deref(),
            Some("agent-terminal")
        );
        assert_eq!(
            parse_dpkg_owner("agent-terminal:amd64: /usr/bin/agent-terminal\n").as_deref(),
            Some("agent-terminal")
        );
        assert_eq!(
            parse_dpkg_owner(
                "diversion by x from: /usr/bin/agent-terminal\nagent-terminal: /usr/bin/agent-terminal\n"
            )
            .as_deref(),
            Some("agent-terminal")
        );
        assert_eq!(
            parse_dpkg_owner("dpkg-query: no path found matching pattern"),
            None
        );
        assert_eq!(
            first_word("agent-terminal\n").as_deref(),
            Some("agent-terminal")
        );
        assert_eq!(first_word("error: No package owns /x"), None);
    }

    #[test]
    fn only_the_app_itself_counts_as_a_running_instance() {
        assert!(is_app_instance(b"/usr/bin/agent-terminal\0"));
        assert!(is_app_instance(b"agent-terminal\0--resume\0abc\0"));
        for helper in [
            &b"/usr/bin/agent-terminal\0--approval-hook\0"[..],
            b"/usr/bin/agent-terminal\0--cross-agent-mcp\0",
            b"agent-terminal\0--uninstall\0--yes\0",
            b"/usr/bin/agent-terminal-other\0",
            b"bash\0-c\0agent-terminal\0",
            b"",
        ] {
            assert!(!is_app_instance(helper), "{helper:?}");
        }
        // This test process is not the app, and is never counted anyway.
        assert_eq!(other_instances(u32::MAX), 0);
    }

    #[test]
    fn the_other_copy_is_the_one_that_stays() {
        let home = Path::new("/h");
        let local = Install::Local { files: vec![] };
        let package = Install::Package {
            manager: Manager::Apt,
            name: PACKAGE.into(),
        };
        let everything = |_: &Path| true;
        assert_eq!(
            other_copy(&local, home, everything),
            Some(PathBuf::from("/usr/bin/agent-terminal"))
        );
        assert_eq!(
            other_copy(&package, home, everything),
            Some(PathBuf::from("/h/.local/bin/agent-terminal"))
        );
        assert_eq!(other_copy(&package, home, |_| false), None);
        let dev = Install::Unmanaged {
            exe: PathBuf::from("/x/target/debug/agent-terminal"),
        };
        assert_eq!(other_copy(&dev, home, everything), None);
    }

    #[test]
    fn an_upgraded_binary_is_looked_up_by_its_installed_path() {
        assert_eq!(
            installed_path(Path::new("/usr/bin/agent-terminal (deleted)")),
            PathBuf::from("/usr/bin/agent-terminal")
        );
        assert_eq!(
            installed_path(Path::new("/usr/bin/agent-terminal")),
            PathBuf::from("/usr/bin/agent-terminal")
        );
    }

    #[test]
    fn a_symbolic_checkpoint_ref_never_takes_its_target() {
        if !git::git_installed() {
            return;
        }
        let tmp = tempfile::tempdir().unwrap();
        let repo = tmp.path();
        let sh = |args: &[&str]| {
            let out = std::process::Command::new("git")
                .args(["-c", "user.name=t", "-c", "user.email=t@t"])
                .args(args)
                .current_dir(repo)
                .env("GIT_CONFIG_NOSYSTEM", "1")
                .env("GIT_CONFIG_GLOBAL", "/dev/null")
                .output()
                .unwrap();
            assert!(out.status.success(), "{args:?}");
            String::from_utf8_lossy(&out.stdout).trim().to_owned()
        };
        sh(&["init", "-q", "-b", "main"]);
        sh(&["commit", "-q", "--allow-empty", "-m", "one"]);
        sh(&[
            "symbolic-ref",
            "refs/agent-terminal/planted",
            "refs/heads/main",
        ]);
        delete_ref(repo, "refs/agent-terminal/planted").unwrap();
        assert!(!sh(&["for-each-ref", "refs/heads/main"]).is_empty());
        assert_eq!(sh(&["for-each-ref", "refs/agent-terminal"]), "");
        assert!(delete_ref(repo, "refs/heads/main").is_err());
    }

    #[test]
    fn a_registration_that_fails_to_revert_keeps_its_journal() {
        let tmp = tempfile::tempdir().unwrap();
        let home = std::fs::canonicalize(tmp.path()).unwrap();
        let state = home.join(DELEGATION_STATE);
        fs::create_dir_all(&state).unwrap();
        fs::write(state.join("delegation-registration.json"), "{}").unwrap();
        // Unreadable agy configuration: the revert fails.
        fs::create_dir_all(home.join(".gemini/config")).unwrap();
        fs::write(home.join(".gemini/config/mcp_config.json"), "not json").unwrap();
        let plan = plan(&inputs(&home));
        assert!(plan
            .items
            .contains(&Item::AgyRegistration { home: home.clone() }));
        let report = apply(&plan, uid(), &home);
        assert_eq!(report.failed.len(), 2, "{:?}", report.failed);
        assert!(state.join("delegation-registration.json").exists());
    }

    #[test]
    fn a_local_install_is_recognised_by_its_path() {
        let home = Path::new("/h");
        assert_eq!(
            detect_install(Path::new("/h/.local/bin/agent-terminal"), home),
            Install::Local {
                files: local_files(home)
            }
        );
    }

    #[test]
    fn sizes_and_paths_read_well() {
        assert_eq!(human_size(0), "0 B");
        assert_eq!(human_size(999), "999 B");
        assert_eq!(human_size(1_644_425_216), "1.6 GB");
        assert_eq!(human_size(24_000), "24 KB");
        let home = Path::new("/h");
        assert_eq!(tilde(Path::new("/h/.config/x"), home), "~/.config/x");
        assert_eq!(tilde(Path::new("/other"), home), "/other");
    }
}
