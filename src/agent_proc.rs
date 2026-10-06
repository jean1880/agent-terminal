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
/// How long quitting the app waits for its agents to leave after SIGTERM.
const SHUTDOWN_GRACE: Duration = Duration::from_secs(2);

thread_local! {
    /// Process groups of the agents not yet seen to exit, for [`terminate_all`].
    static LIVE: RefCell<Vec<i32>> = const { RefCell::new(Vec::new()) };
}

/// What to run.
#[derive(Debug, Clone, Default)]
pub struct SpawnSpec {
    /// Program first. Looked up in `PATH` when it has no slash.
    pub argv: Vec<String>,
    pub cwd: Option<String>,
    /// Added to the user's own environment (which is inherited), overriding on conflict.
    pub env: Vec<(String, String)>,
    /// Removed from the inherited environment before `env` is applied (a launching agent
    /// session's own markers, the config's `clear_env`).
    pub unset: Vec<String>,
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
        for key in &spec.unset {
            launcher.unsetenv(key);
        }
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
        if let Some(pid) = pid {
            LIVE.with(|l| l.borrow_mut().push(pid));
        }
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

/// Stops every agent this app started, for its shutdown: SIGTERM to each process group, a wait of
/// at most [`SHUTDOWN_GRACE`], then SIGKILL to the groups whose agent is still running. Blocks:
/// the main loop is over by then, so [`terminate`]'s timer would never fire (and agents run in
/// their own session, so nothing else would stop them).
///
/// A group is signalled only while its leader is unreaped (running, or a zombie no one reaps once
/// the loop is over), so a reused pid is never hit. Ceiling: a tool child that left the group
/// (`setsid`) is not reached; see [`Shared::signal`].
pub fn terminate_all() {
    let groups = LIVE.with(|l| std::mem::take(&mut *l.borrow_mut()));
    let signal_group = |pid: i32, signal: i32| {
        // SAFETY: kill(2) with a negative pid signals that process group; no memory is touched.
        unsafe { libc::kill(-pid, signal) };
    };
    let groups: Vec<i32> = groups
        .into_iter()
        .filter(|pid| leader_state(*pid).is_some())
        .collect();
    if groups.is_empty() {
        return;
    }
    info!(count = groups.len(), "stopping agents on quit");
    for pid in &groups {
        signal_group(*pid, SIGTERM);
    }
    let deadline = std::time::Instant::now() + SHUTDOWN_GRACE;
    let running = |pid: &i32| leader_state(*pid).is_some_and(|s| s != 'Z');
    while groups.iter().any(running) && std::time::Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(50));
    }
    for pid in &groups {
        match leader_state(*pid) {
            None => {}
            Some(state) => {
                if state != 'Z' {
                    warn!(pid, "agent ignored SIGTERM on quit; killing it");
                }
                // Also takes any tool child still in the group with it.
                signal_group(*pid, SIGKILL);
            }
        }
    }
}

/// The state letter (`R`, `S`, `Z`, …) of `pid` from `/proc`; `None` once it has been reaped.
fn leader_state(pid: i32) -> Option<char> {
    let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    stat.rsplit(')').next()?.trim_start().chars().next()
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
        if let Some(pid) = shared.pid {
            LIVE.with(|l| l.borrow_mut().retain(|p| *p != pid));
        }
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

/// How long `agy models` / `agy -p /usage` may take (shared by the catalogue and the usage probe).
pub const AGY_TIMEOUT: Duration = Duration::from_secs(20);

/// The environment an agent's processes get beyond the user's own: what its profile adds (the env
/// file) and what the app removes (`clear_env`, an approval socket inherited from a launching
/// session). Threads, side processes and the background probes all use the same value, so the
/// model list and usage indicator come from the account a thread would use.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct AgentEnv {
    pub env: Vec<(String, String)>,
    pub unset: Vec<String>,
}

impl AgentEnv {
    fn apply(&self, launcher: &gio::SubprocessLauncher) {
        for key in &self.unset {
            launcher.unsetenv(key);
        }
        for (key, value) in &self.env {
            launcher.setenv(key, value, true);
        }
    }
}

/// Runs a one-shot side process (agy `-p /model --output-format json`, `agy models`) and returns
/// its stdout and whether it exited with status 0. A failure to start is `("<reason>", false)`.
///
/// Its stdin is `/dev/null` (never the app's), and it is killed when `timeout` passes: dropping
/// the `communicate` future would leave the child running, so the kill is explicit. The result is
/// then `("timed out", false)`.
pub async fn run_side(
    argv: Vec<String>,
    cwd: Option<String>,
    env: &AgentEnv,
    timeout: Duration,
) -> (String, bool) {
    let Some(program) = argv.first() else {
        return ("empty command line".to_owned(), false);
    };
    let name = Path::new(program)
        .file_name()
        .map_or_else(|| program.clone(), |n| n.to_string_lossy().into_owned());
    // Neither STDIN_PIPE nor STDIN_INHERIT: GIO gives the child /dev/null.
    let launcher = gio::SubprocessLauncher::new(
        gio::SubprocessFlags::STDOUT_PIPE | gio::SubprocessFlags::STDERR_SILENCE,
    );
    if let Some(cwd) = &cwd {
        launcher.set_cwd(cwd);
    }
    env.apply(&launcher);
    let args: Vec<&OsStr> = argv.iter().map(|a| OsStr::new(a.as_str())).collect();
    let subprocess = match launcher.spawn(&args) {
        Ok(s) => s,
        Err(e) => {
            warn!(program = %name, error = %e.message(), "side process did not start");
            return (format!("could not start {name}: {}", e.message()), false);
        }
    };
    let finished = glib::future_with_timeout(timeout, subprocess.communicate_utf8_future(None));
    match finished.await {
        Ok(Ok((stdout, _))) => (
            stdout.map(|s| s.to_string()).unwrap_or_default(),
            subprocess.is_successful(),
        ),
        Ok(Err(e)) => {
            warn!(program = %name, error = %e.message(), "side process failed");
            (e.message().to_owned(), false)
        }
        Err(_) => {
            warn!(program = %name, "side process timed out; killing it");
            subprocess.force_exit();
            ("timed out".to_owned(), false)
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
        leader_state(pid).is_none_or(|s| s == 'Z')
    }

    #[test]
    fn quitting_stops_every_agent_even_one_that_ignores_sigterm() {
        in_loop(|ctx| {
            let (polite, out, _, _) = start(&sh("echo $$; exec sleep 30"));
            // SIGTERM ignored, inherited by its sleep too: only the SIGKILL ends it.
            let (stubborn, out2, _, _) =
                start(&sh("trap '' TERM; echo $$; while :; do sleep 1; done"));
            assert!(pump_until(ctx, 10, || !out.borrow().is_empty()
                && !out2.borrow().is_empty()));
            let pids: Vec<i32> = [&out, &out2]
                .iter()
                .map(|o| o.borrow()[0].parse().expect("pid"))
                .collect();
            let started = std::time::Instant::now();
            // The main loop is not running here, as at shutdown.
            terminate_all();
            assert!(started.elapsed() < SHUTDOWN_GRACE + Duration::from_secs(1));
            for pid in pids {
                assert!(
                    pump_until(ctx, 10, || is_gone(pid)),
                    "agent {pid} outlived quit"
                );
            }
            // Nothing is left to stop.
            assert!(LIVE.with(|l| l.borrow().is_empty()));
            drop((polite, stubborn));
        });
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

    fn side(script: &str, env: AgentEnv, timeout: Duration) -> (String, bool) {
        let script = script.to_owned();
        in_loop(|ctx| {
            let out: Rc<RefCell<Option<(String, bool)>>> = Rc::default();
            let o = out.clone();
            glib::spawn_future_local(async move {
                let got = run_side(
                    vec!["/bin/sh".into(), "-c".into(), script],
                    None,
                    &env,
                    timeout,
                )
                .await;
                *o.borrow_mut() = Some(got);
            });
            assert!(pump_until(ctx, 15, || out.borrow().is_some()), "no result");
            let got = out.borrow_mut().take().expect("result");
            got
        })
    }

    #[test]
    fn a_side_process_that_outlives_its_timeout_is_killed() {
        let dir = tempfile::tempdir().expect("tmp");
        let pidfile = dir.path().join("pid");
        let script = format!("echo $$ > '{}'; exec sleep 60", pidfile.display());
        let started = std::time::Instant::now();
        let (text, ok) = side(&script, AgentEnv::default(), Duration::from_millis(400));
        assert!(!ok);
        assert_eq!(text, "timed out");
        assert!(started.elapsed() < Duration::from_secs(10));
        let pid: i32 = std::fs::read_to_string(&pidfile)
            .expect("pid file")
            .trim()
            .parse()
            .expect("pid");
        // The kill is asynchronous (SIGKILL, then the reaper): give it a moment.
        let mut gone = false;
        for _ in 0..50 {
            if is_gone(pid) {
                gone = true;
                break;
            }
            std::thread::sleep(Duration::from_millis(100));
        }
        assert!(gone, "the timed-out child kept running");
    }

    #[test]
    fn a_side_process_reads_end_of_file_not_the_apps_stdin_and_gets_the_agent_env() {
        let env = AgentEnv {
            env: vec![("AT_SIDE_VAR".into(), "set".into())],
            unset: vec!["HOME".into()],
        };
        // `read` returns at once only when stdin is closed or null.
        let (out, ok) = side(
            "read x; echo \"eof:$AT_SIDE_VAR:${HOME-unset}\"",
            env,
            Duration::from_secs(10),
        );
        assert!(ok, "{out}");
        assert_eq!(out.trim(), "eof:set:unset");
    }

    #[test]
    fn run_side_captures_stdout_and_success() {
        in_loop(|ctx| {
            let result: Rc<RefCell<Vec<(String, bool)>>> = Rc::default();
            for script in ["echo hi; exit 0", "echo oops; exit 2"] {
                let r = result.clone();
                glib::spawn_future_local(async move {
                    let got = run_side(
                        vec!["/bin/sh".into(), "-c".into(), script.into()],
                        None,
                        &AgentEnv::default(),
                        Duration::from_secs(10),
                    )
                    .await;
                    r.borrow_mut().push(got);
                });
            }
            let r = result.clone();
            glib::spawn_future_local(async move {
                let got = run_side(
                    vec!["/nonexistent/x".into()],
                    None,
                    &AgentEnv::default(),
                    Duration::from_secs(10),
                )
                .await;
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
