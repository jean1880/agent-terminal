//! The P4 cross-lane seam (`docs/delegation/p4-supervisor.md` §1.1). Frozen in Wave 0: lanes change
//! bodies elsewhere, never these shapes. A change here needs the coordinator.

use std::future::Future;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::rc::Rc;

use agent_core::adapter::{Driver, Mode};
use agent_core::delegation::authority::{InheritedAuthority, TaskMode};
use agent_core::delegation::ipc::{Call, CallOutcome, Hello, Welcome};
use agent_core::delegation::mailbox::{DeliveryPlan, MessageKind};
use agent_core::delegation::scope::ScopedEnvelope;
use agent_core::delegation::wire::WorkerSummary;
use agent_core::delegation::{
    AuthorityEpoch, CapabilityToken, DelegationBinding, ErrorCode, InstanceNonce, MessageId,
    OperationId, ProcessGeneration, ToolError, WorkerId,
};
use agent_core::event::{Envelope, TurnState};
use serde_json::Value;

use crate::agent_proc::{AgentEnv, ExitWatch};

/// A future on the GTK main context (never `Send`).
pub(crate) type LocalFuture<T> = Pin<Box<dyn Future<Output = T>>>;

// ---- identities -------------------------------------------------------------------------------

/// One IPC connection, unique for the app's life.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub(crate) struct ConnId(pub u64);

/// Whether a principal is a user's root session or a supervised child.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) enum Role {
    Root,
    Child,
}

// ---- runtime directory (D builds, A and C use) -------------------------------------------------

/// `$XDG_RUNTIME_DIR/agent-terminal`, proven a real 0700 directory owned by this uid.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct RuntimeDir(PathBuf);

impl RuntimeDir {
    /// Only `rundir::verified()` and tests call this: it asserts the checks were made.
    pub(crate) fn from_verified(path: PathBuf) -> Self {
        Self(path)
    }

    pub(crate) fn path(&self) -> &Path {
        &self.0
    }
}

// ---- launch bindings (D prepares off-thread, A stages, B consumes) ------------------------------

/// What one process start of a session receives. Debug never prints values (agy's env holds the
/// token).
#[derive(Clone, Default)]
pub(crate) struct LaunchBinding {
    /// `OpenSession.delegation` (the Codex adapter's thread config overlay: P5).
    pub open: Option<DelegationBinding>,
    /// agy only: `DelegationBinding::env(app_pid)`.
    pub env: Vec<(String, String)>,
    /// Claude only: `--mcp-config <overlay>`, plus the child arguments for a child.
    pub argv_tail: Vec<String>,
}

impl std::fmt::Debug for LaunchBinding {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LaunchBinding")
            .field("open", &self.open.is_some())
            .field(
                "env",
                &self.env.iter().map(|(k, _)| k.as_str()).collect::<Vec<_>>(),
            )
            .field("argv_tail_len", &self.argv_tail.len())
            .finish()
    }
}

/// Input of `launch::prepare_binding` (blocking; run in `gio::spawn_blocking`).
#[derive(Debug, Clone)]
pub(crate) struct BindingRequest {
    pub driver: Driver,
    pub role: Role,
    pub worker: WorkerId,
    /// The generation this binding is for: the session's next one.
    pub generation: ProcessGeneration,
    pub task_mode: Option<TaskMode>,
    pub app_pid: u32,
    pub bridge_command: PathBuf,
    pub endpoint: PathBuf,
    pub instance: InstanceNonce,
    pub rundir: RuntimeDir,
}

/// A fully prepared, immutable binding: token minted, Claude overlay already written.
#[derive(Debug, Clone)]
pub(crate) struct PreparedBinding {
    pub worker: WorkerId,
    pub generation: ProcessGeneration,
    pub token: CapabilityToken,
    pub launch: LaunchBinding,
    /// The Claude overlay file, unlinked only after this generation's observed exit.
    pub overlay: Option<PathBuf>,
}

/// A child profile resolved for launch (from the window's cached detection). `env.unset` already
/// holds the profile's `clear_env`; the child factory adds `STRIP_ON_SPAWN` to it.
#[derive(Clone)]
pub(crate) struct ResolvedChild {
    pub program: String,
    pub extra_args: Vec<String>,
    pub env: AgentEnv,
    pub default_model: Option<String>,
    pub default_effort: Option<String>,
}

// ---- IPC (C serves, A dispatches) --------------------------------------------------------------

/// The peer of a Unix connection (`SO_PEERCRED` through `gio::Credentials`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub(crate) struct PeerFacts {
    pub pid: Option<u32>,
}

/// What the IPC server asks of the supervisor. Every method runs on the main context.
pub(crate) trait CallDispatcher {
    fn authenticate(
        &self,
        conn: ConnId,
        hello: Hello,
        peer: PeerFacts,
    ) -> LocalFuture<Result<Welcome, ToolError>>;
    fn call(&self, conn: ConnId, call: Call) -> LocalFuture<CallOutcome>;
    fn cancel(&self, conn: ConnId, id: u64);
    /// The connection is gone: release its waits. Accepted mutations carry on.
    fn closed(&self, conn: ConnId);
}

/// `/proc` facts for L3 and the census. `ipc::peer::RealProc` reads `/proc`; tests inject a fake.
pub(crate) trait ProcFacts {
    fn stat(&self, pid: u32) -> Option<String>;
    fn exe_id(&self, pid: u32) -> Option<(u64, u64)>;
    fn cmdline(&self, pid: u32) -> Option<Vec<u8>>;
    fn children_of(&self, ppid: u32) -> Vec<u32>;
}

// ---- authority (D implements, B calls, A constructs) --------------------------------------------

/// What a provider tool call does, provider by provider (§5.1).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Effect {
    Read,
    Edit,
    Exec,
    Network,
    Delegate,
    /// `own`: the call is to server `agent_terminal`.
    Mcp {
        own: bool,
    },
    Unknown,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum GateVerdict {
    /// Nothing inherited forbids it: the existing policy (and the rule filter) decides.
    Defer,
    /// A human card, with no session or Always option, never auto-allowed.
    AskOnly {
        reason: String,
    },
    Deny {
        reason: String,
    },
}

/// The supervisor's live view of a child's authority, read on every judgement.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct LiveAuthority {
    /// Epoch current, tree not revoking, read-back passed.
    pub valid: bool,
    /// Claude's inert bootstrap turn is running: every request is denied.
    pub bootstrap: bool,
    /// The parent principal's live effective mode (`session_safe` applied).
    pub parent_mode: Mode,
}

/// Inherited authority, applied before every auto-allow path (Bypass, Accept edits, session and
/// Always rules).
pub(crate) trait ApprovalGate {
    fn judge(&self, tool: &str, input: &Value, cwd: Option<&str>) -> GateVerdict;
    /// Whether a session or Always rule may match `action` in this child, and whether a card may
    /// offer to keep one.
    fn rule_may_apply(&self, action: &crate::always_allow::Action, cwd: Option<&str>) -> bool;
}

/// Constructor input of `authority::ChildAuthority::new`.
#[derive(Clone)]
pub(crate) struct AuthorityParams {
    pub inherited: InheritedAuthority,
    pub driver: Driver,
    pub task_mode: TaskMode,
    pub home: Option<PathBuf>,
    pub live: Rc<dyn Fn() -> LiveAuthority>,
}

// ---- session seam (A implements, B calls) ------------------------------------------------------

pub(crate) enum TurnBoundary<'a> {
    Started { native_turn: Option<&'a str> },
    Ended { state: TurnState },
}

/// Installed in a root or child `ChatSession`. Called on the main thread with no `RefCell` borrow
/// held.
#[derive(Clone)]
pub(crate) struct DelegationLaunchFacts {
    pub generation: ProcessGeneration,
    pub argv: Vec<String>,
    pub physical_cwd: PathBuf,
    pub effective_home: Option<PathBuf>,
    /// Presence only: configuration redirect values may themselves contain secrets.
    pub config_redirect_vars: Vec<String>,
    pub hook_proven: bool,
}

pub(crate) trait DelegationHooks {
    fn before_spawn(&self, _facts: DelegationLaunchFacts) -> LocalFuture<Result<(), String>> {
        Box::pin(async { Ok(()) })
    }
    /// Immediately before a root prompt can reach its adapter. False holds it unsent while
    /// the supervisor rebinds; children use their separate typed dispatch boundary.
    fn before_root_prompt(&self) -> bool {
        true
    }
    /// Takes the binding staged for exactly this generation. Never blocks, never does I/O.
    /// `None`: no token.
    fn binding(&self, driver: Driver, generation: ProcessGeneration) -> Option<LaunchBinding>;
    /// After a successful spawn: the supervisor keeps `watch` until it reports
    /// `StopPhase::Exited`.
    fn process_started(&self, generation: ProcessGeneration, pid: Option<i32>, watch: ExitWatch);
    /// The session closed this generation out (`finish_process`, which also runs for a deliberate
    /// stop). This is NOT an observed exit: kill the token and end runs, but free no process slot
    /// and unlink nothing.
    fn process_stopping(&self, generation: ProcessGeneration);
    /// Every raw provider line, before the adapter. Memory only: never logged, never stored.
    fn line(&self, generation: ProcessGeneration, line: &str, stderr: bool);
    /// Every adapter envelope before masking and persistence.
    fn observed(&self, generation: ProcessGeneration, env: &Envelope);
    fn turn(&self, generation: ProcessGeneration, boundary: TurnBoundary<'_>);
    fn mode_changed(&self, effective: Mode);
}

/// A child's publication boundary; replaces `store.append_event` in `Inner::emit` for that
/// session.
pub(crate) trait ChildPublish {
    fn publish(&self, generation: ProcessGeneration, env: Envelope) -> Option<Envelope>;
    fn persist(&self, thread: &str, provider: Option<&str>, env: &Envelope);
    fn cancel_generation(&self, generation: ProcessGeneration);
}

pub(crate) struct ChildPolicy {
    pub gate: Rc<dyn ApprovalGate>,
    pub publish: Rc<dyn ChildPublish>,
    /// The fixed task mode; `set_mode` never widens past it and never enters Bypass.
    pub mode: Mode,
}

#[derive(Default)]
pub(crate) struct SessionOptions {
    pub hooks: Option<Rc<dyn DelegationHooks>>,
    pub child: Option<ChildPolicy>,
    /// The last persisted generation. A reopened session must not reuse generation one.
    pub generation_base: ProcessGeneration,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum InputSource {
    /// Claude's inert read-back turn (§3.3).
    Bootstrap,
    /// The spawn's task.
    Task,
    Message {
        id: MessageId,
        kind: MessageKind,
    },
}

/// One execution-driving delivery, already reserved durably and passed E7 by the caller.
#[derive(Debug, Clone)]
pub(crate) struct DelegatedInput {
    pub operation: OperationId,
    pub epoch: AuthorityEpoch,
    pub source: InputSource,
    pub from: WorkerId,
    pub from_label: String,
    /// The scrubbed canonical text (contract §2). Labelled by the session as delegated data.
    pub text: String,
    /// `StartTurn`, `WriteLive` or `SteerNow` only. `QueueNextTurn` is the supervisor's (§4.5).
    pub plan: DeliveryPlan,
    /// Correlates the adapter's `ControlResult` for a Codex steer.
    pub tag: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum DelegatedOutcome {
    /// Claude live user line queued to stdin.
    Written,
    /// Codex strict steer sent; its outcome arrives as `ControlResult { request = tag }`.
    SteerSent {
        tag: String,
    },
    TurnStarted,
    Refused(ErrorCode),
}

// ---- UI and closing (A provides, E and P6 consume) --------------------------------------------

/// P6 implements; the supervisor holds it weakly.
pub(crate) trait WorkerSubscriber {
    fn scoped(&self, env: &ScopedEnvelope);
    /// A live event whose store transaction committed at this sequence.
    fn committed(&self, env: &ScopedEnvelope, _seq: i64) {
        self.scoped(env);
    }
    /// Persisted history must not trigger live effects such as taking a new diff baseline.
    fn replayed(&self, env: &ScopedEnvelope) {
        self.scoped(env);
    }
    fn registry(&self, root_thread: &str, workers: &[WorkerSummary]);
}

/// One supervised process the closing window must see exit.
pub(crate) struct ChildStop {
    pub agent: String,
    pub title: String,
    pub watch: ExitWatch,
}
