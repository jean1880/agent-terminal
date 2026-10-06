//! The external diff tool at run time: the one shared current tool (so every card and the diff
//! panel follow Settings at once), and opening a file in it.
//!
//! The tool is run as an argv, never through a shell ([`agent_kit::difftool`]). Everything that
//! touches the disk or a process (reading the old side from git, the temp files, locating and
//! spawning the program) runs off the main thread. Every path is confined to the thread's
//! repository ([`agent_kit::filediff::resolve`]). Logging names the tool, never a file's content
//! or path.

use std::cell::{Cell, RefCell};
use std::ffi::OsString;
use std::path::{Path, PathBuf};

use agent_kit::difftool::{self, DiffTool, Values};
use agent_kit::filediff::{self, Side};
use tracing::{info, warn};

use crate::probe::ListenerSet;

/// The configured tool, shared by every window.
#[derive(Default)]
pub struct DiffTools {
    current: RefCell<Option<DiffTool>>,
    /// Settings → Diff Tool "Expand diffs": a completed edit opens its diff on its own.
    expand_by_default: Cell<bool>,
    listeners: ListenerSet,
}

thread_local! {
    static SHARED: std::rc::Rc<DiffTools> = std::rc::Rc::default();
}

impl DiffTools {
    pub fn shared() -> std::rc::Rc<DiffTools> {
        SHARED.with(std::rc::Rc::clone)
    }

    /// The tool to open files in, when one is configured and valid. A tool that fails
    /// validation (a hand edit) is treated as none: the buttons hide rather than fail.
    pub fn get(&self) -> Option<DiffTool> {
        self.current
            .borrow()
            .clone()
            .filter(|t| difftool::validate(t).is_ok())
    }

    /// Replaces the tool and tells the listeners when it changed.
    pub fn set(&self, tool: Option<DiffTool>) {
        if *self.current.borrow() == tool {
            return;
        }
        *self.current.borrow_mut() = tool;
        self.listeners.notify();
    }

    /// Whether a file-change card opens its diff when its edit completes.
    pub fn expand_by_default(&self) -> bool {
        self.expand_by_default.get()
    }

    pub fn set_expand_by_default(&self, on: bool) {
        self.expand_by_default.set(on);
    }

    /// Calls `f` (on the main thread) whenever the tool changes; the id disconnects it.
    pub fn connect_changed(&self, f: impl Fn() + 'static) -> u64 {
        self.listeners.add(f)
    }

    pub fn disconnect(&self, id: u64) {
        self.listeners.remove(id);
    }
}

/// The button label: "Open in Meld".
pub fn open_label(tool: &DiffTool) -> String {
    format!("Open in {}", tool.name)
}

/// The tooltip a button shows when no tool is configured.
pub const NO_TOOL_HINT: &str = "Set a diff tool in Settings";

/// Where `{new}` comes from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum NewSide {
    /// The working file as it is now.
    Working,
    /// The file as of a commit (the diff panel's "last turn" base compares two checkpoints).
    Rev(String),
}

/// One file to open.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OpenRequest {
    /// The repository root.
    pub toplevel: PathBuf,
    /// The file, as the agent or git named it (confined to `toplevel` before use).
    pub path: String,
    /// The commit or tree the old side is read from, and `{rev}`.
    pub old_rev: String,
    pub new_side: NewSide,
}

/// A process ready to start.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Prepared {
    pub program: PathBuf,
    pub args: Vec<OsString>,
    pub cwd: PathBuf,
}

fn temp_from(root: &Path, rel: &Path, side: Side) -> Result<PathBuf, String> {
    let name = rel.file_name().ok_or("The file has no name")?;
    let content = match &side {
        Side::Bytes(b) => b.as_slice(),
        Side::Missing => &[],
        Side::TooLarge => return Err("The file is too large to open in a diff tool".to_owned()),
    };
    filediff::write_temp_old(root, name, content)
        .map_err(|e| format!("Could not write the temporary file: {e}"))
}

/// Everything short of starting the process: resolves and confines the path, reads the old side
/// from git into a private temp file, finds the program, and fills the argv. Blocking.
///
/// `locate` is `SystemProbe::locate`; only an absolute answer is used.
pub fn prepare(
    tool: &DiffTool,
    req: &OpenRequest,
    locate: impl Fn(&str) -> Option<String>,
    runtime_dir: Option<&str>,
) -> Result<Prepared, String> {
    difftool::validate(tool)?;
    let file = filediff::resolve(&req.toplevel, &req.path)?;
    let root = filediff::temp_root(runtime_dir).ok_or(
        "There is no runtime directory ($XDG_RUNTIME_DIR) to keep the old version of the file in",
    )?;
    // The program first: no temp file for a tool that is not there.
    let program_name = tool.argv.first().map(String::as_str).unwrap_or_default();
    let program = locate(program_name)
        .filter(|p| Path::new(p).is_absolute())
        .ok_or_else(|| {
            format!(
                "{} is not installed ({program_name} was not found)",
                tool.name
            )
        })?;

    let old_side = filediff::file_at_rev(&file_root(&req.toplevel)?, &req.old_rev, &file.rel)?;
    let old = temp_from(&root, &file.rel, old_side)?;
    let new: PathBuf = match &req.new_side {
        NewSide::Working if file.abs.is_file() => file.abs.clone(),
        // A deleted file: the tool is given an empty one to compare against.
        NewSide::Working => temp_from(&root, &file.rel, Side::Missing)?,
        NewSide::Rev(rev) => {
            let side = filediff::file_at_rev(&file_root(&req.toplevel)?, rev, &file.rel)?;
            temp_from(&root, &file.rel, side)?
        }
    };
    let values = Values {
        old: old.into_os_string(),
        new: new.into_os_string(),
        path: file.rel.to_string_lossy().into_owned(),
        repo: file_root(&req.toplevel)?.into_os_string(),
        rev: req.old_rev.clone(),
    };
    let mut argv = difftool::substitute(tool, &values)?.into_iter();
    argv.next(); // the program as written; the located path replaces it
    Ok(Prepared {
        program: PathBuf::from(program),
        args: argv.collect(),
        cwd: file_root(&req.toplevel)?,
    })
}

fn file_root(toplevel: &Path) -> Result<PathBuf, String> {
    toplevel
        .canonicalize()
        .map_err(|e| format!("Could not resolve the repository: {e}"))
}

/// Starts `prepared` detached: no stdin, no output, its own process group, the user's
/// environment. A thread waits for it so it does not linger as a zombie, and logs how it ended
/// (the tool's name only).
pub fn spawn_detached(name: &str, prepared: &Prepared) -> Result<(), String> {
    use std::os::unix::process::CommandExt;
    use std::process::{Command, Stdio};
    let mut child = Command::new(&prepared.program)
        .args(&prepared.args)
        .current_dir(&prepared.cwd)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .process_group(0)
        .spawn()
        .map_err(|e| format!("Could not start {name}: {e}"))?;
    info!(tool = name, "opened the diff tool");
    let name = name.to_owned();
    std::thread::spawn(move || match child.wait() {
        Ok(status) => info!(tool = name, %status, "the diff tool exited"),
        Err(e) => warn!(tool = name, error = %e, "could not wait for the diff tool"),
    });
    Ok(())
}

/// The whole open, blocking: [`prepare`] then [`spawn_detached`].
pub fn open_blocking(
    tool: &DiffTool,
    req: &OpenRequest,
    locate: impl Fn(&str) -> Option<String>,
    runtime_dir: Option<&str>,
) -> Result<(), String> {
    let prepared = prepare(tool, req, locate, runtime_dir)?;
    spawn_detached(&tool.name, &prepared)
}

/// [`open_blocking`] off the main thread, locating the program with the app's own probe.
pub async fn open(tool: DiffTool, req: OpenRequest) -> Result<(), String> {
    let (path, home, shell) = (
        std::env::var("PATH").ok(),
        std::env::var("HOME").ok(),
        std::env::var("SHELL").ok(),
    );
    gtk4::gio::spawn_blocking(move || {
        let probe = crate::utils::SystemProbe::new(path, home, shell);
        let runtime = std::env::var("XDG_RUNTIME_DIR").ok();
        open_blocking(&tool, &req, |c| probe.locate(c), runtime.as_deref())
    })
    .await
    .unwrap_or_else(|_| Err("opening the diff tool panicked".to_owned()))
}

/// Opens a file of the diff panel: the repository is found from the tab's `dir` first.
/// `old_rev` and `new_side` come from the diff on show.
pub async fn open_from_dir(
    tool: DiffTool,
    dir: PathBuf,
    path: String,
    old_rev: String,
    new_side: NewSide,
) -> Result<(), String> {
    let (search, home, shell) = (
        std::env::var("PATH").ok(),
        std::env::var("HOME").ok(),
        std::env::var("SHELL").ok(),
    );
    gtk4::gio::spawn_blocking(move || {
        let toplevel = agent_kit::git::discover(&dir)?
            .ok_or("This tab's folder is not inside a git repository")?
            .toplevel;
        let probe = crate::utils::SystemProbe::new(search, home, shell);
        let runtime = std::env::var("XDG_RUNTIME_DIR").ok();
        let req = OpenRequest {
            toplevel,
            path,
            old_rev,
            new_side,
        };
        open_blocking(&tool, &req, |c| probe.locate(c), runtime.as_deref())
    })
    .await
    .unwrap_or_else(|_| Err("opening the diff tool panicked".to_owned()))
}

/// The presets that are installed, by index into [`difftool::PRESETS`], found off the main
/// thread (the probe may start the user's shell). Only absolute locations count.
pub async fn installed_presets() -> Vec<bool> {
    let (path, home, shell) = (
        std::env::var("PATH").ok(),
        std::env::var("HOME").ok(),
        std::env::var("SHELL").ok(),
    );
    gtk4::gio::spawn_blocking(move || {
        let probe = crate::utils::SystemProbe::new(path, home, shell);
        difftool::PRESETS
            .iter()
            .map(|p| p.is_installed(|c| probe.locate(c)))
            .collect()
    })
    .await
    .unwrap_or_default()
}

/// Start-up housekeeping: files left by a crashed run, older than a day, go.
pub fn sweep_stale() {
    let Some(root) = filediff::temp_root(std::env::var("XDG_RUNTIME_DIR").ok().as_deref()) else {
        return;
    };
    gtk4::gio::spawn_blocking(move || {
        let n = filediff::sweep_temp(&root, filediff::TEMP_MAX_AGE);
        if n > 0 {
            info!(removed = n, "removed stale diff temp files");
        }
    });
}

/// Exit housekeeping: this process's temp files go.
pub fn remove_own() {
    if let Some(root) = filediff::temp_root(std::env::var("XDG_RUNTIME_DIR").ok().as_deref()) {
        filediff::remove_own_temp(&root);
    }
}

#[cfg(test)]
mod tests {
    use std::os::unix::fs::PermissionsExt;
    use std::process::Command;

    use super::*;

    fn sh(dir: &Path, args: &[&str]) {
        let out = Command::new("git")
            .args([
                "-c",
                "user.name=t",
                "-c",
                "user.email=t@t",
                "-c",
                "commit.gpgsign=false",
            ])
            .args([
                "-c",
                "core.hooksPath=/dev/null",
                "-c",
                "init.defaultBranch=main",
            ])
            .args(args)
            .current_dir(dir)
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .output()
            .expect("git");
        assert!(out.status.success(), "git {args:?}");
    }

    /// A repo whose file has a space in its name, plus a tool that records its argv.
    struct Fixture {
        repo: tempfile::TempDir,
        run: tempfile::TempDir,
        recorder: PathBuf,
        log: PathBuf,
    }

    fn fixture() -> Option<Fixture> {
        if !agent_kit::git::git_installed() {
            eprintln!("git not installed; skipping");
            return None;
        }
        let repo = tempfile::tempdir().expect("repo");
        sh(repo.path(), &["init", "-q"]);
        std::fs::write(repo.path().join("my file.txt"), "before\n").expect("file");
        std::fs::write(repo.path().join("-dash.txt"), "before\n").expect("dash");
        sh(repo.path(), &["add", "."]);
        sh(repo.path(), &["commit", "-q", "-m", "init"]);
        let run = tempfile::tempdir().expect("run");
        let log = run.path().join("argv.log");
        let recorder = run.path().join("recorder");
        std::fs::write(
            &recorder,
            format!(
                "#!/bin/sh\nfor a in \"$@\"; do printf '%s\\n' \"$a\" >> '{}'; done\n",
                log.display()
            ),
        )
        .expect("script");
        std::fs::set_permissions(&recorder, std::fs::Permissions::from_mode(0o755)).expect("chmod");
        Some(Fixture {
            repo,
            run,
            recorder,
            log,
        })
    }

    fn head(dir: &Path) -> String {
        let out = Command::new("git")
            .args(["rev-parse", "HEAD"])
            .current_dir(dir)
            .output()
            .expect("git");
        String::from_utf8_lossy(&out.stdout).trim().to_owned()
    }

    fn tool(argv: &[&str]) -> DiffTool {
        DiffTool {
            name: "Recorder".into(),
            argv: argv.iter().map(|s| (*s).to_owned()).collect(),
        }
    }

    #[test]
    fn the_old_side_goes_to_a_private_temp_file_and_the_argv_is_filled_per_element() {
        let Some(f) = fixture() else { return };
        let top = f.repo.path().canonicalize().expect("canon");
        std::fs::write(top.join("my file.txt"), "after\n").expect("edit");
        let req = OpenRequest {
            toplevel: top.clone(),
            path: "my file.txt".into(),
            old_rev: head(&top),
            new_side: NewSide::Working,
        };
        let recorder = f.recorder.display().to_string();
        let prepared = prepare(
            &tool(&["recorder", "--label={path}", "{old}", "{new}"]),
            &req,
            |c| (c == "recorder").then(|| recorder.clone()),
            f.run.path().to_str(),
        )
        .expect("prepare");
        assert_eq!(prepared.program, f.recorder);
        assert_eq!(prepared.cwd, top);
        assert_eq!(prepared.args.len(), 3, "one argument per element");
        assert_eq!(prepared.args[0], OsString::from("--label=my file.txt"));
        // {old}: the pre-turn content in a 0600 file with the original name, in a 0700 directory.
        let old = PathBuf::from(&prepared.args[1]);
        assert_eq!(std::fs::read_to_string(&old).expect("old"), "before\n");
        assert_eq!(
            old.file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .as_deref(),
            Some("my file.txt")
        );
        let mode = |p: &Path| std::fs::metadata(p).expect("meta").permissions().mode() & 0o777;
        assert_eq!(mode(&old), 0o600);
        assert_eq!(mode(old.parent().expect("dir")), 0o700);
        assert!(old.starts_with(f.run.path().join("agent-terminal/diff")));
        // {new}: the working file itself.
        assert_eq!(PathBuf::from(&prepared.args[2]), top.join("my file.txt"));
    }

    #[test]
    fn a_leading_dash_name_and_a_checkpoint_new_side_are_handled() {
        let Some(f) = fixture() else { return };
        let top = f.repo.path().canonicalize().expect("canon");
        let rev = head(&top);
        std::fs::write(top.join("-dash.txt"), "after\n").expect("edit");
        let req = OpenRequest {
            toplevel: top.clone(),
            path: "-dash.txt".into(),
            old_rev: rev.clone(),
            new_side: NewSide::Rev(rev),
        };
        let recorder = f.recorder.display().to_string();
        let prepared = prepare(
            &tool(&["recorder", "{path}", "{new}"]),
            &req,
            |_| Some(recorder.clone()),
            f.run.path().to_str(),
        )
        .expect("prepare");
        assert_eq!(prepared.args[0], OsString::from("./-dash.txt"));
        // The new side was materialised from the commit, not read from the working file.
        assert_eq!(
            std::fs::read_to_string(PathBuf::from(&prepared.args[1])).expect("new"),
            "before\n"
        );
    }

    #[test]
    fn nothing_leaves_the_repository_and_a_missing_program_is_reported_before_anything_is_written()
    {
        let Some(f) = fixture() else { return };
        let top = f.repo.path().canonicalize().expect("canon");
        let rev = head(&top);
        let recorder = f.recorder.display().to_string();
        let run = f.run.path().to_str();
        for bad in ["../x", "/etc/passwd", "nodir/x"] {
            let req = OpenRequest {
                toplevel: top.clone(),
                path: bad.into(),
                old_rev: rev.clone(),
                new_side: NewSide::Working,
            };
            assert!(
                prepare(
                    &tool(&["recorder", "{old}", "{new}"]),
                    &req,
                    |_| Some(recorder.clone()),
                    run
                )
                .is_err(),
                "{bad}"
            );
        }
        let req = OpenRequest {
            toplevel: top.clone(),
            path: "my file.txt".into(),
            old_rev: rev.clone(),
            new_side: NewSide::Working,
        };
        let err = prepare(&tool(&["nonesuch", "{old}", "{new}"]), &req, |_| None, run)
            .expect_err("missing");
        assert!(err.contains("not installed"), "{err}");
        assert!(
            !f.run.path().join("agent-terminal").exists(),
            "no temp file for a missing tool"
        );
        // A bare name from `locate` is not a location.
        assert!(prepare(&tool(&["x", "{old}"]), &req, |c| Some(c.to_owned()), run).is_err());
        // An unknown placeholder, and a rev that is not hex, never reach a process.
        assert!(prepare(
            &tool(&["recorder", "{nope}"]),
            &req,
            |_| Some(recorder.clone()),
            run
        )
        .is_err());
        let hostile = OpenRequest {
            old_rev: "--output=/tmp/x".into(),
            ..req.clone()
        };
        assert!(prepare(
            &tool(&["recorder", "{old}", "{rev}"]),
            &hostile,
            |_| Some(recorder.clone()),
            run
        )
        .is_err());
        // No runtime directory: nowhere private to write.
        assert!(prepare(
            &tool(&["recorder", "{old}"]),
            &req,
            |_| Some(recorder.clone()),
            None
        )
        .is_err());
    }

    #[test]
    fn the_tool_is_started_with_each_element_as_one_argument_and_no_shell() {
        let Some(f) = fixture() else { return };
        let top = f.repo.path().canonicalize().expect("canon");
        std::fs::write(top.join("my file.txt"), "after\n").expect("edit");
        let req = OpenRequest {
            toplevel: top.clone(),
            path: "my file.txt".into(),
            old_rev: head(&top),
            new_side: NewSide::Working,
        };
        let recorder = f.recorder.display().to_string();
        open_blocking(
            &tool(&[
                "recorder",
                "$(touch /tmp/pwned-by-diff-test)",
                "{path}",
                "a;b",
            ]),
            &req,
            |_| Some(recorder.clone()),
            f.run.path().to_str(),
        )
        .expect("open");
        let mut waited = 0;
        while !f.log.exists() && waited < 100 {
            std::thread::sleep(std::time::Duration::from_millis(50));
            waited += 1;
        }
        let logged = std::fs::read_to_string(&f.log).expect("the tool ran");
        assert_eq!(
            logged,
            "$(touch /tmp/pwned-by-diff-test)\nmy file.txt\na;b\n"
        );
        assert!(!Path::new("/tmp/pwned-by-diff-test").exists());
    }

    #[test]
    fn a_program_that_cannot_start_is_an_error_not_a_crash() {
        let prepared = Prepared {
            program: "/nonexistent/diff-tool".into(),
            args: Vec::new(),
            cwd: "/".into(),
        };
        let err = spawn_detached("Ghost", &prepared).expect_err("no such file");
        assert!(err.starts_with("Could not start Ghost"), "{err}");
    }

    #[test]
    fn a_hand_edited_invalid_tool_reads_as_none_and_changes_notify_once() {
        let tools = DiffTools::default();
        let seen = std::rc::Rc::new(std::cell::Cell::new(0));
        let counter = seen.clone();
        tools.connect_changed(move || counter.set(counter.get() + 1));
        tools.set(Some(tool(&["meld", "{old}", "{new}"])));
        tools.set(Some(tool(&["meld", "{old}", "{new}"])));
        assert_eq!(seen.get(), 1, "setting the same tool again is not a change");
        assert!(tools.get().is_some());
        tools.set(Some(tool(&["meld", "{bogus}"])));
        assert_eq!(tools.get(), None);
        assert_eq!(open_label(&tool(&["meld", "{new}"])), "Open in Recorder");
    }
}
