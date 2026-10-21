//! L2, the file-system part (the pure parts are `agent_core::delegation::
//! agy_guard`). Blocking: run off the main thread.
//!
//! Every check fails closed: an entry that cannot be listed, a type that cannot be read, a
//! dangling link or a limit reached withholds delegation just as a source found does.

use std::collections::HashSet;
use std::ffi::OsStr;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};

use agent_core::delegation::agy_guard::{
    allow_rules_missing, argv_redirect, env_redirect, l2_directories, L2Refusal, ALLOW_RULES,
    WORKSPACE_SOURCE_NAMES,
};
use agent_core::delegation::Limits;

/// One agy launch, as L2 judges it: the final argv and environment and the physical cwd.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct L2Input {
    pub argv: Vec<String>,
    pub env: Vec<(String, String)>,
    pub physical_cwd: PathBuf,
    pub verified_home: PathBuf,
    /// No approval hook: the allow rules in `~/.gemini/antigravity-cli/settings.json` must exist.
    pub hookless: bool,
}

/// agy's hookless allow rules, under the verified home.
const AGY_SETTINGS: &str = ".gemini/antigravity-cli/settings.json";

/// The git toplevel of a directory: `Ok(None)` outside a repository.
type Discover<'a> = &'a dyn Fn(&Path) -> Result<Option<PathBuf>, String>;

/// Where the walk may go. The app walks the real filesystem; tests bound it to a temp tree.
struct Bounds<'a> {
    discover: Discover<'a>,
    /// Tests only: the walk stops at this directory instead of `/`.
    stop_at: Option<&'a Path>,
}

fn real_discover(dir: &Path) -> Result<Option<PathBuf>, String> {
    agent_kit::git::discover(dir).map(|repo| repo.map(|r| r.toplevel))
}

/// `Ok` only when every L2 check passes; else why delegation is withheld.
pub(crate) fn scan(input: &L2Input, limits: &Limits) -> Result<(), L2Refusal> {
    scan_with(
        input,
        limits,
        &Bounds {
            discover: &real_discover,
            stop_at: None,
        },
    )
}

fn scan_with(input: &L2Input, limits: &Limits, bounds: &Bounds<'_>) -> Result<(), L2Refusal> {
    if let Some(flag) = argv_redirect(&input.argv) {
        return Err(L2Refusal::ArgvRedirect(flag));
    }
    if let Some(name) = env_redirect(&input.env) {
        return Err(L2Refusal::EnvRedirect(name));
    }
    check_home(input)?;
    if input.hookless {
        check_allow_rules(&input.verified_home)?;
    }
    for dir in searched_directories(&input.physical_cwd, bounds)? {
        scan_directory(&dir, &input.physical_cwd, limits)?;
    }
    Ok(())
}

/// The environment's `HOME` (the last one wins, as in a spawn), canonicalised, is the verified
/// home.
fn check_home(input: &L2Input) -> Result<(), L2Refusal> {
    let home = input
        .env
        .iter()
        .rev()
        .find(|(k, _)| k == "HOME")
        .map(|(_, v)| PathBuf::from(v))
        .filter(|h| h.is_absolute())
        .ok_or(L2Refusal::HomeMismatch)?;
    let real = std::fs::canonicalize(home).map_err(|_| L2Refusal::HomeMismatch)?;
    let verified =
        std::fs::canonicalize(&input.verified_home).map_err(|_| L2Refusal::HomeMismatch)?;
    if real == verified {
        Ok(())
    } else {
        Err(L2Refusal::HomeMismatch)
    }
}

fn check_allow_rules(home: &Path) -> Result<(), L2Refusal> {
    let text = match std::fs::read_to_string(home.join(AGY_SETTINGS)) {
        Ok(text) => text,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            return Err(L2Refusal::AllowRulesMissing(
                ALLOW_RULES.iter().map(|r| (*r).to_owned()).collect(),
            ));
        }
        Err(_) => {
            return Err(L2Refusal::Unreadable(
                "antigravity-cli/settings.json".into(),
            ))
        }
    };
    let missing = allow_rules_missing(&text)?;
    if missing.is_empty() {
        Ok(())
    } else {
        Err(L2Refusal::AllowRulesMissing(
            missing.into_iter().map(str::to_owned).collect(),
        ))
    }
}

/// Git: the cwd up to its toplevel. Non-git: the cwd and every ancestor up to `/` (or the test
/// bound). A failed discovery withholds.
fn searched_directories(cwd: &Path, bounds: &Bounds<'_>) -> Result<Vec<PathBuf>, L2Refusal> {
    if !cwd.is_absolute() {
        return Err(L2Refusal::DiscoveryFailed);
    }
    let physical = std::fs::canonicalize(cwd).map_err(|_| L2Refusal::DiscoveryFailed)?;
    if physical != cwd {
        // The caller promised a physical path; a link on the way means it is not one.
        return Err(L2Refusal::DiscoveryFailed);
    }
    let toplevel = (bounds.discover)(&physical)
        .map_err(|_| L2Refusal::DiscoveryFailed)?
        .map(|top| std::fs::canonicalize(top).map_err(|_| L2Refusal::DiscoveryFailed))
        .transpose()?;
    let mut dirs = l2_directories(&physical, toplevel.as_deref());
    if let Some(stop) = bounds.stop_at {
        if let Some(i) = dirs.iter().position(|d| d == stop) {
            dirs.truncate(i + 1);
        }
    }
    Ok(dirs)
}

/// `path` for a refusal: relative to the cwd, through `..` for an ancestor. Never a value.
fn shown(path: &Path, cwd: &Path) -> String {
    if let Ok(rel) = path.strip_prefix(cwd) {
        return rel.to_string_lossy().into_owned();
    }
    for (up, ancestor) in cwd.ancestors().enumerate().skip(1) {
        if let Ok(rel) = path.strip_prefix(ancestor) {
            let mut out = PathBuf::new();
            for _ in 0..up {
                out.push("..");
            }
            return out.join(rel).to_string_lossy().into_owned();
        }
    }
    path.to_string_lossy().into_owned()
}

/// `lstat`: `Ok(false)` only for "not there"; any other error withholds.
fn present(path: &Path, cwd: &Path) -> Result<bool, L2Refusal> {
    match std::fs::symlink_metadata(path) {
        Ok(_) => Ok(true),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(_) => Err(L2Refusal::Unreadable(shown(path, cwd))),
    }
}

fn is_source_name(name: &OsStr) -> bool {
    WORKSPACE_SOURCE_NAMES.iter().any(|n| OsStr::new(n) == name)
}

/// One searched directory: `hooks/hooks.json`, then a recursive scan of `.agents`.
fn scan_directory(dir: &Path, cwd: &Path, limits: &Limits) -> Result<(), L2Refusal> {
    let hooks = dir.join("hooks/hooks.json");
    if present(&hooks, cwd)? {
        return Err(L2Refusal::WorkspaceSource(shown(&hooks, cwd)));
    }
    let agents = dir.join(".agents");
    if !present(&agents, cwd)? {
        return Ok(());
    }
    // Follow a linked `.agents`; a dangling one withholds.
    let meta =
        std::fs::metadata(&agents).map_err(|_| L2Refusal::Unreadable(shown(&agents, cwd)))?;
    if !meta.is_dir() {
        // agy reads `.agents` as a directory; a plain file there declares nothing.
        return Ok(());
    }
    scan_tree(&agents, cwd, limits)
}

/// The recursive `.agents` scan: any entry with a source name, at any depth, withholds. Linked
/// directories are followed (canonicalised); a directory already seen by `(dev, ino)` is a cycle
/// and is not entered again. Bounded by depth and by entries.
fn scan_tree(agents: &Path, cwd: &Path, limits: &Limits) -> Result<(), L2Refusal> {
    let unreadable = |p: &Path| L2Refusal::Unreadable(shown(p, cwd));
    let root_meta = std::fs::metadata(agents).map_err(|_| unreadable(agents))?;
    let mut seen: HashSet<(u64, u64)> = HashSet::from([(root_meta.dev(), root_meta.ino())]);
    // (the path shown for findings, the path read, depth below `.agents`)
    let mut stack: Vec<(PathBuf, PathBuf, u32)> = vec![(agents.to_path_buf(), agents.into(), 0)];
    let mut entries: u32 = 0;
    while let Some((logical, real, depth)) = stack.pop() {
        let listing = std::fs::read_dir(&real).map_err(|_| unreadable(&logical))?;
        for entry in listing {
            let entry = entry.map_err(|_| unreadable(&logical))?;
            entries += 1;
            if entries > limits.l2_scan_entries {
                return Err(L2Refusal::ScanLimit);
            }
            let name = entry.file_name();
            let logical_child = logical.join(&name);
            if is_source_name(&name) {
                return Err(L2Refusal::WorkspaceSource(shown(&logical_child, cwd)));
            }
            let kind = entry.file_type().map_err(|_| unreadable(&logical_child))?;
            let real_child = entry.path();
            let (is_dir, next_real) = if kind.is_symlink() {
                // A dangling link, or one whose target cannot be read, withholds.
                let meta =
                    std::fs::metadata(&real_child).map_err(|_| unreadable(&logical_child))?;
                if meta.is_dir() {
                    let canonical = std::fs::canonicalize(&real_child)
                        .map_err(|_| unreadable(&logical_child))?;
                    (true, canonical)
                } else {
                    (false, real_child)
                }
            } else {
                (kind.is_dir(), real_child)
            };
            if !is_dir {
                continue;
            }
            let meta = std::fs::metadata(&next_real).map_err(|_| unreadable(&logical_child))?;
            if !seen.insert((meta.dev(), meta.ino())) {
                continue; // a cycle, or a directory reached twice: already scanned
            }
            if depth + 1 > limits.l2_scan_depth {
                return Err(L2Refusal::ScanLimit);
            }
            stack.push((logical_child, next_real, depth + 1));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::os::unix::fs::{symlink, PermissionsExt};

    struct Tree {
        _tmp: tempfile::TempDir,
        /// The canonical temp root: the walk's injected bound.
        root: PathBuf,
        /// `<root>/a/b/c`, the launch's cwd.
        cwd: PathBuf,
        home: PathBuf,
    }

    impl Tree {
        fn new() -> Self {
            let tmp = tempfile::tempdir().expect("tempdir");
            let root = tmp.path().canonicalize().expect("canonical");
            let cwd = root.join("a/b/c");
            let home = root.join("home");
            fs::create_dir_all(&cwd).expect("mkdir");
            fs::create_dir_all(home.join(".gemini/antigravity-cli")).expect("mkdir");
            Self {
                _tmp: tmp,
                root,
                cwd,
                home,
            }
        }

        fn input(&self) -> L2Input {
            L2Input {
                argv: vec!["agy".into(), "--mode".into(), "plan".into()],
                env: vec![
                    ("PATH".into(), "/usr/bin".into()),
                    ("HOME".into(), self.home.to_string_lossy().into_owned()),
                ],
                physical_cwd: self.cwd.clone(),
                verified_home: self.home.clone(),
                hookless: false,
            }
        }

        /// The walk bounded at the temp root, outside git.
        fn scan(&self, input: &L2Input, limits: &Limits) -> Result<(), L2Refusal> {
            let no_git = |_: &Path| Ok(None);
            scan_with(
                input,
                limits,
                &Bounds {
                    discover: &no_git,
                    stop_at: Some(&self.root),
                },
            )
        }

        fn touch(&self, rel: &str) {
            let p = self.root.join(rel);
            fs::create_dir_all(p.parent().expect("parent")).expect("mkdir");
            fs::write(p, b"{}").expect("write");
        }
    }

    fn running_as_root() -> bool {
        super::super::rundir::effective_uid() == 0
    }

    #[test]
    fn a_clean_tree_passes() {
        let t = Tree::new();
        t.touch("a/b/c/.agents/rules/style.md");
        t.touch("a/.agents/skills/x/SKILL.md");
        assert_eq!(t.scan(&t.input(), &Limits::default()), Ok(()));
    }

    #[test]
    fn a_non_git_walk_reaches_the_root_bound() {
        let t = Tree::new();
        // Above the cwd, at the very top of the walk.
        t.touch(".agents/mcp_config.json");
        assert_eq!(
            t.scan(&t.input(), &Limits::default()),
            Err(L2Refusal::WorkspaceSource(
                "../../../.agents/mcp_config.json".into()
            ))
        );
        let t = Tree::new();
        t.touch("a/hooks/hooks.json");
        assert_eq!(
            t.scan(&t.input(), &Limits::default()),
            Err(L2Refusal::WorkspaceSource("../../hooks/hooks.json".into()))
        );
    }

    #[test]
    fn the_unbounded_walk_goes_to_slash() {
        let t = Tree::new();
        let no_git = |_: &Path| Ok(None);
        let dirs = searched_directories(
            &t.cwd,
            &Bounds {
                discover: &no_git,
                stop_at: None,
            },
        )
        .expect("dirs");
        assert_eq!(dirs.first(), Some(&t.cwd));
        assert_eq!(dirs.last().map(PathBuf::as_path), Some(Path::new("/")));
        let failing = |_: &Path| Err("git timed out".to_owned());
        assert_eq!(
            searched_directories(
                &t.cwd,
                &Bounds {
                    discover: &failing,
                    stop_at: None,
                }
            ),
            Err(L2Refusal::DiscoveryFailed)
        );
    }

    #[test]
    fn git_stops_at_the_toplevel() {
        if !agent_kit::git::git_installed() {
            return;
        }
        let t = Tree::new();
        let repo = t.root.join("a");
        let out = std::process::Command::new("git")
            .args(["init", "-q"])
            .current_dir(&repo)
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .output()
            .expect("git init");
        assert!(out.status.success());
        // Above the toplevel: not searched.
        t.touch(".agents/mcp_config.json");
        let real = |d: &Path| real_discover(d);
        let bounds = Bounds {
            discover: &real,
            stop_at: None,
        };
        assert_eq!(scan_with(&t.input(), &Limits::default(), &bounds), Ok(()));
        // At the toplevel: searched.
        t.touch("a/.agents/plugins.txt");
        assert_eq!(
            scan_with(&t.input(), &Limits::default(), &bounds),
            Err(L2Refusal::WorkspaceSource(
                "../../.agents/plugins.txt".into()
            ))
        );
    }

    #[test]
    fn every_source_name_withholds() {
        for name in WORKSPACE_SOURCE_NAMES {
            let t = Tree::new();
            t.touch(&format!("a/b/c/.agents/{name}"));
            assert_eq!(
                t.scan(&t.input(), &Limits::default()),
                Err(L2Refusal::WorkspaceSource(format!(".agents/{name}"))),
                "{name}"
            );
        }
        // `sidecars` as a directory, too.
        let t = Tree::new();
        fs::create_dir_all(t.cwd.join(".agents/sidecars")).expect("mkdir");
        assert_eq!(
            t.scan(&t.input(), &Limits::default()),
            Err(L2Refusal::WorkspaceSource(".agents/sidecars".into()))
        );
    }

    #[test]
    fn nested_plugins_are_found() {
        let t = Tree::new();
        t.touch("a/b/c/.agents/plugins/p1/deep/er/plugin.json");
        assert_eq!(
            t.scan(&t.input(), &Limits::default()),
            Err(L2Refusal::WorkspaceSource(
                ".agents/plugins/p1/deep/er/plugin.json".into()
            ))
        );
        let t = Tree::new();
        t.touch("a/b/c/.agents/plugins/p2/mcp_config.json");
        assert!(matches!(
            t.scan(&t.input(), &Limits::default()),
            Err(L2Refusal::WorkspaceSource(_))
        ));
    }

    #[test]
    fn linked_directories_are_followed_and_cycles_end() {
        let t = Tree::new();
        let agents = t.cwd.join(".agents");
        fs::create_dir_all(agents.join("x")).expect("mkdir");
        symlink(&agents, agents.join("x/loop")).expect("symlink");
        symlink(&agents, agents.join("self")).expect("symlink");
        assert_eq!(t.scan(&t.input(), &Limits::default()), Ok(()));
        // A linked directory outside the tree is scanned through the link.
        let outside = t.root.join("elsewhere/plug");
        fs::create_dir_all(&outside).expect("mkdir");
        fs::write(outside.join("hooks.json"), b"{}").expect("write");
        symlink(t.root.join("elsewhere"), agents.join("x/linked")).expect("symlink");
        assert_eq!(
            t.scan(&t.input(), &Limits::default()),
            Err(L2Refusal::WorkspaceSource(
                ".agents/x/linked/plug/hooks.json".into()
            ))
        );
    }

    #[test]
    fn a_dangling_link_or_unreadable_dir_withholds() {
        let t = Tree::new();
        let agents = t.cwd.join(".agents");
        fs::create_dir_all(&agents).expect("mkdir");
        symlink(t.root.join("nowhere"), agents.join("gone")).expect("symlink");
        assert_eq!(
            t.scan(&t.input(), &Limits::default()),
            Err(L2Refusal::Unreadable(".agents/gone".into()))
        );
        if running_as_root() {
            return; // root reads a 000 directory
        }
        let t = Tree::new();
        let locked = t.cwd.join(".agents/locked");
        fs::create_dir_all(&locked).expect("mkdir");
        fs::set_permissions(&locked, fs::Permissions::from_mode(0o000)).expect("chmod");
        let got = t.scan(&t.input(), &Limits::default());
        fs::set_permissions(&locked, fs::Permissions::from_mode(0o700)).expect("chmod back");
        assert_eq!(got, Err(L2Refusal::Unreadable(".agents/locked".into())));
    }

    #[test]
    fn the_limits_withhold() {
        let t = Tree::new();
        t.touch("a/b/c/.agents/1/2/3/note.md");
        let shallow = Limits {
            l2_scan_depth: 2,
            ..Limits::default()
        };
        assert_eq!(t.scan(&t.input(), &shallow), Err(L2Refusal::ScanLimit));
        let few = Limits {
            l2_scan_entries: 3,
            ..Limits::default()
        };
        assert_eq!(t.scan(&t.input(), &few), Err(L2Refusal::ScanLimit));
        assert_eq!(t.scan(&t.input(), &Limits::default()), Ok(()));
    }

    #[test]
    fn argv_and_env_redirects_withhold() {
        let t = Tree::new();
        for flag in ["--gemini_dir", "--gemini-dir=/x", "-app_data_dir"] {
            let mut input = t.input();
            input.argv.push(flag.into());
            input.argv.push("/x".into());
            assert!(
                matches!(
                    t.scan(&input, &Limits::default()),
                    Err(L2Refusal::ArgvRedirect(_))
                ),
                "{flag}"
            );
        }
        for name in [
            "ANTIGRAVITY_APP_DATA_DIR",
            "JETSKI_APP_DATA_DIR",
            "CASCADE_GLOBAL_CONFIG_OVERRIDE",
            "CASCADE_MCP_SERVER_INIT",
        ] {
            let mut input = t.input();
            input.env.push((name.into(), String::new()));
            assert_eq!(
                t.scan(&input, &Limits::default()),
                Err(L2Refusal::EnvRedirect(name.into()))
            );
        }
    }

    #[test]
    fn home_must_be_the_verified_one() {
        let t = Tree::new();
        let mut other = t.input();
        other
            .env
            .push(("HOME".into(), t.root.to_string_lossy().into_owned()));
        assert_eq!(
            t.scan(&other, &Limits::default()),
            Err(L2Refusal::HomeMismatch)
        );
        let mut none = t.input();
        none.env.retain(|(k, _)| k != "HOME");
        assert_eq!(
            t.scan(&none, &Limits::default()),
            Err(L2Refusal::HomeMismatch)
        );
        // A link to the verified home is the same home.
        let link = t.root.join("home-link");
        symlink(&t.home, &link).expect("symlink");
        let mut linked = t.input();
        linked
            .env
            .push(("HOME".into(), link.to_string_lossy().into_owned()));
        assert_eq!(t.scan(&linked, &Limits::default()), Ok(()));
    }

    #[test]
    fn hookless_needs_every_allow_rule() {
        let t = Tree::new();
        let mut input = t.input();
        input.hookless = true;
        assert_eq!(
            t.scan(&input, &Limits::default()),
            Err(L2Refusal::AllowRulesMissing(
                ALLOW_RULES.iter().map(|r| (*r).to_owned()).collect()
            ))
        );
        let settings = t.home.join(AGY_SETTINGS);
        fs::write(&settings, "{not json").expect("write");
        assert!(matches!(
            t.scan(&input, &Limits::default()),
            Err(L2Refusal::Unreadable(_))
        ));
        let some = serde_json::json!({"permissions": {"allow": &ALLOW_RULES[..7]}});
        fs::write(&settings, some.to_string()).expect("write");
        assert_eq!(
            t.scan(&input, &Limits::default()),
            Err(L2Refusal::AllowRulesMissing(
                vec![ALLOW_RULES[7].to_owned()]
            ))
        );
        let all = serde_json::json!({"permissions": {"allow": ALLOW_RULES}});
        fs::write(&settings, all.to_string()).expect("write");
        assert_eq!(t.scan(&input, &Limits::default()), Ok(()));
    }

    #[test]
    fn a_cwd_through_a_link_is_not_physical() {
        let t = Tree::new();
        let link = t.root.join("cwd-link");
        symlink(&t.cwd, &link).expect("symlink");
        let mut input = t.input();
        input.physical_cwd = link;
        assert_eq!(
            t.scan(&input, &Limits::default()),
            Err(L2Refusal::DiscoveryFailed)
        );
    }
}
