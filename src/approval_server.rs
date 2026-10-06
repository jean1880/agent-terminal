//! The unix-socket server behind agy's PreToolUse approval hook.
//!
//! agy runs with `--dangerously-skip-permissions`, so `agent-terminal --approval-hook` (see
//! [`crate::approval_hook`]) is the ONLY gate, and it is installed with a catch-all matcher: every
//! tool call reaches this server, which therefore decides policy per tool and per [`Mode`]
//! ([`policy`]). Read-only tools are answered without bothering the user; mutating or executing
//! tools are denied (plan mode) or put to the user as an `ApprovalRequested` envelope.
//!
//! Wire: one NDJSON [`ApprovalQuery`] line in, one [`ApprovalReply`] line out
//! (`agent_core::approval`). Every path that cannot produce an answer replies deny: fail closed.
//!
//! Socket: `$XDG_RUNTIME_DIR/agent-terminal/approval-<pid>-<thread>.sock`, directory `0700`,
//! socket `0600`; removed when the last [`ApprovalHandle`] clone is dropped. Nobody else can reach
//! the socket even in the instant between `bind` and `chmod`, because the directory is private.
//!
//! Ceiling: a line is read with `DataInputStream` without a length cap. Only the owning user can
//! connect (private directory), so a runaway client is a self-inflicted problem. Upgrade path:
//! read through a bounded buffer.

use std::cell::{Cell, RefCell};
use std::collections::{HashMap, HashSet};
use std::path::{Component, Path, PathBuf};
use std::rc::{Rc, Weak};
use std::time::Duration;

use agent_core::adapter::Mode;
use agent_core::approval::{self, ApprovalQuery, ApprovalReply};
use agent_core::event::{Decision, Envelope, Event, ResponseCapability};
use gtk4::gio::{self, prelude::*};
use gtk4::glib;
use serde_json::Value;
use tracing::{debug, info, warn};

/// Env var naming the absolute path of the running binary, so the hook entry never falls back to
/// an installed (possibly older) `agent-terminal` on PATH.
pub const ENV_HOOK_BIN: &str = "AGENT_TERMINAL_HOOK_BIN";
/// How long the user has to answer. The server replies deny and expires the card at this point,
/// before the hook client's own timeout ([`crate::approval_hook::REPLY_TIMEOUT`], 590 s) and the
/// hook's configured `timeout` (600 s).
pub const DEFAULT_DEADLINE: Duration = Duration::from_secs(570);
/// How long a connected hook may take to send its query.
const QUERY_TIMEOUT: Duration = Duration::from_secs(10);
const IO_PRIORITY: glib::Priority = glib::Priority::DEFAULT;
/// `sockaddr_un.sun_path` holds 108 bytes including the NUL.
const MAX_SOCKET_PATH: usize = 100;

// ---- policy (pure) ----

/// What the server does with one tool call.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Verdict {
    /// Allow without asking.
    Allow,
    /// Put it to the user.
    Ask,
    /// Refuse without asking, with this reason.
    Deny(&'static str),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ToolClass {
    ReadOnly,
    Edit,
    /// Commands, network, MCP, subagents: always ask (or deny in plan mode).
    Exec,
    /// Not in the table: always ask.
    Unknown,
}

fn classify(tool: &str) -> ToolClass {
    match tool {
        "view_file" | "list_dir" | "grep_search" | "find_by_name" | "codebase_search"
        | "view_code_item" | "read_resource" | "list_resources" => ToolClass::ReadOnly,
        t if t.starts_with("get_") => ToolClass::ReadOnly,
        "replace_file_content"
        | "multi_replace_file_content"
        | "write_to_file"
        | "sed_file"
        | "notebook_edit" => ToolClass::Edit,
        "run_command" | "send_command_input" | "call_mcp_tool" | "start_subagent"
        | "invoke_subagent" | "read_url_content" | "search_web" => ToolClass::Exec,
        _ => ToolClass::Unknown,
    }
}

/// Plan mode refuses these outright. Network reads are not mutations, so they are asked about.
fn mutates(tool: &str) -> bool {
    matches!(
        tool,
        "run_command"
            | "send_command_input"
            | "call_mcp_tool"
            | "start_subagent"
            | "invoke_subagent"
    ) || classify(tool) == ToolClass::Edit
}

/// The file an edit tool targets, from the argument names agy uses.
fn edit_target(args: &Value) -> Option<&str> {
    ["TargetFile", "AbsolutePath", "FilePath", "Path"]
        .iter()
        .find_map(|k| args.get(*k).and_then(Value::as_str))
        .filter(|s| !s.is_empty())
}

/// Lexically resolves `.` and `..` (no filesystem access).
fn normalize(path: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for part in path.components() {
        match part {
            Component::ParentDir => {
                out.pop();
            }
            Component::CurDir => {}
            other => out.push(other.as_os_str()),
        }
    }
    out
}

/// Resolves symlinks in the longest existing ancestor, so a link out of the workspace is seen.
fn resolve(path: &Path) -> PathBuf {
    let path = normalize(path);
    let mut tail = Vec::new();
    let mut base = path.as_path();
    loop {
        if let Ok(real) = std::fs::canonicalize(base) {
            let mut real = real;
            real.extend(tail.iter().rev());
            return real;
        }
        match (base.parent(), base.file_name()) {
            (Some(parent), Some(name)) => {
                tail.push(name.to_owned());
                base = parent;
            }
            _ => return path,
        }
    }
}

fn inside_workspace(target: &str, workspaces: &[PathBuf]) -> bool {
    let target = Path::new(target);
    if !target.is_absolute() {
        return false;
    }
    let target = resolve(target);
    workspaces.iter().any(|w| target.starts_with(resolve(w)))
}

/// The policy table. `workspaces` are the session's workspace roots.
///
/// | mode | read-only | file edit | command / network / MCP / subagent | unknown |
/// |---|---|---|---|---|
/// | Plan | allow | deny | deny (network reads: ask) | ask |
/// | Ask | allow | ask | ask | ask |
/// | AcceptEdits | allow | allow inside the workspace, else ask | ask | ask |
pub fn policy(mode: Mode, query: &ApprovalQuery, workspaces: &[PathBuf]) -> Verdict {
    let class = classify(&query.tool);
    match (class, mode) {
        (ToolClass::ReadOnly, _) => Verdict::Allow,
        (_, Mode::Plan) if mutates(&query.tool) => Verdict::Deny("plan mode is read-only"),
        (ToolClass::Edit, Mode::AcceptEdits)
            if edit_target(&query.args).is_some_and(|t| inside_workspace(t, workspaces)) =>
        {
            Verdict::Allow
        }
        _ => Verdict::Ask,
    }
}

/// What "allow for the session" remembers: the tool and, for commands, the first two words.
/// A command with shell chaining or substitution is never remembered: its prefix does not bound
/// what runs. Ceiling: `git status` allows `git status --anything`; upgrade path is per-flag rules.
pub fn session_key(query: &ApprovalQuery) -> Option<(String, String)> {
    let command = query.args.get("CommandLine").and_then(Value::as_str);
    let Some(command) = command else {
        return Some((query.tool.clone(), String::new()));
    };
    const CHAINING: &[&str] = &[";", "&", "|", "`", "$(", ">", "<", "\n", "\r"];
    if CHAINING.iter().any(|c| command.contains(c)) {
        return None;
    }
    let prefix: Vec<&str> = command.split_whitespace().take(2).collect();
    if prefix.is_empty() {
        return None;
    }
    Some((query.tool.clone(), prefix.join(" ")))
}

/// `$XDG_RUNTIME_DIR/agent-terminal`; takes the value so tests never read the real environment.
/// Empty and relative values are ignored, as the XDG spec requires.
pub fn runtime_dir(xdg_runtime_dir: Option<&str>) -> Option<PathBuf> {
    let base = xdg_runtime_dir.filter(|v| Path::new(v).is_absolute())?;
    Some(PathBuf::from(base).join("agent-terminal"))
}

// ---- server ----

type Sink = Rc<dyn Fn(Envelope)>;

struct Pending {
    conn: gio::SocketConnection,
    key: Option<(String, String)>,
}

struct ServerInner {
    path: PathBuf,
    hook_bin: PathBuf,
    listener: gio::SocketListener,
    sink: RefCell<Option<Sink>>,
    pending: RefCell<HashMap<String, Pending>>,
    allowed: RefCell<HashSet<(String, String)>>,
    mode: Cell<Mode>,
    workspaces: Vec<PathBuf>,
    deadline: Duration,
}

impl Drop for ServerInner {
    fn drop(&mut self) {
        self.listener.close();
        if let Err(e) = std::fs::remove_file(&self.path) {
            debug!(error = %e, "approval socket already gone");
        }
    }
}

/// What a session holds: the socket to export, and the means to answer. Cloneable; the socket
/// goes away with the last clone.
#[derive(Clone)]
pub struct ApprovalHandle {
    inner: Rc<ServerInner>,
}

impl ApprovalHandle {
    /// Binds under `$XDG_RUNTIME_DIR/agent-terminal` (see [`runtime_dir`]).
    pub fn bind_default(thread: &str, workspace: &Path, mode: Mode) -> Result<Self, String> {
        let dir = runtime_dir(std::env::var("XDG_RUNTIME_DIR").ok().as_deref())
            .ok_or_else(|| "XDG_RUNTIME_DIR is not set".to_owned())?;
        Self::bind(&dir, thread, workspace, mode, DEFAULT_DEADLINE)
    }

    /// Binds `dir/approval-<pid>-<thread>.sock`. `workspace` bounds `AcceptEdits` auto-allows.
    /// Any failure is an `Err`: the caller then runs agy read-only (no hook, no env).
    pub fn bind(
        dir: &Path,
        thread: &str,
        workspace: &Path,
        mode: Mode,
        deadline: Duration,
    ) -> Result<Self, String> {
        use std::os::unix::fs::{DirBuilderExt, PermissionsExt};
        let hook_bin = std::env::current_exe()
            .map_err(|e| format!("cannot resolve the running binary: {e}"))?;
        std::fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(dir)
            .map_err(|e| format!("cannot create {}: {e}", dir.display()))?;
        std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700))
            .map_err(|e| format!("cannot secure {}: {e}", dir.display()))?;
        let path = dir.join(format!("approval-{}-{thread}.sock", std::process::id()));
        if path.as_os_str().len() > MAX_SOCKET_PATH {
            return Err("approval socket path is too long".to_owned());
        }
        // Our own pid and thread: anything here is a stale leftover.
        let _ = std::fs::remove_file(&path);
        let listener = gio::SocketListener::new();
        listener
            .add_address(
                &gio::UnixSocketAddress::new(&path),
                gio::SocketType::Stream,
                gio::SocketProtocol::Default,
                None::<&glib::Object>,
            )
            .map_err(|e| format!("cannot listen on the approval socket: {}", e.message()))?;
        let inner = Rc::new(ServerInner {
            path,
            hook_bin,
            listener: listener.clone(),
            sink: RefCell::new(None),
            pending: RefCell::new(HashMap::new()),
            allowed: RefCell::new(HashSet::new()),
            mode: Cell::new(mode),
            workspaces: vec![workspace.to_owned()],
            deadline,
        });
        // From here `Drop` removes the file, including on the early return below.
        std::fs::set_permissions(&inner.path, std::fs::Permissions::from_mode(0o600))
            .map_err(|e| format!("cannot secure the approval socket: {e}"))?;
        info!("approval socket listening");
        spawn_accept_loop(Rc::downgrade(&inner), listener);
        Ok(Self { inner })
    }

    pub fn socket_path(&self) -> &Path {
        &self.inner.path
    }

    /// The environment agy must be started with for its hook to find this server. Both values
    /// are non-blank by construction.
    pub fn env(&self) -> Vec<(String, String)> {
        vec![
            (
                approval::ENV_SOCKET.to_owned(),
                self.inner.path.to_string_lossy().into_owned(),
            ),
            (
                ENV_HOOK_BIN.to_owned(),
                self.inner.hook_bin.to_string_lossy().into_owned(),
            ),
        ]
    }

    /// Where `ApprovalRequested` / `ApprovalExpired` envelopes go (the owning session).
    pub fn attach(&self, sink: impl Fn(Envelope) + 'static) {
        *self.inner.sink.borrow_mut() = Some(Rc::new(sink));
    }

    /// The session's current mode (it changes what is asked).
    pub fn set_mode(&self, mode: Mode) {
        self.inner.mode.set(mode);
    }

    pub fn has_pending(&self, request: &str) -> bool {
        self.inner.pending.borrow().contains_key(request)
    }

    /// Answers a pending request. False when it is not (or no longer) pending.
    pub fn respond(&self, request: &str, decision: Decision) -> bool {
        let Some(pending) = self.inner.pending.borrow_mut().remove(request) else {
            return false;
        };
        if decision == Decision::AllowForSession {
            if let Some(key) = pending.key {
                self.inner.allowed.borrow_mut().insert(key);
            }
        }
        reply(
            pending.conn,
            ApprovalReply {
                decision,
                reason: None,
            },
        );
        true
    }

    /// Denies and expires everything still pending (the agent exited or restarted).
    pub fn expire_all(&self, reason: &str) {
        let drained: Vec<(String, Pending)> = self.inner.pending.borrow_mut().drain().collect();
        for (id, pending) in drained {
            reply(
                pending.conn,
                ApprovalReply {
                    decision: Decision::Deny,
                    reason: Some(reason.to_owned()),
                },
            );
            self.inner
                .emit(Envelope::new(Event::ApprovalExpired).request(id));
        }
    }
}

impl ServerInner {
    fn emit(&self, env: Envelope) {
        let sink = self.sink.borrow().clone();
        if let Some(sink) = sink {
            sink(env);
        }
    }
}

fn spawn_accept_loop(weak: Weak<ServerInner>, listener: gio::SocketListener) {
    glib::spawn_future_local(async move {
        loop {
            match listener.accept_future().await {
                Ok((conn, _)) => {
                    if weak.strong_count() == 0 {
                        break;
                    }
                    glib::spawn_future_local(serve(weak.clone(), conn));
                }
                Err(e) => {
                    // Closed on drop; anything else is logged once and ends the loop.
                    debug!(error = %e.message(), "approval accept loop ended");
                    break;
                }
            }
        }
    });
}

/// Writes one reply line, then closes the connection. Never blocks, never panics.
fn reply(conn: gio::SocketConnection, reply: ApprovalReply) {
    glib::spawn_future_local(async move {
        match approval::encode_reply(&reply) {
            Ok(mut line) => {
                line.push('\n');
                let out = conn.output_stream();
                if let Err((_, e)) = out.write_all_future(line.into_bytes(), IO_PRIORITY).await {
                    warn!(error = %e.message(), "approval reply could not be written");
                }
            }
            Err(e) => warn!(error = %e, "approval reply could not be encoded"),
        }
        if let Err(e) = conn.close_future(IO_PRIORITY).await {
            debug!(error = %e.message(), "approval connection close failed");
        }
    });
}

fn deny(conn: gio::SocketConnection, reason: &str) {
    reply(
        conn,
        ApprovalReply {
            decision: Decision::Deny,
            reason: Some(reason.to_owned()),
        },
    );
}

async fn serve(weak: Weak<ServerInner>, conn: gio::SocketConnection) {
    let data = gio::DataInputStream::new(&conn.input_stream());
    let line =
        match glib::future_with_timeout(QUERY_TIMEOUT, data.read_line_future(IO_PRIORITY)).await {
            Ok(Ok(Some(bytes))) => String::from_utf8_lossy(&bytes).into_owned(),
            _ => return deny(conn, "no approval request received"),
        };
    let Some(inner) = weak.upgrade() else {
        return deny(conn, "agent-terminal session closed");
    };
    let query = match approval::parse_query(&line) {
        Ok(q) if !q.id.is_empty() => q,
        _ => return deny(conn, "malformed approval request"),
    };

    match policy(inner.mode.get(), &query, &inner.workspaces) {
        Verdict::Allow => {
            return reply(
                conn,
                ApprovalReply {
                    decision: Decision::Allow,
                    reason: None,
                },
            )
        }
        Verdict::Deny(why) => return deny(conn, why),
        Verdict::Ask => {}
    }
    let key = session_key(&query);
    if key
        .as_ref()
        .is_some_and(|k| inner.allowed.borrow().contains(k))
    {
        return reply(
            conn,
            ApprovalReply {
                decision: Decision::Allow,
                reason: None,
            },
        );
    }
    if inner.sink.borrow().is_none() {
        return deny(conn, "no agent-terminal session is listening");
    }
    if inner.pending.borrow().contains_key(&query.id) {
        return deny(conn, "duplicate approval request id");
    }

    let id = query.id.clone();
    // Registered before the envelope goes out: the sink may answer synchronously.
    inner.pending.borrow_mut().insert(
        id.clone(),
        Pending {
            conn: conn.clone(),
            key,
        },
    );
    let title = query
        .args
        .get("CommandLine")
        .and_then(Value::as_str)
        .map(str::to_owned);
    inner.emit(
        Envelope::new(Event::ApprovalRequested {
            tool: query.tool.clone(),
            title,
            input: query.args.clone(),
            reason: query.cwd.as_ref().map(|c| format!("in {c}")),
            options: vec![Decision::Allow, Decision::AllowForSession, Decision::Deny],
            response: ResponseCapability::Live,
        })
        .request(id.clone()),
    );
    let deadline = inner.deadline;
    drop(inner);

    // The hook sends nothing more. Anything that wakes this read means the hook went away
    // (EOF, error) or misbehaved; a timeout means the user never answered.
    let outcome = glib::future_with_timeout(deadline, data.read_line_future(IO_PRIORITY)).await;
    let Some(inner) = weak.upgrade() else { return };
    let still_pending = inner.pending.borrow_mut().remove(&id);
    let Some(pending) = still_pending else {
        return; // answered; `respond` replied and closed.
    };
    if outcome.is_err() {
        deny(pending.conn, "no answer in agent-terminal");
    }
    inner.emit(Envelope::new(Event::ApprovalExpired).request(id));
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::{in_loop, pump_until};
    use std::io::{BufRead, BufReader, Write};
    use std::os::unix::fs::PermissionsExt;
    use std::os::unix::net::UnixStream;
    use std::sync::mpsc;

    fn q(tool: &str, args: Value) -> ApprovalQuery {
        ApprovalQuery {
            id: "q".into(),
            conversation_id: "c".into(),
            tool: tool.into(),
            args,
            cwd: None,
        }
    }

    fn cmd(line: &str) -> ApprovalQuery {
        q("run_command", serde_json::json!({"CommandLine": line}))
    }

    fn edit(tool: &str, file: &str) -> ApprovalQuery {
        q(tool, serde_json::json!({"TargetFile": file}))
    }

    fn ws() -> Vec<PathBuf> {
        vec![PathBuf::from("/work/repo")]
    }

    #[test]
    fn policy_table() {
        use Verdict::{Allow, Ask, Deny};
        let plan = Mode::Plan;
        let ask = Mode::Ask;
        let acc = Mode::AcceptEdits;
        let w = ws();
        for tool in [
            "view_file",
            "list_dir",
            "grep_search",
            "find_by_name",
            "codebase_search",
            "view_code_item",
            "read_resource",
            "list_resources",
            "get_something",
        ] {
            for mode in [plan, ask, acc] {
                assert_eq!(policy(mode, &q(tool, Value::Null), &w), Allow, "{tool}");
            }
        }
        for tool in [
            "run_command",
            "send_command_input",
            "write_to_file",
            "replace_file_content",
            "multi_replace_file_content",
            "sed_file",
            "notebook_edit",
            "call_mcp_tool",
            "start_subagent",
            "invoke_subagent",
        ] {
            let query = edit(tool, "/work/repo/a.rs");
            assert!(matches!(policy(plan, &query, &w), Deny(_)), "{tool} plan");
            assert_eq!(policy(ask, &query, &w), Ask, "{tool} ask");
        }
        // Network reads are not mutations: asked about, even in plan mode.
        for tool in ["read_url_content", "search_web"] {
            for mode in [plan, ask, acc] {
                assert_eq!(policy(mode, &q(tool, Value::Null), &w), Ask, "{tool}");
            }
        }
        // Unknown tools always ask, in every mode.
        for mode in [plan, ask, acc] {
            assert_eq!(policy(mode, &q("brand_new_tool", Value::Null), &w), Ask);
            assert_eq!(policy(mode, &q("", Value::Null), &w), Ask);
        }
    }

    #[test]
    fn accept_edits_allows_only_edits_inside_the_workspace() {
        use Verdict::{Allow, Ask};
        let w = ws();
        let acc = Mode::AcceptEdits;
        for tool in [
            "write_to_file",
            "replace_file_content",
            "multi_replace_file_content",
            "sed_file",
            "notebook_edit",
        ] {
            assert_eq!(policy(acc, &edit(tool, "/work/repo/src/a.rs"), &w), Allow);
            assert_eq!(
                policy(acc, &edit(tool, "/work/repo/new/dir/b.rs"), &w),
                Allow
            );
            assert_eq!(
                policy(acc, &edit(tool, "/work/repo/../etc/passwd"), &w),
                Ask
            );
            assert_eq!(policy(acc, &edit(tool, "/work/repository/x"), &w), Ask);
            assert_eq!(policy(acc, &edit(tool, "/etc/passwd"), &w), Ask);
            assert_eq!(policy(acc, &edit(tool, "relative.rs"), &w), Ask);
            assert_eq!(policy(acc, &q(tool, Value::Null), &w), Ask, "no target");
        }
        // Commands, network, MCP and subagents still ask.
        assert_eq!(policy(acc, &cmd("ls"), &w), Ask);
        assert_eq!(policy(acc, &q("call_mcp_tool", Value::Null), &w), Ask);
        assert_eq!(policy(acc, &q("invoke_subagent", Value::Null), &w), Ask);
        assert_eq!(policy(acc, &q("search_web", Value::Null), &w), Ask);
    }

    #[test]
    fn a_symlink_out_of_the_workspace_is_not_inside_it() {
        let tmp = tempfile::tempdir().expect("tmp");
        let work = tmp.path().join("work");
        let outside = tmp.path().join("outside");
        std::fs::create_dir_all(&work).expect("mk");
        std::fs::create_dir_all(&outside).expect("mk");
        std::os::unix::fs::symlink(&outside, work.join("link")).expect("link");
        let w = vec![work.clone()];
        let target = |p: PathBuf| edit("write_to_file", &p.to_string_lossy());
        assert_eq!(
            policy(Mode::AcceptEdits, &target(work.join("ok.rs")), &w),
            Verdict::Allow
        );
        assert_eq!(
            policy(Mode::AcceptEdits, &target(work.join("link/evil.rs")), &w),
            Verdict::Ask
        );
    }

    #[test]
    fn session_keys_bound_what_is_remembered() {
        let key = |l: &str| session_key(&cmd(l));
        assert_eq!(
            key("git status --short"),
            Some(("run_command".into(), "git status".into()))
        );
        assert_eq!(key("ls"), Some(("run_command".into(), "ls".into())));
        for chained in [
            "git status; rm -rf x",
            "a && b",
            "a | b",
            "echo `id`",
            "echo $(id)",
            "cat x > y",
            "a\nb",
            "   ",
        ] {
            assert_eq!(key(chained), None, "{chained}");
        }
        assert_eq!(
            session_key(&edit("write_to_file", "/x")),
            Some(("write_to_file".into(), String::new()))
        );
    }

    #[test]
    fn runtime_dir_follows_xdg() {
        assert_eq!(
            runtime_dir(Some("/run/user/1000")),
            Some(PathBuf::from("/run/user/1000/agent-terminal"))
        );
        assert_eq!(runtime_dir(Some("")), None);
        assert_eq!(runtime_dir(Some("relative")), None);
        assert_eq!(runtime_dir(None), None);
    }

    type Seen = Rc<RefCell<Vec<Envelope>>>;

    fn bound(tmp: &Path, mode: Mode, deadline: Duration) -> (ApprovalHandle, Seen) {
        let handle = ApprovalHandle::bind(
            &tmp.join("rt").join("agent-terminal"),
            "t1",
            tmp,
            mode,
            deadline,
        )
        .expect("bind");
        let seen: Seen = Rc::default();
        let s = seen.clone();
        handle.attach(move |e| s.borrow_mut().push(e));
        (handle, seen)
    }

    /// Sends one query from a thread (a real separate client); its reply line arrives on the
    /// returned channel, which the test polls while pumping the loop.
    fn client(path: PathBuf, query: ApprovalQuery) -> mpsc::Receiver<String> {
        let (tx, rx) = mpsc::channel();
        std::thread::spawn(move || {
            let mut s = UnixStream::connect(&path).expect("connect");
            s.set_read_timeout(Some(Duration::from_secs(15)))
                .expect("timeout");
            let line = approval::encode_query(&query).expect("encode");
            writeln!(s, "{line}").expect("send");
            let mut reply = String::new();
            BufReader::new(s).read_line(&mut reply).expect("reply");
            tx.send(reply).expect("deliver");
        });
        rx
    }

    fn wait_reply(ctx: &glib::MainContext, rx: &mpsc::Receiver<String>) -> ApprovalReply {
        let got: RefCell<Option<String>> = RefCell::new(None);
        assert!(pump_until(ctx, 10, || {
            if let Ok(line) = rx.try_recv() {
                *got.borrow_mut() = Some(line);
            }
            got.borrow().is_some()
        }));
        let line = got.borrow().clone().expect("reply");
        approval::parse_reply(&line).expect("reply json")
    }

    fn request_id(seen: &Seen) -> Option<String> {
        seen.borrow()
            .iter()
            .find(|e| matches!(e.event, Event::ApprovalRequested { .. }))
            .and_then(|e| e.request.clone())
    }

    fn with_id(mut query: ApprovalQuery, id: &str) -> ApprovalQuery {
        query.id = id.into();
        query
    }

    #[test]
    fn round_trip_then_session_memory_then_socket_removal() {
        in_loop(|ctx| {
            let tmp = tempfile::tempdir().expect("tmp");
            let (handle, seen) = bound(tmp.path(), Mode::Ask, DEFAULT_DEADLINE);
            let path = handle.socket_path().to_owned();
            let mode = |p: &Path| std::fs::metadata(p).expect("meta").permissions().mode() & 0o777;
            assert_eq!(mode(path.parent().expect("dir")), 0o700);
            assert_eq!(mode(&path), 0o600);
            let name = path.file_name().expect("n").to_string_lossy().into_owned();
            assert_eq!(name, format!("approval-{}-t1.sock", std::process::id()));
            let env = handle.env();
            assert_eq!(env[0].0, approval::ENV_SOCKET);
            assert_eq!(env[0].1, path.to_string_lossy());
            assert_eq!(env[1].0, ENV_HOOK_BIN);
            assert!(Path::new(&env[1].1).is_absolute());

            let rx = client(path.clone(), with_id(cmd("git status --short"), "q1"));
            assert!(pump_until(ctx, 10, || request_id(&seen).is_some()));
            {
                let seen = seen.borrow();
                let Event::ApprovalRequested {
                    tool,
                    options,
                    response,
                    input,
                    ..
                } = &seen[0].event
                else {
                    panic!("first envelope: {:?}", seen[0]);
                };
                assert_eq!(tool, "run_command");
                assert_eq!(input["CommandLine"], "git status --short");
                assert_eq!(
                    options,
                    &[Decision::Allow, Decision::AllowForSession, Decision::Deny]
                );
                assert_eq!(*response, ResponseCapability::Live);
                assert_eq!(seen[0].request.as_deref(), Some("q1"));
            }
            assert!(handle.has_pending("q1"));
            assert!(handle.respond("q1", Decision::AllowForSession));
            assert!(!handle.respond("q1", Decision::Allow), "answered once");
            assert_eq!(wait_reply(ctx, &rx).decision, Decision::AllowForSession);

            // The same command prefix is now allowed without a new card.
            let rx = client(path.clone(), with_id(cmd("git status -b"), "q2"));
            assert_eq!(wait_reply(ctx, &rx).decision, Decision::Allow);
            assert_eq!(seen.borrow().len(), 1, "no second card");
            // A different command still asks.
            let rx = client(path.clone(), with_id(cmd("git push"), "q3"));
            assert!(pump_until(ctx, 10, || seen.borrow().len() == 2));
            assert!(handle.respond("q3", Decision::Deny));
            assert_eq!(wait_reply(ctx, &rx).decision, Decision::Deny);

            drop(handle);
            assert!(!path.exists(), "socket removed on drop");
        });
    }

    #[test]
    fn policy_decides_without_a_card_where_it_can() {
        in_loop(|ctx| {
            let tmp = tempfile::tempdir().expect("tmp");
            let (handle, seen) = bound(tmp.path(), Mode::Plan, DEFAULT_DEADLINE);
            let path = handle.socket_path().to_owned();
            let rx = client(path.clone(), with_id(cmd("rm -rf /"), "p1"));
            let r = wait_reply(ctx, &rx);
            assert_eq!(r.decision, Decision::Deny);
            assert!(r.reason.is_some_and(|s| s.contains("plan")));
            let rx = client(path.clone(), with_id(q("view_file", Value::Null), "p2"));
            assert_eq!(wait_reply(ctx, &rx).decision, Decision::Allow);
            assert!(seen.borrow().is_empty());

            // The mode can change under a live server.
            handle.set_mode(Mode::Ask);
            let rx = client(path, with_id(cmd("ls"), "p3"));
            assert!(pump_until(ctx, 10, || request_id(&seen).is_some()));
            assert!(handle.respond("p3", Decision::Allow));
            assert_eq!(wait_reply(ctx, &rx).decision, Decision::Allow);
        });
    }

    #[test]
    fn an_unanswered_request_is_denied_and_expired_at_the_deadline() {
        in_loop(|ctx| {
            let tmp = tempfile::tempdir().expect("tmp");
            let (handle, seen) = bound(tmp.path(), Mode::Ask, Duration::from_millis(200));
            let rx = client(handle.socket_path().to_owned(), with_id(cmd("ls"), "d1"));
            let r = wait_reply(ctx, &rx);
            assert_eq!(r.decision, Decision::Deny);
            assert_eq!(r.reason.as_deref(), Some("no answer in agent-terminal"));
            assert!(pump_until(ctx, 5, || seen.borrow().len() == 2));
            assert_eq!(seen.borrow()[1].event, Event::ApprovalExpired);
            assert_eq!(seen.borrow()[1].request.as_deref(), Some("d1"));
            assert!(!handle.has_pending("d1"));
        });
    }

    #[test]
    fn a_vanished_hook_and_a_dead_agent_expire_the_card() {
        in_loop(|ctx| {
            let tmp = tempfile::tempdir().expect("tmp");
            let (handle, seen) = bound(tmp.path(), Mode::Ask, DEFAULT_DEADLINE);
            // The hook connects, asks, then dies without waiting.
            let mut s = UnixStream::connect(handle.socket_path()).expect("connect");
            let line = approval::encode_query(&with_id(cmd("ls"), "g1")).expect("encode");
            writeln!(s, "{line}").expect("send");
            assert!(pump_until(ctx, 10, || request_id(&seen).is_some()));
            drop(s);
            assert!(pump_until(ctx, 10, || seen.borrow().len() == 2));
            assert_eq!(seen.borrow()[1].event, Event::ApprovalExpired);

            // expire_all denies what is pending when the agent exits.
            let rx = client(handle.socket_path().to_owned(), with_id(cmd("ls"), "g2"));
            assert!(pump_until(ctx, 10, || seen.borrow().len() == 3));
            handle.expire_all("agent exited");
            assert_eq!(wait_reply(ctx, &rx).decision, Decision::Deny);
            assert!(pump_until(ctx, 10, || seen.borrow().len() == 4));
            assert_eq!(seen.borrow()[3].event, Event::ApprovalExpired);
        });
    }

    #[test]
    fn malformed_requests_are_denied() {
        in_loop(|ctx| {
            let tmp = tempfile::tempdir().expect("tmp");
            let (handle, seen) = bound(tmp.path(), Mode::Ask, DEFAULT_DEADLINE);
            let mut s = UnixStream::connect(handle.socket_path()).expect("connect");
            s.set_read_timeout(Some(Duration::from_secs(15)))
                .expect("timeout");
            writeln!(s, "not json").expect("send");
            let (tx, rx) = mpsc::channel();
            std::thread::spawn(move || {
                let mut reply = String::new();
                let _ = BufReader::new(s).read_line(&mut reply);
                let _ = tx.send(reply);
            });
            assert_eq!(wait_reply(ctx, &rx).decision, Decision::Deny);
            assert!(seen.borrow().is_empty());
        });
    }
}
