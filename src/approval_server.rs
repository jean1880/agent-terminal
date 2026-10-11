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
use std::collections::HashMap;
use std::os::unix::fs::MetadataExt;
use std::path::{Component, Path, PathBuf};
use std::rc::{Rc, Weak};
use std::sync::atomic::{AtomicU64, Ordering};
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
pub enum ToolClass {
    ReadOnly,
    Edit,
    /// Commands, network, MCP, subagents: always ask (or deny in plan mode).
    Exec,
    /// Not in the table: always ask.
    Unknown,
}

pub fn classify(tool: &str) -> ToolClass {
    match tool {
        "view_file" | "list_dir" | "grep_search" | "find_by_name" | "codebase_search"
        | "view_code_item" | "read_resource" | "list_resources" => ToolClass::ReadOnly,
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

pub const TARGET_KEYS: [&str; 9] = [
    "TargetFile",
    "target_file",
    "AbsolutePath",
    "absolute_path",
    "FilePath",
    "file_path",
    "Path",
    "path",
    "file",
];

/// Every file an edit tool names, from the argument names agy uses. A payload with several
/// of them is judged on all of them.
pub fn edit_targets(args: &Value) -> Vec<&str> {
    TARGET_KEYS
        .iter()
        .filter_map(|k| args.get(*k).and_then(Value::as_str))
        .collect()
}

fn has_parent_dir(path: &Path) -> bool {
    path.components().any(|c| c == Component::ParentDir)
}

/// Whether `target` is inside `workspace` with no symlink on the way.
///
/// The target must be absolute and contain no `..` (a link followed by `..` escapes in ways a
/// lexical check cannot see). Starting from the workspace root (itself resolved: the user may
/// open a symlinked project path), every component below it that exists is looked at with
/// `symlink_metadata`, and any symlink, including a dangling leaf, means "not inside". Components
/// that do not exist yet are fine: that is a file about to be created.
fn contained(target: &Path, workspace: &Path) -> bool {
    if !target.is_absolute() || has_parent_dir(target) {
        return false;
    }
    let Ok(root) = std::fs::canonicalize(workspace) else {
        return false;
    };
    let Ok(relative) = target
        .strip_prefix(workspace)
        .or_else(|_| target.strip_prefix(&root))
    else {
        return false;
    };
    let mut current = root;
    for part in relative.components() {
        match part {
            Component::Normal(name) => current.push(name),
            Component::CurDir => continue,
            _ => return false,
        }
        match std::fs::symlink_metadata(&current) {
            Ok(meta) if meta.file_type().is_symlink() => return false,
            // A second hard link may name a file outside the workspace.
            Ok(meta) if meta.is_file() && meta.nlink() > 1 => return false,
            Ok(_) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return true,
            Err(_) => return false,
        }
    }
    true
}

/// Directory or file names that run code outside the agent when found anywhere below the
/// workspace: git hooks and config (`.git`, also a worktree's `.git` file), direnv, editor task
/// and launch configs, dev containers, hook managers, Cargo's `runner`/`build` config.
const SENSITIVE_NAMES: &[&str] = &[
    ".git",
    ".envrc",
    ".direnv",
    ".vscode",
    ".idea",
    ".devcontainer",
    ".husky",
    ".pre-commit-config.yaml",
    ".cargo",
    ".gitlab-ci.yml",
    ".circleci",
    // Project-level agent config: hooks, permissions and MCP servers the next agent run obeys.
    ".claude",
    ".gemini",
    ".agents",
    ".mcp.json",
    ".codex",
    // Other version control, editor and tool configs that run code.
    ".hg",
    ".zed",
    ".exrc",
    ".nvim.lua",
    "mise.toml",
    // Agent instruction files: the next agent run follows what they say. Names are compared
    // lowercased.
    "agents.md",
    "claude.md",
    "gemini.md",
];

/// `target` below `base`, as components, trying `base` as given and then resolved (the target
/// may be named by either the user's path or the real one).
fn relative_to(target: &Path, base: &Path) -> Option<PathBuf> {
    if let Ok(rel) = target.strip_prefix(base) {
        return Some(rel.to_owned());
    }
    let real = std::fs::canonicalize(base).ok()?;
    target.strip_prefix(real).ok().map(Path::to_owned)
}

fn names(rel: &Path) -> Vec<String> {
    rel.components()
        .filter_map(|c| match c {
            Component::Normal(n) => Some(n.to_string_lossy().to_ascii_lowercase()),
            _ => None,
        })
        .collect()
}

/// Whether editing `target` (inside `workspace`) can run code outside the agent, so that
/// `AcceptEdits` must still ask. Lexical, on top of [`contained`].
///
/// Sensitive: any path with a component in [`SENSITIVE_NAMES`]; `.github/workflows/**`; and,
/// when the workspace is `home` or an ancestor of it, a top-level dotfile or anything under a
/// top-level dot-directory of `home` (shell rc files, `~/.config` autostart and systemd user
/// units, `~/.ssh`, `~/.gemini/config/hooks.json`, which is this gate's own wiring, `~/.claude`).
/// Paths under `home` that do not start with a dot (`~/projects/x`) are ordinary.
fn sensitive_target(target: &Path, workspace: &Path, home: Option<&Path>) -> bool {
    if let Some(rel) = relative_to(target, workspace) {
        let parts = names(&rel);
        if parts.iter().any(|p| SENSITIVE_NAMES.contains(&p.as_str()))
            || parts
                .windows(2)
                .any(|w| w[0] == ".github" && w[1] == "workflows")
        {
            return true;
        }
    }
    let Some(home) = home else {
        return false;
    };
    // The workspace is `home` or above it; a project below `home` is not exposed to the dotfiles.
    let root = std::fs::canonicalize(workspace).unwrap_or_else(|_| workspace.to_owned());
    let real_home = std::fs::canonicalize(home).unwrap_or_else(|_| home.to_owned());
    if !(home.starts_with(workspace) || real_home.starts_with(&root)) {
        return false;
    }
    relative_to(target, home)
        .and_then(|rel| names(&rel).into_iter().next())
        .is_some_and(|first| first.starts_with('.'))
}

/// Every named target is inside one workspace root (and there is at least one) and is not a
/// [`sensitive_target`] there.
fn auto_allowed(targets: &[&str], workspaces: &[PathBuf], home: Option<&Path>) -> bool {
    !targets.is_empty()
        && targets.iter().all(|t| {
            let t = Path::new(t);
            workspaces
                .iter()
                .any(|w| contained(t, w) && !sensitive_target(t, w, home))
        })
}

/// Extra argument names the read tools use for the directory or file they look at.
const READ_PATH_KEYS: [&str; 6] = [
    "DirectoryPath",
    "directory_path",
    "SearchPath",
    "search_path",
    "SearchDirectory",
    "Directory",
];

/// Argument names whose value is a list of paths (`codebase_search` takes `TargetDirectories`).
const READ_LIST_KEYS: [&str; 5] = [
    "TargetDirectories",
    "target_directories",
    "SearchPaths",
    "Paths",
    "Directories",
];

/// Tools that walk a tree and read file contents: pointing one at `$HOME` (or above) reads
/// `~/.ssh` and `~/.gemini_env` without ever naming them.
fn searches_recursively(tool: &str) -> bool {
    matches!(tool, "grep_search" | "find_by_name" | "codebase_search")
}

/// Every path a read call names, or `None` when any of them is not text we can judge: a present
/// key whose value is not a string (or a list of strings) fails closed.
pub(crate) fn named_paths(args: &Value) -> Option<Vec<&str>> {
    let mut out = Vec::new();
    for key in TARGET_KEYS.iter().chain(READ_PATH_KEYS.iter()) {
        if let Some(v) = args.get(*key) {
            out.push(v.as_str()?);
        }
    }
    for key in READ_LIST_KEYS {
        if let Some(v) = args.get(key) {
            for item in v.as_array()? {
                out.push(item.as_str()?);
            }
        }
    }
    Some(out)
}

/// Whether `dir` is `home` or one of its ancestors (compared resolved as well as lexically).
fn is_home_or_above(dir: &Path, home: &Path) -> bool {
    let real_dir = std::fs::canonicalize(dir).unwrap_or_else(|_| dir.to_owned());
    let real_home = std::fs::canonicalize(home).unwrap_or_else(|_| home.to_owned());
    home.starts_with(dir) || real_home.starts_with(&real_dir)
}

/// Whether a read-only call may run unasked: every path it names is inside a workspace root,
/// with no symlink or `..` on the way, and not a [`sensitive_target`]. Relative paths are taken
/// from `cwd`. Fails closed on non-text values, empty strings, `~` and `$` (which no shell
/// expansion here will resolve), `read_resource` (a URI can name any file) and tools that must
/// name a path but do not. A search with no path searches `cwd`, which must then be inside a
/// workspace; a recursive search never runs unasked from `$HOME` or an ancestor of it.
fn read_allowed(query: &ApprovalQuery, workspaces: &[PathBuf], home: Option<&Path>) -> bool {
    let tool = query.tool.as_str();
    if tool == "read_resource" {
        return false;
    }
    let Some(named) = named_paths(&query.args) else {
        return false;
    };
    if named
        .iter()
        .any(|t| t.trim().is_empty() || t.starts_with('~') || t.contains('$'))
    {
        return false;
    }
    let inside = |full: &Path| {
        workspaces
            .iter()
            .any(|w| contained(full, w) && !sensitive_target(full, w, home))
    };
    let cwd = query.cwd.as_deref().map(Path::new);
    let mut fulls: Vec<PathBuf> = Vec::new();
    for t in &named {
        let path = Path::new(t);
        fulls.push(if path.is_absolute() {
            path.to_path_buf()
        } else if let Some(cwd) = cwd {
            cwd.join(path)
        } else {
            return false;
        });
    }
    if named.is_empty() {
        match tool {
            // No path argument at all: nothing to disclose.
            "list_resources" => return true,
            "grep_search" | "find_by_name" | "codebase_search" => {
                let Some(cwd) = cwd else { return false };
                fulls.push(cwd.to_path_buf());
            }
            _ => return false,
        }
    }
    if searches_recursively(tool)
        && home.is_some_and(|h| fulls.iter().any(|f| is_home_or_above(f, h)))
    {
        return false;
    }
    fulls.iter().all(|f| inside(f))
}

/// [`policy_with_home`] with no home directory (tests of the workspace rules alone).
#[cfg(test)]
pub fn policy(mode: Mode, query: &ApprovalQuery, workspaces: &[PathBuf]) -> Verdict {
    policy_with_home(mode, query, workspaces, None)
}

/// The policy table. `workspaces` are the session's workspace roots; `home` is the user's home
/// directory, for the dotfile rule of [`sensitive_target`].
///
/// | mode | read-only | file edit | command / network / MCP / subagent | unknown |
/// |---|---|---|---|---|
/// | Plan | allow inside the workspace unless sensitive, else ask | deny | deny (network reads: ask) | ask |
/// | Ask | allow inside the workspace unless sensitive, else ask | ask | ask | ask |
/// | AcceptEdits | allow inside the workspace unless sensitive, else ask | allow inside the workspace unless sensitive, else ask | ask | ask |
pub fn policy_with_home(
    mode: Mode,
    query: &ApprovalQuery,
    workspaces: &[PathBuf],
    home: Option<&Path>,
) -> Verdict {
    let class = classify(&query.tool);
    match (class, mode) {
        (ToolClass::ReadOnly, _) if read_allowed(query, workspaces, home) => Verdict::Allow,
        // Reads outside the workspace, of sensitive targets or through a symlink ask in every
        // mode, Plan included: plan mode refuses mutation, not disclosure.
        (ToolClass::ReadOnly, _) => Verdict::Ask,
        (_, Mode::Plan) if mutates(&query.tool) => Verdict::Deny("plan mode is read-only"),
        (ToolClass::Edit, Mode::AcceptEdits)
            if auto_allowed(&edit_targets(&query.args), workspaces, home) =>
        {
            Verdict::Allow
        }
        _ => Verdict::Ask,
    }
}

/// Resolves symlinks in the longest existing ancestor of an absolute, `..`-free path.
fn resolve(path: &Path) -> PathBuf {
    let mut tail = Vec::new();
    let mut base = path;
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
            _ => return path.to_owned(),
        }
    }
}

/// `$XDG_RUNTIME_DIR/agent-terminal`; takes the value so tests never read the real environment.
/// Empty and relative values are ignored, as the XDG spec requires.
pub fn runtime_dir(xdg_runtime_dir: Option<&str>) -> Option<PathBuf> {
    let base = xdg_runtime_dir.filter(|v| Path::new(v).is_absolute())?;
    Some(PathBuf::from(base).join("agent-terminal"))
}

/// Whether an approval socket can be bound here: `$XDG_RUNTIME_DIR` is an absolute directory.
/// (Binding itself happens per thread; this is the settings-time proof the runtime is usable.)
pub fn socket_bindable() -> bool {
    let xdg = std::env::var("XDG_RUNTIME_DIR").ok();
    runtime_dir(xdg.as_deref())
        .and_then(|d| d.parent().map(Path::to_owned))
        .is_some_and(|base| base.is_dir())
}

// ---- server ----

type Sink = Rc<dyn Fn(Envelope)>;

struct Pending {
    conn: gio::SocketConnection,
    /// The call in canonical terms, when a rule can describe it.
    action: Option<crate::always_allow::Action>,
    cwd: Option<String>,
}

/// One thread's session rules, shared by its chat session (which clears them on a cross-agent
/// switch) and agy's approval server.
pub type SessionRules = Rc<RefCell<crate::always_allow::AlwaysRules>>;

struct ServerInner {
    path: PathBuf,
    hook_bin: PathBuf,
    listener: gio::SocketListener,
    sink: RefCell<Option<Sink>>,
    pending: RefCell<HashMap<String, Pending>>,
    mode: Cell<Mode>,
    workspaces: Vec<PathBuf>,
    /// The user's home, for the dotfile rule of [`sensitive_target`]; `None` applies no such rule.
    home: Option<PathBuf>,
    deadline: Duration,
    /// Queries received and not yet matched to a tool step by the session's canary.
    queries: RefCell<HashMap<String, u32>>,
    /// The "Always allow" rules, cached ([`crate::always_allow::StoreCache`]); `None` offers no
    /// "Always".
    always: RefCell<Option<crate::always_allow::StoreCache>>,
    /// The thread's session rules (its own until a chat session shares its set).
    session_rules: RefCell<SessionRules>,
    /// The unusable-rules-file notice was already shown by this server.
    blocked_noticed: Cell<bool>,
}

impl Drop for ServerInner {
    fn drop(&mut self) {
        self.listener.close();
        // Nobody is left to answer: deny what is waiting rather than leave a hook hanging.
        let waiting: Vec<Pending> = self.pending.get_mut().drain().map(|(_, p)| p).collect();
        for pending in waiting {
            deny(pending.conn, "agent-terminal session closed");
        }
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
        let hook_bin = std::env::current_exe()
            .map_err(|e| format!("cannot resolve the running binary: {e}"))?;
        let home = std::env::var_os("HOME")
            .map(PathBuf::from)
            .filter(|h| h.is_absolute());
        let handle = Self::bind_with_home(
            &dir,
            thread,
            workspace,
            mode,
            DEFAULT_DEADLINE,
            hook_bin,
            home,
        )?;
        if let Some(path) = crate::always_allow::path() {
            handle.use_always_rules(path);
        }
        Ok(handle)
    }

    /// Keeps "Always allow" rules in `path`: offers "Always allow" from now on and auto-allows
    /// what its rules match (read at each request: a few rules of JSON, not worth a worker).
    pub fn use_always_rules(&self, path: PathBuf) {
        *self.inner.always.borrow_mut() = Some(crate::always_allow::StoreCache::new(path));
    }

    /// Uses the chat session's session rules (so a same-agent restart keeps them and a
    /// cross-agent switch clears them).
    pub fn share_session_rules(&self, rules: SessionRules) {
        *self.inner.session_rules.borrow_mut() = rules;
    }

    /// Binds `dir/approval-<pid>-<thread>.sock`. `workspace` bounds `AcceptEdits` auto-allows.
    /// Any failure is an `Err`: the caller then runs agy read-only (no hook, no env).
    /// Tests only: no home directory, so no dotfile rule (`bind_default` supplies `$HOME`).
    #[cfg(test)]
    pub fn bind(
        dir: &Path,
        thread: &str,
        workspace: &Path,
        mode: Mode,
        deadline: Duration,
    ) -> Result<Self, String> {
        let hook_bin = std::env::current_exe()
            .map_err(|e| format!("cannot resolve the running binary: {e}"))?;
        Self::bind_with(dir, thread, workspace, mode, deadline, hook_bin)
    }

    /// [`Self::bind`] with the hook binary given (tests; `bind` passes `current_exe()`).
    ///
    /// Refused, so the caller runs agy read-only: a workspace that is empty, relative or `/`
    /// (it would make every edit "inside"); a hook binary whose path ends in ` (deleted)` (the
    /// package was upgraded under the running app, and the hook entry would exec a stale or
    /// missing file).
    #[cfg(test)]
    pub fn bind_with(
        dir: &Path,
        thread: &str,
        workspace: &Path,
        mode: Mode,
        deadline: Duration,
        hook_bin: PathBuf,
    ) -> Result<Self, String> {
        Self::bind_with_home(dir, thread, workspace, mode, deadline, hook_bin, None)
    }

    /// [`Self::bind_with`] with the user's home given: `bind_default` reads it from the
    /// environment, tests pass their own so none reads the real one.
    pub fn bind_with_home(
        dir: &Path,
        thread: &str,
        workspace: &Path,
        mode: Mode,
        deadline: Duration,
        hook_bin: PathBuf,
        home: Option<PathBuf>,
    ) -> Result<Self, String> {
        use std::os::unix::fs::{DirBuilderExt, PermissionsExt};
        if hook_bin.to_string_lossy().ends_with(" (deleted)") {
            return Err("the running binary was replaced on disk; restart agent-terminal".into());
        }
        if !workspace.is_absolute() || workspace.parent().is_none() {
            return Err("the workspace is not a usable directory".to_owned());
        }
        std::fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(dir)
            .map_err(|e| format!("cannot create {}: {e}", dir.display()))?;
        std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700))
            .map_err(|e| format!("cannot secure {}: {e}", dir.display()))?;
        // Unique per bind (a counter), and the thread id is untrusted text in a path: keep
        // alphanumerics, `-` and `_`, and a bounded length.
        static BINDS: AtomicU64 = AtomicU64::new(0);
        let label: String = thread
            .chars()
            .filter(|c| c.is_ascii_alphanumeric() || *c == '-' || *c == '_')
            .take(16)
            .collect();
        let label = if label.is_empty() {
            "t".to_owned()
        } else {
            label
        };
        let path = dir.join(format!(
            "approval-{}-{}-{label}.sock",
            std::process::id(),
            BINDS.fetch_add(1, Ordering::Relaxed)
        ));
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
            mode: Cell::new(mode),
            workspaces: vec![workspace.to_owned()],
            home,
            deadline,
            queries: RefCell::new(HashMap::new()),
            always: RefCell::new(None),
            session_rules: RefCell::new(SessionRules::default()),
            blocked_noticed: Cell::new(false),
        });
        // From here `Drop` removes the file, including on the early return below.
        std::fs::set_permissions(&inner.path, std::fs::Permissions::from_mode(0o600))
            .map_err(|e| format!("cannot secure the approval socket: {e}"))?;
        info!("approval socket listening");
        spawn_accept_loop(Rc::downgrade(&inner), listener);
        Ok(Self { inner })
    }

    #[cfg_attr(not(test), allow(dead_code))] // exercised by tests; kept as API
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

    /// Uses up one recorded hook query for any of `tools`. The session calls this when a tool
    /// step starts: no query means agy ran the tool without asking us (the hook is not active).
    pub fn consume_query(&self, tools: &[&str]) -> bool {
        let mut queries = self.inner.queries.borrow_mut();
        for tool in tools {
            if let Some(n) = queries.get_mut(*tool).filter(|n| **n > 0) {
                *n -= 1;
                return true;
            }
        }
        false
    }

    /// [`Self::bind_default`] only when `hook` says agy's hooks file installs the approval hook
    /// (the result of [`crate::hook_config::check_installed`], which reads a file and so is run
    /// off the main thread by the caller). Fail-closed: an `Err` verdict (including "not checked
    /// yet") binds nothing and means: run agy read-only.
    pub fn bind_checked(
        hook: Result<(), String>,
        thread: &str,
        workspace: &Path,
        mode: Mode,
    ) -> Result<Self, String> {
        hook?;
        Self::bind_default(thread, workspace, mode)
    }

    #[cfg_attr(not(test), allow(dead_code))] // exercised by tests; kept as API
    pub fn has_pending(&self, request: &str) -> bool {
        self.inner.pending.borrow().contains_key(request)
    }

    /// Answers a pending request, optionally saving a custom rule for AllowAlways.
    pub fn respond_with_rule(
        &self,
        request: &str,
        decision: Decision,
        custom_rule: Option<crate::always_allow::Rule>,
    ) -> bool {
        let Some(pending) = self.inner.pending.borrow_mut().remove(request) else {
            return false;
        };
        let mut decision = decision;
        if matches!(decision, Decision::AllowForSession | Decision::AllowAlways)
            && !self.inner.remember(&pending, decision, custom_rule)
        {
            decision = Decision::Allow;
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

    /// Answers a pending request. False when it is not (or no longer) pending.
    #[allow(dead_code)]
    pub fn respond(&self, request: &str, decision: Decision) -> bool {
        self.respond_with_rule(request, decision, None)
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
    /// Where a call is judged by the rules: agy, the session's workspace, and the call's cwd (the
    /// workspace when agy names none).
    fn context<'a>(&'a self, cwd: Option<&'a str>) -> Option<crate::always_allow::Context<'a>> {
        let workspace = self.workspaces.first()?;
        Some(crate::always_allow::Context {
            driver: "agy",
            workspace,
            cwd: cwd.or_else(|| workspace.to_str()),
            home: self.home.as_deref(),
        })
    }

    /// Whether an Always rule allows `action` (from the cached store). An unusable file allows
    /// nothing, and its notice names the file once per server.
    fn saved_allow(
        &self,
        action: &crate::always_allow::Action,
        ctx: &crate::always_allow::Context,
    ) -> bool {
        let mut cache = self.always.borrow_mut();
        let Some(cache) = cache.as_mut() else {
            return false;
        };
        let path = cache.path().to_owned();
        match cache.rules() {
            Ok(rules) => rules.matches_any(action, ctx),
            Err(e) => {
                let notice = crate::always_allow::blocked_notice(&path, e);
                warn!(error = %e, "approval rules file unusable; saved rules are off");
                if !self.blocked_noticed.replace(true) {
                    self.emit(Envelope::new(Event::Notice { text: notice }));
                }
                false
            }
        }
    }

    /// Whether a session rule (agy's own) or an Always rule allows `action`.
    fn rules_allow(&self, action: &crate::always_allow::Action, cwd: Option<&str>) -> bool {
        let Some(ctx) = self.context(cwd) else {
            return false;
        };
        let session = self.session_rules.borrow().clone();
        let in_session = session.borrow().matches_session(action, &ctx);
        in_session || self.saved_allow(action, &ctx)
    }

    /// Records what "Allow for session" / "Always allow" keeps for `pending`, judged afresh by
    /// [`crate::always_allow::decide`] whatever the card offered. False (said in a notice) when
    /// nothing is kept: the call is allowed once. An Always rule holds for the session at once
    /// and is saved off the main thread.
    fn remember(
        self: &Rc<Self>,
        pending: &Pending,
        decision: Decision,
        custom: Option<crate::always_allow::Rule>,
    ) -> bool {
        use crate::always_allow::Scope;
        let Some(ctx) = self.context(pending.cwd.as_deref()) else {
            return false;
        };
        let scope = if decision == Decision::AllowAlways {
            Scope::Always
        } else {
            Scope::Session
        };
        let rule = match crate::always_allow::decide(pending.action.as_ref(), custom, scope, &ctx) {
            Ok(rule) => rule,
            Err(why) => {
                self.emit(Envelope::new(Event::Notice {
                    text: format!("Allowed once; no rule was kept: {why}."),
                }));
                return false;
            }
        };
        let path = self
            .always
            .borrow()
            .as_ref()
            .map(|cache| cache.path().to_owned());
        if let (Scope::Always, Some(path)) = (scope, path) {
            let weak = Rc::downgrade(self);
            let saved = rule.clone();
            glib::spawn_future_local(async move {
                let target = path.clone();
                let result = gio::spawn_blocking(move || {
                    crate::always_allow::AlwaysRules::remember(&target, saved)
                })
                .await;
                let Some(inner) = weak.upgrade() else { return };
                match result {
                    Ok(Ok(_)) => info!("remembered an always-allow rule"),
                    Ok(Err(e)) => {
                        warn!(error = %e, "always-allow rule not saved");
                        inner.emit(Envelope::new(Event::Notice {
                            text: crate::always_allow::blocked_notice(&path, &e),
                        }));
                    }
                    Err(_) => warn!("the always-allow save worker panicked"),
                }
            });
        }
        let session = self.session_rules.borrow().clone();
        session.borrow_mut().add(rule);
        true
    }

    fn emit(&self, env: Envelope) {
        let sink = self.sink.borrow().clone();
        if let Some(sink) = sink {
            sink(masked_for_view(env));
        }
    }
}

/// An envelope with every secret masked, for the view: the same scrub the store applies before
/// it persists (`scrub_envelope`: every string, `raw` included). The answer path keeps the
/// unmasked call; only what is shown is scrubbed. Masking is idempotent.
///
/// Ceiling: it works on one envelope, so a secret split across two `ContentDelta` chunks is
/// shown as typed here and is only caught when the store heals the item at completion.
pub(crate) fn masked_for_view(env: Envelope) -> Envelope {
    agent_kit::store::scrub_envelope(env.clone()).unwrap_or_else(|e| {
        tracing::warn!(error = %e, "could not mask an event");
        // Fail closed: keep the shape and the buttons, drop everything free-form.
        let mut env = env;
        env.raw = None;
        match &mut env.event {
            Event::ApprovalRequested {
                title,
                input,
                reason,
                remembers,
                ..
            } => {
                *title = None;
                *input = Value::Null;
                *reason = None;
                *remembers = None;
            }
            event => {
                *event = Event::Notice {
                    text: "(output withheld: it could not be masked)".to_owned(),
                }
            }
        }
        env
    })
}

fn spawn_accept_loop(weak: Weak<ServerInner>, listener: gio::SocketListener) {
    glib::spawn_future_local(async move {
        loop {
            match listener.accept_future().await {
                Ok((conn, _)) => {
                    if weak.strong_count() == 0 {
                        deny(conn, "agent-terminal session closed");
                        break;
                    }
                    glib::spawn_future_local(serve(weak.clone(), conn));
                }
                Err(e)
                    if e.matches(gio::IOErrorEnum::Closed)
                        || e.matches(gio::IOErrorEnum::Cancelled) =>
                {
                    debug!("approval accept loop ended");
                    break;
                }
                Err(e) => {
                    // A transient failure (a client that vanished mid-accept, fd pressure)
                    // must not leave the gate deaf: log and keep listening.
                    warn!(error = %e.message(), "approval accept failed; continuing");
                    glib::timeout_future(Duration::from_millis(100)).await;
                    if weak.strong_count() == 0 {
                        break;
                    }
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

    *inner
        .queries
        .borrow_mut()
        .entry(query.tool.clone())
        .or_insert(0) += 1;
    match policy_with_home(
        inner.mode.get(),
        &query,
        &inner.workspaces,
        inner.home.as_deref(),
    ) {
        Verdict::Allow => {
            return reply(
                conn,
                ApprovalReply {
                    decision: Decision::Allow,
                    reason: None,
                },
            );
        }
        Verdict::Deny(why) => return deny(conn, why),
        Verdict::Ask => {}
    }
    // One matcher for every agent (`always_allow`): the call in canonical terms, judged by the
    // thread's session rules (agy's own) and the Always rules.
    let action = crate::always_allow::Action::of(&query.tool, &query.args);
    if action
        .as_ref()
        .is_some_and(|a| inner.rules_allow(a, query.cwd.as_deref()))
    {
        info!(tool = %query.tool, "auto-approving a call a remembered rule matches");
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
    let (options, remembers) = rule_options(
        action.as_ref(),
        inner.context(query.cwd.as_deref()).as_ref(),
        inner.always.borrow().is_some(),
    );
    // Registered before the envelope goes out: the sink may answer synchronously.
    inner.pending.borrow_mut().insert(
        id.clone(),
        Pending {
            conn: conn.clone(),
            action,
            cwd: query.cwd.clone(),
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
            options,
            response: ResponseCapability::Live,
            remembers,
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

/// Whether a call naming `target` may be matched by a path rule: [`contained`] in `workspace`
/// (absolute, no `..`, no symlink or extra hard link on the way) and not a [`sensitive_target`].
/// The rule engine's path check; the same one agy's Accept-edits policy uses.
pub(crate) fn path_auto_matchable(target: &Path, workspace: &Path, home: Option<&Path>) -> bool {
    contained(target, workspace) && !sensitive_target(target, workspace, home)
}

/// `path` with symlinks resolved in its longest existing ancestor: how path rules are compared.
pub(crate) fn canonical_target(path: &Path) -> PathBuf {
    resolve(path)
}

/// The buttons agy's card offers, and what "Always allow" would keep: Allow; "Allow for
/// session" when a rule could match the call ([`crate::always_allow::offerable`]: no secret, a
/// simple command, a contained path); "Always allow" when a rules file is configured and the
/// call is one [`crate::always_allow::always_offerable`] keeps (the predicate the rule is then
/// checked by, so the button never promises what is refused); Deny.
fn rule_options(
    action: Option<&crate::always_allow::Action>,
    ctx: Option<&crate::always_allow::Context>,
    always: bool,
) -> (Vec<Decision>, Option<String>) {
    let mut options = vec![Decision::Allow];
    let mut remembers = None;
    if let (Some(action), Some(ctx)) = (action, ctx) {
        if crate::always_allow::offerable(action, ctx) {
            options.push(Decision::AllowForSession);
            if always && crate::always_allow::always_offerable(action) {
                options.push(Decision::AllowAlways);
                remembers = Some(remembers_text(action, ctx));
            }
        }
    }
    options.push(Decision::Deny);
    (options, remembers)
}

/// What an approval card says "Always allow" keeps for `action`.
pub(crate) fn remembers_text(
    action: &crate::always_allow::Action,
    ctx: &crate::always_allow::Context,
) -> String {
    match action {
        crate::always_allow::Action::Shell { command } => {
            let words = crate::always_allow::command_tokens(command)
                .map_or_else(|| command.clone(), |argv| argv.join(" "));
            let cwd = ctx
                .cwd
                .map_or_else(|| ctx.workspace.to_string_lossy(), std::borrow::Cow::from);
            format!("`{words}` in {cwd}")
        }
        crate::always_allow::Action::Edit { path } | crate::always_allow::Action::Read { path } => {
            path.clone()
        }
        crate::always_allow::Action::Mcp { server, tool } => format!("{server}/{tool}"),
    }
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
        ] {
            for mode in [plan, ask, acc] {
                // A read that names no path: allowed only where the path is optional.
                // Searches with no path and no cwd, and read_resource, ask too.
                let want = if tool == "list_resources" { Allow } else { Ask };
                assert_eq!(policy(mode, &q(tool, Value::Null), &w), want, "{tool}");
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
        // No prefix rule: a `get_*` name is not read-only because of how it starts.
        for mode in [plan, ask, acc] {
            assert_eq!(policy(mode, &q("get_secrets", Value::Null), &w), Ask);
            assert_eq!(policy(mode, &q("brand_new_tool", Value::Null), &w), Ask);
            assert_eq!(policy(mode, &q("", Value::Null), &w), Ask);
        }
    }

    #[test]
    fn policy_table_read_containment() {
        use Verdict::{Allow, Ask};
        let tmp = tempfile::tempdir().expect("tmp");
        let repo = tmp.path().join("repo");
        std::fs::create_dir_all(repo.join("src")).expect("mk");
        std::fs::create_dir_all(tmp.path().join("repo-sibling")).expect("mk");
        std::fs::write(repo.join("src/a.rs"), "x").expect("write");
        std::fs::write(tmp.path().join("repo-sibling/secret"), "x").expect("write");
        std::os::unix::fs::symlink("/etc/hostname", repo.join("link")).expect("link");
        std::os::unix::fs::symlink(tmp.path().join("repo-sibling"), repo.join("dirlink"))
            .expect("link");
        std::fs::create_dir_all(repo.join(".git")).expect("mk");
        let w = vec![repo.clone()];
        let home = tmp.path().join("home");
        std::fs::create_dir_all(home.join(".ssh")).expect("mk");
        let read = |p: &str| q("view_file", serde_json::json!({"AbsolutePath": p}));
        let at = |rel: &str| repo.join(rel).to_string_lossy().into_owned();
        let ssh = home.join(".ssh/id_rsa").to_string_lossy().into_owned();
        let sibling = tmp
            .path()
            .join("repo-sibling/secret")
            .to_string_lossy()
            .into_owned();
        for mode in [Mode::Plan, Mode::Ask, Mode::AcceptEdits] {
            let check = |query: &ApprovalQuery| policy_with_home(mode, query, &w, Some(&home));
            assert_eq!(check(&read(&at("src/a.rs"))), Allow, "{mode:?} in project");
            assert_eq!(
                check(&read(&at("src/new.rs"))),
                Allow,
                "{mode:?} not yet there"
            );
            assert_eq!(check(&read(&ssh)), Ask, "{mode:?} ssh key");
            assert_eq!(check(&read(&sibling)), Ask, "{mode:?} sibling dir");
            assert_eq!(check(&read("/etc/hostname")), Ask, "{mode:?} etc");
            assert_eq!(check(&read(&at("link"))), Ask, "{mode:?} symlink file");
            assert_eq!(
                check(&read(&at("dirlink/secret"))),
                Ask,
                "{mode:?} symlink dir"
            );
            assert_eq!(
                check(&read(&at("../repo-sibling/secret"))),
                Ask,
                "{mode:?} dotdot"
            );
            assert_eq!(check(&read(&at(".git/config"))), Ask, "{mode:?} sensitive");
            assert_eq!(check(&read("src/a.rs")), Ask, "{mode:?} relative, no cwd");
            // One bad path among several spoils the call.
            let two = q(
                "list_dir",
                serde_json::json!({"DirectoryPath": at("src"), "Path": "/etc"}),
            );
            assert_eq!(check(&two), Ask, "{mode:?} mixed");
            let list = q("list_dir", serde_json::json!({"DirectoryPath": at("src")}));
            assert_eq!(check(&list), Allow, "{mode:?} list in project");
            let mut rel = read("src/a.rs");
            rel.cwd = Some(repo.to_string_lossy().into_owned());
            assert_eq!(check(&rel), Allow, "{mode:?} relative with cwd");
        }
    }

    #[test]
    fn read_containment_fails_closed_on_odd_arguments() {
        use Verdict::{Allow, Ask};
        let tmp = tempfile::tempdir().expect("tmp");
        let repo = tmp.path().join("repo");
        std::fs::create_dir_all(repo.join("src")).expect("mk");
        let w = vec![repo.clone()];
        let home = tmp.path().join("home");
        std::fs::create_dir_all(&home).expect("mk");
        let inside = repo.join("src").to_string_lossy().into_owned();
        let repo_s = repo.to_string_lossy().into_owned();
        let with_cwd = |tool: &str, args: Value, cwd: &str| {
            let mut query = q(tool, args);
            query.cwd = Some(cwd.to_owned());
            query
        };
        for mode in [Mode::Plan, Mode::Ask, Mode::AcceptEdits] {
            let check = |query: &ApprovalQuery| policy_with_home(mode, query, &w, Some(&home));
            let view = |v: Value| q("view_file", serde_json::json!({ "AbsolutePath": v }));
            // `~` and `$` are never expanded here.
            assert_eq!(check(&view("~/.ssh/id_rsa".into())), Ask, "{mode:?} tilde");
            assert_eq!(check(&view("$HOME/.bashrc".into())), Ask, "{mode:?} dollar");
            assert_eq!(
                check(&view(format!("{inside}/$X").into())),
                Ask,
                "{mode:?} $ inside"
            );
            // Non-text, empty and list values.
            assert_eq!(check(&view(serde_json::json!(7))), Ask, "{mode:?} number");
            assert_eq!(check(&view(Value::Null)), Ask, "{mode:?} null");
            assert_eq!(
                check(&view(serde_json::json!(["/etc/passwd"]))),
                Ask,
                "{mode:?} array"
            );
            assert_eq!(check(&view("".into())), Ask, "{mode:?} empty");
            assert_eq!(check(&view("   ".into())), Ask, "{mode:?} blank");
            // A list of paths is judged element by element.
            let dirs = |v: Value| {
                q(
                    "codebase_search",
                    serde_json::json!({"TargetDirectories": v}),
                )
            };
            assert_eq!(
                check(&dirs(serde_json::json!([inside]))),
                Allow,
                "{mode:?} list ok"
            );
            assert_eq!(
                check(&dirs(serde_json::json!([inside, "/etc"]))),
                Ask,
                "{mode:?} list outside"
            );
            assert_eq!(
                check(&dirs(serde_json::json!([inside, 3]))),
                Ask,
                "{mode:?} list non-string"
            );
            assert_eq!(
                check(&dirs(serde_json::json!("x"))),
                Ask,
                "{mode:?} list as string"
            );
            // A search path outside the workspace.
            let grep = |p: &str| q("grep_search", serde_json::json!({"SearchPath": p}));
            assert_eq!(check(&grep(&inside)), Allow, "{mode:?} grep inside");
            assert_eq!(check(&grep("/etc")), Ask, "{mode:?} grep outside");
            // No path: the cwd decides.
            let bare = |cwd: &str| with_cwd("grep_search", serde_json::json!({"Query": "x"}), cwd);
            assert_eq!(check(&bare(&repo_s)), Allow, "{mode:?} cwd inside");
            assert_eq!(check(&bare("/etc")), Ask, "{mode:?} cwd outside");
            assert_eq!(
                check(&q("find_by_name", Value::Null)),
                Ask,
                "{mode:?} no cwd"
            );
            // Resources: a URI can name any file.
            let res = q(
                "read_resource",
                serde_json::json!({"Uri": "file:///etc/shadow"}),
            );
            assert_eq!(check(&res), Ask, "{mode:?} read_resource");
            assert_eq!(
                check(&q("list_resources", Value::Null)),
                Allow,
                "{mode:?} list_resources"
            );
        }
    }

    #[test]
    fn recursive_search_from_home_or_above_asks_but_list_dir_does_not() {
        use Verdict::{Allow, Ask};
        let tmp = tempfile::tempdir().expect("tmp");
        let home = tmp.path().join("home");
        std::fs::create_dir_all(home.join("proj")).expect("mk");
        // The workspace is the home directory itself.
        let w = vec![home.clone()];
        let home_s = home.to_string_lossy().into_owned();
        let proj = home.join("proj").to_string_lossy().into_owned();
        for mode in [Mode::Plan, Mode::Ask, Mode::AcceptEdits] {
            let check = |query: &ApprovalQuery| policy_with_home(mode, query, &w, Some(&home));
            for tool in ["grep_search", "find_by_name", "codebase_search"] {
                let at = |p: &str| q(tool, serde_json::json!({"SearchPath": p}));
                assert_eq!(check(&at(&home_s)), Ask, "{mode:?} {tool} home");
                assert_eq!(
                    check(&at(&tmp.path().to_string_lossy())),
                    Ask,
                    "{mode:?} {tool} above home"
                );
                assert_eq!(
                    check(&at(&proj)),
                    Allow,
                    "{mode:?} {tool} project below home"
                );
                let mut bare = q(tool, serde_json::json!({"Query": "x"}));
                bare.cwd = Some(home_s.clone());
                assert_eq!(check(&bare), Ask, "{mode:?} {tool} bare in home");
            }
            let list = q("list_dir", serde_json::json!({"DirectoryPath": proj}));
            assert_eq!(check(&list), Allow, "{mode:?} list_dir below home");
        }
    }

    #[test]
    fn w0_agy_fixture_read_outside_the_project_asks_in_every_mode() {
        let raw = include_str!("../tests/fixtures/real/agy/read-outside-project.jsonl");
        let step: Value = raw
            .lines()
            .filter_map(|l| serde_json::from_str::<Value>(l).ok())
            .find(|v| v["step_update"]["tool_name"] == "view_file")
            .expect("view_file step in fixture");
        let info = &step["step_update"]["tool_info"];
        let query = q("view_file", info["parameters"].clone());
        assert_eq!(edit_targets(&query.args), vec!["/etc/hostname"]);
        let tmp = tempfile::tempdir().expect("tmp");
        let w = vec![tmp.path().to_owned()];
        for mode in [Mode::Plan, Mode::Ask, Mode::AcceptEdits] {
            assert_eq!(policy(mode, &query, &w), Verdict::Ask, "{mode:?}");
        }
    }

    #[test]
    fn accept_edits_allows_only_edits_inside_the_workspace() {
        use Verdict::{Allow, Ask};
        let tmp = tempfile::tempdir().expect("tmp");
        let repo = tmp.path().join("repo");
        std::fs::create_dir_all(repo.join("src")).expect("mk");
        let w = vec![repo.clone()];
        let at = |rel: &str| repo.join(rel).to_string_lossy().into_owned();
        let acc = Mode::AcceptEdits;
        for tool in [
            "write_to_file",
            "replace_file_content",
            "multi_replace_file_content",
            "sed_file",
            "notebook_edit",
        ] {
            assert_eq!(policy(acc, &edit(tool, &at("src/a.rs")), &w), Allow);
            assert_eq!(policy(acc, &edit(tool, &at("new/dir/b.rs")), &w), Allow);
            assert_eq!(policy(acc, &edit(tool, &at("../etc/passwd")), &w), Ask);
            assert_eq!(policy(acc, &edit(tool, &at("src/../a.rs")), &w), Ask);
            let sibling = format!("{}ository/x", repo.display());
            assert_eq!(policy(acc, &edit(tool, &sibling), &w), Ask);
            assert_eq!(policy(acc, &edit(tool, "/etc/passwd"), &w), Ask);
            assert_eq!(policy(acc, &edit(tool, "relative.rs"), &w), Ask);
            assert_eq!(policy(acc, &q(tool, Value::Null), &w), Ask, "no target");
        }
        // Commands, network, MCP and subagents still ask.
        let w = ws();
        assert_eq!(policy(acc, &cmd("ls"), &w), Ask);
        assert_eq!(policy(acc, &q("call_mcp_tool", Value::Null), &w), Ask);
        assert_eq!(policy(acc, &q("invoke_subagent", Value::Null), &w), Ask);
        assert_eq!(policy(acc, &q("search_web", Value::Null), &w), Ask);
    }

    #[test]
    fn accept_edits_asks_for_paths_that_run_code_outside_the_agent() {
        use Verdict::{Allow, Ask};
        let tmp = tempfile::tempdir().expect("tmp");
        let repo = tmp.path().join("repo");
        std::fs::create_dir_all(repo.join("src")).expect("mk");
        let w = vec![repo.clone()];
        let at = |rel: &str| edit("write_to_file", &repo.join(rel).to_string_lossy());
        let acc = Mode::AcceptEdits;
        assert_eq!(policy(acc, &at("src/main.rs"), &w), Allow);
        assert_eq!(policy(acc, &at("src/.github.rs"), &w), Allow);
        for rel in [
            ".git/hooks/pre-commit",
            ".git/config",
            "sub/.git/hooks/post-merge",
            ".envrc",
            ".direnv/x",
            ".vscode/tasks.json",
            ".idea/workspace.xml",
            ".github/workflows/ci.yml",
            ".husky/pre-push",
            ".cargo/config.toml",
            ".GIT/config",
            ".claude/settings.json",
            ".gemini/config/hooks.json",
            ".agents/x",
            ".mcp.json",
        ] {
            assert_eq!(policy(acc, &at(rel), &w), Ask, "{rel}");
        }
        // Other .github files are ordinary.
        assert_eq!(policy(acc, &at(".github/CODEOWNERS"), &w), Allow);
        // Plan still denies and Ask still asks, sensitive or not.
        assert!(matches!(
            policy(Mode::Plan, &at(".git/config"), &w),
            Verdict::Deny(_)
        ));
        assert_eq!(policy(Mode::Ask, &at("src/main.rs"), &w), Ask);
    }

    #[test]
    fn a_workspace_that_is_home_asks_for_dotfiles_and_dot_directories() {
        use Verdict::{Allow, Ask};
        let tmp = tempfile::tempdir().expect("tmp");
        let home = tmp.path().join("home");
        std::fs::create_dir_all(home.join("projects/x/src")).expect("mk");
        let acc = Mode::AcceptEdits;
        let at = |rel: &str| edit("write_to_file", &home.join(rel).to_string_lossy());
        for w in [vec![home.clone()], vec![tmp.path().to_owned()]] {
            let p = |q: &ApprovalQuery| policy_with_home(acc, q, &w, Some(&home));
            for rel in [
                ".bashrc",
                ".profile",
                ".config/autostart/x.desktop",
                ".config/systemd/user/a.service",
                ".local/share/applications/a.desktop",
                ".ssh/authorized_keys",
                ".gemini/config/hooks.json",
                ".claude/settings.json",
            ] {
                assert_eq!(p(&at(rel)), Ask, "{rel}");
            }
            assert_eq!(p(&at("projects/x/src/a.rs")), Allow);
            assert_eq!(p(&at("notes.txt")), Allow);
            assert_eq!(p(&at("projects/x/.git/config")), Ask);
        }
        // A project below home is not exposed to the dotfile rule, and an unknown home adds none.
        let project = vec![home.join("projects/x")];
        let target = edit(
            "write_to_file",
            &home.join("projects/x/.config/a").to_string_lossy(),
        );
        assert_eq!(policy_with_home(acc, &target, &project, Some(&home)), Allow);
        assert_eq!(
            policy_with_home(acc, &at(".bashrc"), std::slice::from_ref(&home), None),
            Allow
        );
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
        // A link followed by `..` is refused outright, not resolved.
        let dotdot = format!("{}/link/../ok.rs", work.display());
        assert_eq!(
            policy(Mode::AcceptEdits, &edit("write_to_file", &dotdot), &w),
            Verdict::Ask
        );
        // The link itself, and a dangling one: both are symlinks, so not inside.
        assert_eq!(
            policy(Mode::AcceptEdits, &target(work.join("link")), &w),
            Verdict::Ask
        );
        std::os::unix::fs::symlink(outside.join("nope"), work.join("dangling")).expect("link");
        assert_eq!(
            policy(Mode::AcceptEdits, &target(work.join("dangling")), &w),
            Verdict::Ask
        );
        // A symlinked workspace root is resolved once and trusted (the user opened it).
        let alias = tmp.path().join("alias");
        std::os::unix::fs::symlink(&work, &alias).expect("link");
        assert_eq!(
            policy(
                Mode::AcceptEdits,
                &target(alias.join("fine.rs")),
                std::slice::from_ref(&alias)
            ),
            Verdict::Allow
        );
        // A hard link to a file outside is a regular file with nlink 2: not inside.
        let secret = outside.join("secret");
        std::fs::write(&secret, "x").expect("write");
        std::fs::hard_link(&secret, work.join("hl")).expect("hardlink");
        assert_eq!(
            policy(Mode::AcceptEdits, &target(work.join("hl")), &w),
            Verdict::Ask
        );
        std::fs::write(work.join("plain"), "x").expect("write");
        assert_eq!(
            policy(Mode::AcceptEdits, &target(work.join("plain")), &w),
            Verdict::Allow
        );
        // Several path keys: ALL must be inside.
        let both = |second: String| {
            q(
                "write_to_file",
                serde_json::json!({
                    "TargetFile": work.join("a.rs").to_string_lossy(),
                    "AbsolutePath": second,
                }),
            )
        };
        let inside = work.join("b.rs").to_string_lossy().into_owned();
        assert_eq!(policy(Mode::AcceptEdits, &both(inside), &w), Verdict::Allow);
        assert_eq!(
            policy(Mode::AcceptEdits, &both("/etc/passwd".into()), &w),
            Verdict::Ask
        );
    }

    #[test]
    fn plain_rules_refuse_wrappers_runners_and_shell_syntax() {
        use crate::always_allow::rememberable_command;
        assert!(rememberable_command("git status --short"));
        assert!(rememberable_command("/usr/bin/zsh -lc 'git status'"));
        for refused in [
            "make test",
            "cargo build",
            "npm test",
            "pytest -x",
            "docker ps",
            "uv run x",
            "go test ./...",
            "./run.sh",
            "/usr/bin/ls",
            "bin/tool",
            "git diff",
            "git log -p",
            "git show HEAD",
            "git difftool",
            "git commit -m x",
            "git -c core.pager=x status",
            "git status -c x",
            "git status; rm -rf x",
            "a && b",
            "a | b",
            "echo `id`",
            "echo $(id)",
            "echo $HOME",
            "cat x > y",
            "a\nb",
            "   ",
            "python -c x",
            "python3.12 script.py",
            "/usr/bin/python3 -c x",
            "node -e x",
            "bash -xc ls",
            "bash -c 'make test'",
            "sh run.sh",
            "env FOO=1 ls",
            "sudo ls",
            "xargs rm",
            "nohup ls",
            "timeout 5 ls",
            "watch ls",
            "eval ls",
            "FOO=1 ls",
            "find . -name x -exec rm {} +",
            "find . -delete",
        ] {
            assert!(!rememberable_command(refused), "{refused}");
        }
        // Plain find is fine.
        assert!(rememberable_command("find . -name x"));
        // send_command_input, subagents, network and unknown tools are no action a rule names.
        for tool in [
            "send_command_input",
            "start_subagent",
            "invoke_subagent",
            "search_web",
            "read_url_content",
            "mystery",
        ] {
            let args = serde_json::json!({"CommandLine": "ls", "TargetFile": "/x"});
            assert_eq!(crate::always_allow::Action::of(tool, &args), None, "{tool}");
        }
    }

    #[test]
    fn edit_rules_are_the_exact_canonical_path_inside_the_workspace() {
        use crate::always_allow::{Action, Context, Rule};
        let tmp = tempfile::tempdir().expect("tmp");
        let real = std::fs::canonicalize(tmp.path()).expect("canon");
        let ctx = Context {
            driver: "agy",
            workspace: &real,
            cwd: None,
            home: None,
        };
        let edit = |p: &Path| Action::Edit {
            path: p.to_string_lossy().into_owned(),
        };
        let a = Rule::from_action(&edit(&real.join("a.rs")), &ctx).expect("rule a");
        assert!(a.matches(&edit(&real.join("a.rs")), &ctx));
        assert!(!a.matches(&edit(&real.join("b.rs")), &ctx));
        // Relative (no cwd) and `..` paths are never remembered.
        assert!(Rule::from_action(&edit(Path::new("a.rs")), &ctx).is_err());
        assert!(Rule::from_action(&edit(&real.join("x/../a.rs")), &ctx).is_err());
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
    fn always_allow_is_remembered_for_the_workspace_and_survives_a_new_session() {
        in_loop(|ctx| {
            let tmp = tempfile::tempdir().expect("tmp");
            let rules = tmp.path().join("state").join("always-allow.json");
            let (handle, seen) = bound(tmp.path(), Mode::Ask, DEFAULT_DEADLINE);
            // Without a rules file there is no "Always": nothing could remember it.
            // (A build runner like `cargo` is never remembered: it runs model-editable files.)
            let rx = client(
                handle.socket_path().to_owned(),
                with_id(cmd("ls -la"), "q0"),
            );
            assert!(pump_until(ctx, 10, || request_id(&seen).is_some()));
            let options = |n: usize| match &seen.borrow()[n].event {
                Event::ApprovalRequested { options, .. } => options.clone(),
                other => panic!("{other:?}"),
            };
            assert!(!options(0).contains(&Decision::AllowAlways));
            assert!(handle.respond("q0", Decision::Deny));
            wait_reply(ctx, &rx);

            handle.use_always_rules(rules.clone());
            let rx = client(
                handle.socket_path().to_owned(),
                with_id(cmd("ls -la"), "q1"),
            );
            assert!(pump_until(ctx, 10, || seen.borrow().len() == 2));
            assert!(options(1).contains(&Decision::AllowAlways));
            // The card says exactly what would be kept.
            assert!(
                matches!(&seen.borrow()[1].event, Event::ApprovalRequested { remembers: Some(r), .. }
                if r.starts_with("`ls -la` in "))
            );
            assert!(handle.respond("q1", Decision::AllowAlways));
            assert_eq!(wait_reply(ctx, &rx).decision, Decision::AllowAlways);
            // Saved off the main thread.
            assert!(pump_until(ctx, 10, || {
                crate::always_allow::AlwaysRules::open(&rules).is_ok_and(|r| r.rules.len() == 1)
            }));
            // A command that cannot be remembered exactly is never offered "Always".
            let rx = client(
                handle.socket_path().to_owned(),
                with_id(cmd("ls -la && rm -rf x"), "q2"),
            );
            assert!(pump_until(ctx, 10, || seen.borrow().len() == 3));
            assert!(!options(2).contains(&Decision::AllowAlways));
            assert!(handle.respond("q2", Decision::Deny));
            wait_reply(ctx, &rx);
            // U2: also for an edit of one contained path and for one MCP tool, as for every
            // agent (an edit outside the workspace or of a sensitive file gets neither).
            let target = tmp.path().join("a.rs").to_string_lossy().into_owned();
            let rx = client(
                handle.socket_path().to_owned(),
                with_id(edit("write_to_file", &target), "q2e"),
            );
            assert!(pump_until(ctx, 10, || seen.borrow().len() == 4));
            assert!(options(3).contains(&Decision::AllowAlways));
            assert!(options(3).contains(&Decision::AllowForSession));
            assert!(handle.respond("q2e", Decision::Deny));
            wait_reply(ctx, &rx);
            let mcp = q(
                "call_mcp_tool",
                serde_json::json!({"ServerName": "fs", "ToolName": "write"}),
            );
            let rx = client(handle.socket_path().to_owned(), with_id(mcp, "q2m"));
            assert!(pump_until(ctx, 10, || seen.borrow().len() == 5));
            assert!(options(4).contains(&Decision::AllowAlways));
            assert!(handle.respond("q2m", Decision::Deny));
            wait_reply(ctx, &rx);
            drop(handle);

            // A new session in the same workspace allows the exact call without asking.
            let (again, seen) = bound(tmp.path(), Mode::Ask, DEFAULT_DEADLINE);
            again.use_always_rules(rules.clone());
            let rx = client(again.socket_path().to_owned(), with_id(cmd("ls -la"), "q3"));
            assert_eq!(wait_reply(ctx, &rx).decision, Decision::Allow);
            assert!(seen.borrow().is_empty(), "no card");
            // Plan mode still refuses what mutates, whatever is remembered.
            again.set_mode(Mode::Plan);
            let rx = client(
                again.socket_path().to_owned(),
                with_id(edit("write_to_file", "/elsewhere/a.rs"), "q4"),
            );
            assert_eq!(wait_reply(ctx, &rx).decision, Decision::Deny);

            // Another workspace does not inherit it.
            let other = tempfile::tempdir().expect("other");
            let (elsewhere, seen) = bound(other.path(), Mode::Ask, DEFAULT_DEADLINE);
            elsewhere.use_always_rules(rules);
            let rx = client(
                elsewhere.socket_path().to_owned(),
                with_id(cmd("ls -la"), "q5"),
            );
            assert!(pump_until(ctx, 10, || seen.borrow().len() == 1), "it asks");
            assert!(elsewhere.respond("q5", Decision::Deny));
            wait_reply(ctx, &rx);
        });
    }

    #[test]
    fn pattern_rules_auto_approve_without_asking_user() {
        in_loop(|ctx| {
            let tmp = tempfile::tempdir().expect("tmp");
            let rules_path = tmp.path().join("always-allow.json");
            let mut rules = crate::always_allow::AlwaysRules::default();
            assert!(rules.add(
                crate::always_allow::Rule::new("*", "run_command", "cargo test*").expect("rule")
            ));
            rules.save(&rules_path).expect("save");

            let (handle, seen) = bound(tmp.path(), Mode::Ask, DEFAULT_DEADLINE);
            handle.use_always_rules(rules_path);

            // A command matching the pattern is auto-approved without emitting a card:
            let rx = client(
                handle.socket_path().to_owned(),
                with_id(cmd("cargo test --workspace"), "p1"),
            );
            assert_eq!(wait_reply(ctx, &rx).decision, Decision::Allow);
            assert!(
                seen.borrow().is_empty(),
                "no approval requested card for auto-approved pattern"
            );

            // A command not matching the pattern asks for approval:
            let rx2 = client(
                handle.socket_path().to_owned(),
                with_id(cmd("cargo build"), "p2"),
            );
            assert!(pump_until(ctx, 10, || seen.borrow().len() == 1));
            assert!(handle.respond("p2", Decision::Deny));
            assert_eq!(wait_reply(ctx, &rx2).decision, Decision::Deny);
        });
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
            assert!(
                name.starts_with(&format!("approval-{}-", std::process::id()))
                    && name.ends_with("-t1.sock"),
                "{name}"
            );
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

            // The exact same command is now allowed without a new card.
            let rx = client(path.clone(), with_id(cmd("git status --short"), "q2"));
            assert_eq!(wait_reply(ctx, &rx).decision, Decision::Allow);
            assert_eq!(seen.borrow().len(), 1, "no second card");
            // A longer variant of it is not: exact match only.
            let rx = client(path.clone(), with_id(cmd("git status --short -b"), "q2b"));
            assert!(pump_until(ctx, 10, || seen.borrow().len() == 2));
            assert!(handle.respond("q2b", Decision::Deny));
            assert_eq!(wait_reply(ctx, &rx).decision, Decision::Deny);
            // A different command still asks.
            let rx = client(path.clone(), with_id(cmd("git push"), "q3"));
            assert!(pump_until(ctx, 10, || seen.borrow().len() == 3));
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
            let inside = tmp.path().join("a.txt").to_string_lossy().into_owned();
            let rx = client(
                path.clone(),
                with_id(
                    q("view_file", serde_json::json!({"AbsolutePath": inside})),
                    "p2",
                ),
            );
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
    fn bind_refuses_a_deleted_binary_and_a_useless_workspace() {
        in_loop(|_| {
            let tmp = tempfile::tempdir().expect("tmp");
            let dir = tmp.path().join("rt");
            let bin = |s: &str| PathBuf::from(s);
            let try_bind = |ws: &Path, hook: PathBuf| {
                ApprovalHandle::bind_with(&dir, "t", ws, Mode::Ask, DEFAULT_DEADLINE, hook)
            };
            assert!(try_bind(tmp.path(), bin("/usr/bin/agent-terminal")).is_ok());
            let deleted = try_bind(tmp.path(), bin("/usr/bin/agent-terminal (deleted)"));
            assert!(deleted.is_err_and(|e| e.contains("replaced")));
            for ws in ["", "/", "relative"] {
                assert!(
                    try_bind(Path::new(ws), bin("/usr/bin/agent-terminal")).is_err(),
                    "{ws:?}"
                );
            }
        });
    }

    #[test]
    fn socket_names_are_unique_and_sanitized() {
        in_loop(|_| {
            let tmp = tempfile::tempdir().expect("tmp");
            let dir = tmp.path().join("rt");
            let bind = |thread: &str| {
                ApprovalHandle::bind(&dir, thread, tmp.path(), Mode::Ask, DEFAULT_DEADLINE)
                    .expect("bind")
            };
            let a = bind("same");
            let b = bind("same");
            assert_ne!(a.socket_path(), b.socket_path());
            let evil = bind("../../x/y z\n");
            let name = evil
                .socket_path()
                .file_name()
                .expect("n")
                .to_string_lossy()
                .into_owned();
            assert_eq!(evil.socket_path().parent(), a.socket_path().parent());
            assert!(name.ends_with("-xyz.sock"), "{name}");
            assert!(bind("")
                .socket_path()
                .to_string_lossy()
                .ends_with("-t.sock"));
        });
    }

    #[test]
    fn dropping_the_server_denies_what_is_pending() {
        in_loop(|ctx| {
            let tmp = tempfile::tempdir().expect("tmp");
            let (handle, seen) = bound(tmp.path(), Mode::Ask, DEFAULT_DEADLINE);
            let rx = client(handle.socket_path().to_owned(), with_id(cmd("ls"), "z1"));
            assert!(pump_until(ctx, 10, || request_id(&seen).is_some()));
            drop(handle);
            let r = wait_reply(ctx, &rx);
            assert_eq!(r.decision, Decision::Deny);
        });
    }

    #[test]
    fn queries_are_recorded_for_the_canary_and_consumed_once() {
        in_loop(|ctx| {
            let tmp = tempfile::tempdir().expect("tmp");
            let (handle, _) = bound(tmp.path(), Mode::Plan, DEFAULT_DEADLINE);
            assert!(!handle.consume_query(&["run_command"]));
            let rx = client(handle.socket_path().to_owned(), with_id(cmd("ls"), "k1"));
            let _ = wait_reply(ctx, &rx); // denied by plan mode, but still recorded
            assert!(!handle.consume_query(&["write_to_file"]), "other tool");
            assert!(handle.consume_query(&["write_to_file", "run_command"]));
            assert!(!handle.consume_query(&["run_command"]), "once only");
        });
    }

    #[test]
    fn an_unproven_hook_binds_nothing() {
        // Fail-closed: a failed or missing verdict never reaches the socket (which would also
        // read the live `XDG_RUNTIME_DIR`, so only this half is exercised here).
        for why in [
            "not installed",
            "the approval hook has not been checked yet",
        ] {
            let refused = ApprovalHandle::bind_checked(
                Err(why.to_owned()),
                "t1",
                Path::new("/tmp"),
                Mode::Ask,
            );
            assert_eq!(refused.err().as_deref(), Some(why));
        }
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

    #[test]
    fn respond_with_rule_saves_custom_wildcard_rule_and_auto_approves_later() {
        in_loop(|ctx| {
            let tmp = tempfile::tempdir().expect("tmp");
            let rules = tmp.path().join("always.json");
            let (handle, seen) = bound(tmp.path(), Mode::Ask, DEFAULT_DEADLINE);
            handle.use_always_rules(rules.clone());

            let rx = client(
                handle.socket_path().to_owned(),
                with_id(cmd("ls -la"), "q_custom"),
            );
            assert!(pump_until(ctx, 10, || seen.borrow().len() == 1));
            // The user loosens the pattern to "ls *" for every workspace.
            let custom_rule =
                crate::always_allow::Rule::new("*", "run_command", "ls *").expect("rule");
            assert!(handle.respond_with_rule("q_custom", Decision::AllowAlways, Some(custom_rule)));
            assert_eq!(wait_reply(ctx, &rx).decision, Decision::AllowAlways);
            assert!(pump_until(ctx, 10, || {
                crate::always_allow::AlwaysRules::open(&rules).is_ok_and(|r| r.rules.len() == 1)
            }));

            // A new session allows a matching call without a card.
            drop(handle);
            let (again, seen2) = bound(tmp.path(), Mode::Ask, DEFAULT_DEADLINE);
            again.use_always_rules(rules.clone());
            let rx2 = client(
                again.socket_path().to_owned(),
                with_id(cmd("ls -l"), "q_auto"),
            );
            assert_eq!(wait_reply(ctx, &rx2).decision, Decision::Allow);
            assert_eq!(seen2.borrow().len(), 0);
        });
    }

    /// R1 and S2: a runner is never kept by "Always allow", even through the dialog's pattern or
    /// a direct answer the card never offered; for the session it is kept as the exact command
    /// only, never as a pattern.
    #[test]
    fn runners_are_never_kept_always_and_only_exactly_for_the_session() {
        in_loop(|ctx| {
            let tmp = tempfile::tempdir().expect("tmp");
            let rules = tmp.path().join("always.json");
            let (handle, seen) = bound(tmp.path(), Mode::Ask, DEFAULT_DEADLINE);
            handle.use_always_rules(rules.clone());
            let ask = |id: &str, line: &str| {
                let rx = client(handle.socket_path().to_owned(), with_id(cmd(line), id));
                let n = seen.borrow().len();
                assert!(pump_until(ctx, 10, || seen.borrow().len() > n));
                rx
            };
            let card_options = |id: &str| {
                seen.borrow()
                    .iter()
                    .find_map(|e| match &e.event {
                        Event::ApprovalRequested { options, .. }
                            if e.request.as_deref() == Some(id) =>
                        {
                            Some(options.clone())
                        }
                        _ => None,
                    })
                    .expect("card")
            };

            let rx = ask("a1", "cargo test --bin foo");
            assert!(!card_options("a1").contains(&Decision::AllowAlways));
            let typed =
                crate::always_allow::Rule::new("*", "run_command", "cargo test*").expect("rule");
            assert!(handle.respond_with_rule("a1", Decision::AllowAlways, Some(typed)));
            assert_eq!(wait_reply(ctx, &rx).decision, Decision::Allow, "downgraded");
            assert!(!rules.exists(), "nothing saved");

            let rx = ask("a2", "cargo test --bin foo");
            let loose =
                crate::always_allow::Rule::new("*", "run_command", "cargo *").expect("rule");
            assert!(handle.respond_with_rule("a2", Decision::AllowForSession, Some(loose)));
            assert_eq!(
                wait_reply(ctx, &rx).decision,
                Decision::Allow,
                "no pattern kept"
            );

            let rx = ask("a3", "cargo test --bin foo");
            assert!(handle.respond("a3", Decision::AllowForSession));
            assert_eq!(wait_reply(ctx, &rx).decision, Decision::AllowForSession);
            let rx = client(
                handle.socket_path().to_owned(),
                with_id(cmd("cargo test --bin foo"), "a4"),
            );
            assert_eq!(
                wait_reply(ctx, &rx).decision,
                Decision::Allow,
                "exact repeat"
            );
            let rx = ask("a5", "cargo clippy");
            assert!(handle.respond("a5", Decision::Deny));
            assert_eq!(wait_reply(ctx, &rx).decision, Decision::Deny);
        });
    }
}
