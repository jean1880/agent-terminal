//! Transport for one agent process, on the GTK main loop (gio, no threads, no tokio).
//!
//! [`AgentProcess`] spawns the agent with `gio::SubprocessLauncher`, reads its stdout and stderr
//! line by line with `DataInputStream::read_line_future` inside `glib::spawn_future_local`, queues
//! stdin writes in order, and reports the exit once both pipes have drained (so the last frames
//! always reach the adapter before `SessionExited`). Nothing here blocks the main thread and
//! nothing panics on an I/O error: a failure is logged and surfaces as an exit.
//!
//! Logging never includes argv, prompt or frame bodies (they may hold secrets); only the program's
//! file name, the pid and the exit code.

use std::cell::{Cell, RefCell};
use std::collections::VecDeque;
use std::ffi::OsStr;
use std::path::Path;
use std::rc::Rc;
use std::time::Duration;

use gtk4::gio::{self, prelude::*};
use gtk4::glib;
use tracing::{debug, info, warn};

const IO_PRIORITY: glib::Priority = glib::Priority::DEFAULT;
const SIGINT: i32 = 2;
const SIGTERM: i32 = 15;
const SIGKILL: i32 = 9;
/// How long a process may ignore SIGTERM before it is killed.
const TERM_GRACE: Duration = Duration::from_secs(3);
/// After the process exits, how long a pipe held open by a grandchild may delay the exit report.
const EXIT_DRAIN: Duration = Duration::from_secs(2);

/// What to run.
#[derive(Debug, Clone, Default)]
pub struct SpawnSpec {
    /// Program first. Looked up in `PATH` when it has no slash.
    pub argv: Vec<String>,
    pub cwd: Option<String>,
    /// Added to the user's own environment (which is inherited), overriding on conflict.
    pub env: Vec<(String, String)>,
}

type ExitCallback = Box<dyn FnOnce(Option<i32>)>;

struct Shared {
    subprocess: gio::Subprocess,
    /// The process id, which is also its process group id (it is a session leader).
    pid: Option<i32>,
    stdin: Option<gio::OutputStream>,
    queue: RefCell<VecDeque<String>>,
    pumping: Cell<bool>,
    stdin_dead: Cell<bool>,
    exited: Cell<bool>,
    readers_open: Cell<u8>,
    /// `Some` once the process has been reaped; the inner option is the exit code.
    exit_code: Cell<Option<Option<i32>>>,
    on_exit: RefCell<Option<ExitCallback>>,
    name: String,
}

impl Shared {
    /// Signals the agent's whole process group; falls back to the agent alone.
    ///
    /// Ceiling: a tool child that called `setsid` itself (a daemon) is in another group and is
    /// not reached, and once the agent has exited and been reaped nothing is signalled (its
    /// pid could be reused). Upgrade path: a cgroup (`systemd-run --user --scope`).
    fn signal(&self, signal: i32) {
        if let Some(pid) = self.pid {
            // SAFETY: kill(2) with a negative pid signals that process group; no memory is touched.
            if unsafe { libc::kill(-pid, signal) } == 0 {
                return;
            }
        }
        self.subprocess.send_signal(signal);
    }

    fn reader_done(&self) {
        self.readers_open
            .set(self.readers_open.get().saturating_sub(1));
        if self.readers_open.get() == 0 {
            self.finish();
        }
    }

    /// Reports the exit once, when the process is reaped (pipes drained or the drain window over).
    fn finish(&self) {
        let Some(code) = self.exit_code.get() else {
            return;
        };
        let callback = self.on_exit.borrow_mut().take();
        if let Some(callback) = callback {
            callback(code);
        }
    }
}

/// A running agent. Dropping it terminates the process (SIGTERM, then SIGKILL after a grace).
pub struct AgentProcess {
    shared: Rc<Shared>,
}

impl AgentProcess {
    /// Starts the process. `on_stdout` / `on_stderr` get each line without its newline;
    /// `on_exit` is called exactly once with the exit code (`None` when killed by a signal),
    /// after both pipes reached EOF. All callbacks run on the main thread.
    pub fn spawn(
        spec: &SpawnSpec,
        on_stdout: impl Fn(&str) + 'static,
        on_stderr: impl Fn(&str) + 'static,
        on_exit: impl FnOnce(Option<i32>) + 'static,
    ) -> Result<Self, String> {
        let Some(program) = spec.argv.first() else {
            return Err("empty command line".to_owned());
        };
        let name = Path::new(program)
            .file_name()
            .map_or_else(|| program.clone(), |n| n.to_string_lossy().into_owned());
        let launcher = gio::SubprocessLauncher::new(
            gio::SubprocessFlags::STDIN_PIPE
                | gio::SubprocessFlags::STDOUT_PIPE
                | gio::SubprocessFlags::STDERR_PIPE,
        );
        if let Some(cwd) = &spec.cwd {
            launcher.set_cwd(cwd);
        }
        // Own session and process group, so the agent's tool children can be signalled with it
        // (and a terminal Ctrl-C aimed at the app does not reach them). `setsid` is
        // async-signal-safe, which is all the post-fork child may call.
        launcher.set_child_setup(|| {
            // SAFETY: setsid(2) takes no arguments and touches no memory.
            unsafe {
                libc::setsid();
            }
        });
        for (key, value) in &spec.env {
            launcher.setenv(key, value, true);
        }
        let argv: Vec<&OsStr> = spec.argv.iter().map(|a| OsStr::new(a.as_str())).collect();
        let subprocess = launcher
            .spawn(&argv)
            .map_err(|e| format!("could not start {name}: {}", e.message()))?;
        let (Some(stdout), Some(stderr)) = (subprocess.stdout_pipe(), subprocess.stderr_pipe())
        else {
            subprocess.force_exit();
            return Err(format!("{name} started without its output pipes"));
        };
        info!(
            program = %name,
            pid = subprocess.identifier().as_deref().unwrap_or("?"),
            "agent process started"
        );

        let pid = subprocess
            .identifier()
            .and_then(|id| id.parse::<i32>().ok())
            .filter(|p| *p > 1);
        let shared = Rc::new(Shared {
            pid,
            stdin: subprocess.stdin_pipe(),
            subprocess,
            queue: RefCell::new(VecDeque::new()),
            pumping: Cell::new(false),
            stdin_dead: Cell::new(false),
            exited: Cell::new(false),
            readers_open: Cell::new(2),
            exit_code: Cell::new(None),
            on_exit: RefCell::new(Some(Box::new(on_exit))),
            name,
        });
        spawn_reader(shared.clone(), stdout, Box::new(on_stdout), "stdout");
        spawn_reader(shared.clone(), stderr, Box::new(on_stderr), "stderr");
        spawn_waiter(shared.clone());
        Ok(Self { shared })
    }

    pub fn is_alive(&self) -> bool {
        !self.shared.exited.get()
    }

    /// Queues one line for stdin (a newline is appended). Writes happen in call order.
    pub fn write_line(&self, line: &str) {
        let shared = &self.shared;
        if shared.exited.get() || shared.stdin_dead.get() {
            debug!(program = %shared.name, "dropping a write to a dead agent");
            return;
        }
        let mut buf = String::with_capacity(line.len() + 1);
        buf.push_str(line);
        buf.push('\n');
        shared.queue.borrow_mut().push_back(buf);
        pump(shared);
    }

    /// SIGINT (what agy treats as "stop this turn"; it then exits).
    pub fn interrupt(&self) {
        if self.is_alive() {
            self.shared.signal(SIGINT);
        }
    }

    /// SIGTERM, then SIGKILL if it is still there after a short grace.
    pub fn terminate(&self) {
        terminate(&self.shared);
    }
}

impl Drop for AgentProcess {
    fn drop(&mut self) {
        terminate(&self.shared);
    }
}

fn terminate(shared: &Rc<Shared>) {
    if shared.exited.get() {
        return;
    }
    shared.signal(SIGTERM);
    let shared = shared.clone();
    glib::spawn_future_local(async move {
        glib::timeout_future(TERM_GRACE).await;
        if !shared.exited.get() {
            warn!(program = %shared.name, "agent ignored SIGTERM; killing it");
            shared.signal(SIGKILL);
        }
    });
}

fn spawn_reader(
    shared: Rc<Shared>,
    stream: gio::InputStream,
    on_line: Box<dyn Fn(&str)>,
    label: &'static str,
) {
    glib::spawn_future_local(async move {
        let data = gio::DataInputStream::new(&stream);
        loop {
            match data.read_line_future(IO_PRIORITY).await {
                Ok(Some(bytes)) => {
                    let line = String::from_utf8_lossy(&bytes);
                    on_line(line.trim_end_matches('\r'));
                }
                Ok(None) => break,
                Err(e) => {
                    warn!(program = %shared.name, pipe = label, error = %e.message(), "read failed");
                    break;
                }
            }
        }
        shared.reader_done();
    });
}

fn spawn_waiter(shared: Rc<Shared>) {
    glib::spawn_future_local(async move {
        let code = match shared.subprocess.wait_future().await {
            Ok(()) if shared.subprocess.has_exited() => Some(shared.subprocess.exit_status()),
            Ok(()) => None,
            Err(e) => {
                warn!(program = %shared.name, error = %e.message(), "wait failed");
                None
            }
        };
        info!(program = %shared.name, code = ?code, "agent process exited");
        shared.exited.set(true);
        shared.queue.borrow_mut().clear();
        shared.exit_code.set(Some(code));
        if shared.readers_open.get() == 0 {
            shared.finish();
        } else {
            // A grandchild may still hold a pipe open; do not wait for it forever.
            glib::timeout_future(EXIT_DRAIN).await;
            shared.finish();
        }
    });
}

fn pump(shared: &Rc<Shared>) {
    if shared.pumping.replace(true) {
        return;
    }
    let shared = shared.clone();
    glib::spawn_future_local(async move {
        loop {
            let next = shared.queue.borrow_mut().pop_front();
            let (Some(buf), Some(stdin)) = (next, shared.stdin.clone()) else {
                break;
            };
            let failure = match stdin.write_all_future(buf.into_bytes(), IO_PRIORITY).await {
                Ok((_, _, None)) => None,
                Ok((_, _, Some(e))) | Err((_, e)) => Some(e),
            };
            if let Some(e) = failure {
                warn!(program = %shared.name, error = %e.message(), "write to the agent failed");
                shared.stdin_dead.set(true);
                shared.queue.borrow_mut().clear();
                break;
            }
        }
        shared.pumping.set(false);
    });
}

/// Runs a one-shot side process (agy `-p /model --output-format json`, `agy models`) and returns
/// its stdout and whether it exited with status 0. A failure to start is `("<reason>", false)`.
pub async fn run_side(argv: Vec<String>, cwd: Option<String>) -> (String, bool) {
    let Some(program) = argv.first() else {
        return ("empty command line".to_owned(), false);
    };
    let name = Path::new(program)
        .file_name()
        .map_or_else(|| program.clone(), |n| n.to_string_lossy().into_owned());
    let launcher = gio::SubprocessLauncher::new(
        gio::SubprocessFlags::STDOUT_PIPE | gio::SubprocessFlags::STDERR_SILENCE,
    );
    if let Some(cwd) = &cwd {
        launcher.set_cwd(cwd);
    }
    let args: Vec<&OsStr> = argv.iter().map(|a| OsStr::new(a.as_str())).collect();
    let subprocess = match launcher.spawn(&args) {
        Ok(s) => s,
        Err(e) => {
            warn!(program = %name, error = %e.message(), "side process did not start");
            return (format!("could not start {name}: {}", e.message()), false);
        }
    };
    match subprocess.communicate_utf8_future(None).await {
        Ok((stdout, _)) => (
            stdout.map(|s| s.to_string()).unwrap_or_default(),
            subprocess.is_successful(),
        ),
        Err(e) => {
            warn!(program = %name, error = %e.message(), "side process failed");
            (e.message().to_owned(), false)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::{in_loop, pump_until};

    fn sh(script: &str) -> SpawnSpec {
        SpawnSpec {
            argv: vec!["/bin/sh".into(), "-c".into(), script.into()],
            ..SpawnSpec::default()
        }
    }

    type Lines = Rc<RefCell<Vec<String>>>;
    type Exit = Rc<Cell<Option<Option<i32>>>>;

    fn start(spec: &SpawnSpec) -> (AgentProcess, Lines, Lines, Exit) {
        let out: Lines = Rc::default();
        let err: Lines = Rc::default();
        let exit: Exit = Rc::default();
        let (o, e, x) = (out.clone(), err.clone(), exit.clone());
        let proc = AgentProcess::spawn(
            spec,
            move |l| o.borrow_mut().push(l.to_owned()),
            move |l| e.borrow_mut().push(l.to_owned()),
            move |c| x.set(Some(c)),
        )
        .expect("spawn");
        (proc, out, err, exit)
    }

    #[test]
    fn lines_round_trip_in_order_and_exit_reports_the_code() {
        in_loop(|ctx| {
            let (proc, out, err, exit) = start(&sh(
                "while read l; do echo \"out:$l\"; echo \"err:$l\" >&2; [ \"$l\" = bye ] && exit 3; done",
            ));
            for l in ["one", "two", "bye"] {
                proc.write_line(l);
            }
            assert!(pump_until(ctx, 10, || exit.get().is_some()), "no exit");
            assert_eq!(exit.get(), Some(Some(3)));
            assert_eq!(*out.borrow(), ["out:one", "out:two", "out:bye"]);
            assert_eq!(*err.borrow(), ["err:one", "err:two", "err:bye"]);
            assert!(!proc.is_alive());
            // A write after the exit is dropped quietly.
            proc.write_line("late");
        });
    }

    #[test]
    fn interrupt_and_terminate_stop_the_process() {
        in_loop(|ctx| {
            // A harness may start the tests with SIGINT ignored (inherited by every child);
            // the interrupt half can only be observed when it is not.
            let sigint_ignored = std::fs::read_to_string("/proc/self/status")
                .ok()
                .and_then(|s| {
                    s.lines()
                        .find_map(|l| l.strip_prefix("SigIgn:"))
                        .and_then(|h| u64::from_str_radix(h.trim(), 16).ok())
                })
                .is_some_and(|mask| mask & 0x2 != 0);
            if !sigint_ignored {
                let (proc, _, _, exit) = start(&sh("exec sleep 30"));
                proc.interrupt();
                assert!(pump_until(ctx, 10, || exit.get().is_some()));
                // Killed by a signal: no exit code.
                assert_eq!(exit.get(), Some(None));
            }

            let (proc, _, _, exit) = start(&sh("exec sleep 30"));
            proc.terminate();
            assert!(pump_until(ctx, 10, || exit.get().is_some()));
            assert_eq!(exit.get(), Some(None));
        });
    }

    /// Whether `pid` is gone (or only a zombie waiting to be reaped).
    fn is_gone(pid: i32) -> bool {
        match std::fs::read_to_string(format!("/proc/{pid}/stat")) {
            Err(_) => true,
            Ok(stat) => stat
                .rsplit(')')
                .next()
                .is_some_and(|rest| rest.trim_start().starts_with('Z')),
        }
    }

    #[test]
    fn terminate_takes_the_agents_tool_children_with_it() {
        in_loop(|ctx| {
            // The "agent" starts a background child, reports its pid, and waits.
            let (proc, out, _, exit) = start(&sh("sleep 60 & echo $!; wait"));
            assert!(pump_until(ctx, 10, || !out.borrow().is_empty()));
            let child: i32 = out.borrow()[0].parse().expect("child pid");
            assert!(!is_gone(child), "child should be running");
            proc.terminate();
            assert!(pump_until(ctx, 10, || exit.get().is_some()));
            assert!(
                pump_until(ctx, 10, || is_gone(child)),
                "the tool child outlived the agent"
            );
        });
    }

    #[test]
    fn extra_env_and_cwd_apply_and_the_rest_is_inherited() {
        in_loop(|ctx| {
            let tmp = tempfile::tempdir().expect("tmp");
            let mut spec =
                sh("echo \"$AT_TEST_VAR\"; pwd; [ -n \"$PATH\" ] && echo path-inherited");
            spec.cwd = Some(tmp.path().to_string_lossy().into_owned());
            spec.env = vec![("AT_TEST_VAR".into(), "hello".into())];
            let (_proc, out, _, exit) = start(&spec);
            assert!(pump_until(ctx, 10, || exit.get().is_some()));
            let want_dir = std::fs::canonicalize(tmp.path()).expect("canon");
            let got = out.borrow().clone();
            assert_eq!(got[0], "hello");
            assert_eq!(
                std::fs::canonicalize(&got[1]).expect("canon"),
                want_dir,
                "{got:?}"
            );
            assert_eq!(got[2], "path-inherited");
        });
    }

    #[test]
    fn a_missing_program_is_an_error_not_a_panic() {
        in_loop(|_| {
            let spec = SpawnSpec {
                argv: vec!["/nonexistent/agent".into()],
                ..SpawnSpec::default()
            };
            assert!(AgentProcess::spawn(&spec, |_| {}, |_| {}, |_| {}).is_err());
            assert!(AgentProcess::spawn(&SpawnSpec::default(), |_| {}, |_| {}, |_| {}).is_err());
        });
    }

    #[test]
    fn run_side_captures_stdout_and_success() {
        in_loop(|ctx| {
            let result: Rc<RefCell<Vec<(String, bool)>>> = Rc::default();
            for script in ["echo hi; exit 0", "echo oops; exit 2"] {
                let r = result.clone();
                glib::spawn_future_local(async move {
                    let got =
                        run_side(vec!["/bin/sh".into(), "-c".into(), script.into()], None).await;
                    r.borrow_mut().push(got);
                });
            }
            let r = result.clone();
            glib::spawn_future_local(async move {
                let got = run_side(vec!["/nonexistent/x".into()], None).await;
                r.borrow_mut().push(got);
            });
            assert!(pump_until(ctx, 10, || result.borrow().len() == 3));
            let mut got = result.borrow().clone();
            got.sort();
            assert!(got.contains(&("hi\n".to_owned(), true)), "{got:?}");
            assert!(got.contains(&("oops\n".to_owned(), false)), "{got:?}");
            assert!(got
                .iter()
                .any(|(s, ok)| !ok && s.contains("could not start")));
        });
    }
}
