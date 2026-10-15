# P4: supervisor and authority (implementation design, revision 3)

Status: revision 3, 2026-10-15.

Implementation clarification, 2026-10-15: a deliberate same-authority credential rotation
revokes the old token and approvals without a duplicate synthetic `GenerationExited` epoch
bump. Observed exit still releases process slots and old overlays. Real Stop, Close,
restriction, configuration changes and unexpected exits retain their authority fences and
override a pending rotation. See the implementation amendments in `contract-v1.md`.

Revision 3 has three changes:
- §1.1 and §1.2 are now the compiled Wave 0 code, verbatim. Wave 0 is built, and the code is the source of truth.
- Codex Ask children use a child-specific `workspaceWrite` mapping (§10.1).
- A latched bootstrap violation is a failure (§3.3).

Revision 1 got CHANGES-REQUIRED from Codex: 6 blockers and 3 shoulds. Revision 2 fixed all nine and applied the user
decisions of 2026-10-15:
- closing a root's page stops its children;
- Always and session rules apply in children only inside the child's lease.

The contract deviations this needs are recorded as **"P4 amendments"** at the end of `contract-v1.md` (A1–A4).

Inputs: `contract-v1.md` (revision 6 plus the P4 amendments; it wins on conflict), `p1-types.md`, plan §3, §5–§7, §9
and the P0 amendments. Code: `feat/cross-agent-runtime` at `8397088e`, where P1 is merged.

| Rev 1 finding | Fixed in |
| --- | --- |
| B1 Wave 0 seam incomplete | §1.1 and §1.2 are the compiled Wave 0 files, verbatim |
| B2 deferred deliveries bypass reserve and E7 | §4.5: the supervisor owns every deferred queue; the session queue never holds delegated input |
| B3 synthetic exit treated as observed | §7.1: one `ExitWatch` per generation; only `StopPhase::Exited` frees a slot or unlinks an overlay |
| B4 Codex read-back incomplete | §3.3: sandbox, approvalPolicy and servers are checked, servers continuously; P4 owns it, and P5 owns only what is requested |
| B5 classification only knew agy names | §5.1 `effect()`, provider by provider; unknown tools are denied in Plan |
| B6 Claude read-back after the task | §3.3: an inert bootstrap turn comes first (amendment A1) |
| S7 file system work on the main thread | §5.5: bindings are prepared off-thread; `binding()` only takes a staged value |
| S8 Codex rebind by restart | amendment A2, §5.4 |
| S9 window transfer | §2.3, amendment A3 |

## 0. Preconditions and scope

| Needed before builders start | Why |
| --- | --- |
| P1 merged (done, `8397088e`) | everything uses `agent_core::delegation::*`. The real signatures take `app_pid`: `DelegationBinding::env(app_pid)`, `claude_overlay(app_pid)`, `codex_thread_config(app_pid)` |
| P3b merged with the queries of §0.1 | P3a has only row helpers |
| Wave 0 (§1.1, §1.2) committed by the coordinator | lanes B, C, D and A compile apart |

Not P4:
- the `--cross-agent-mcp` bridge and rmcp (P5);
- what the Codex adapter *requests* (child `enabled=false` list, private `CODEX_HOME`, overlay on resume) and Claude
  `--replay-user-messages` (P5);
- the worker UI (P6), root auto-continuation (P7), Settings (P9).

**P4 ships dark.** The default `RouteTable` is P1 `admit_route`, which reports every route `Unverified`. Root bindings
need the profile's `available_for_delegation` setting, which defaults to false.

### 0.1 Store API P4 consumes from P3b (`&Store` functions, usable inside `durable_call`)

```rust
fn delegation_page(&self, q: &PageQuery) -> Result<Vec<PageUnitRow>>;
fn delegation_list(&self, root: &str, subtree_of: &str, after: Option<&str>, limit: u32) -> Result<Vec<WorkerRow>>;
fn delegation_bump_epoch(&self, root: &str, restriction: bool) -> Result<i64>;
fn delegation_invalidate_unreserved(&self, root: &str, workers: &[String], error_json: &str) -> Result<InvalidateCounts>;
    // accepted ops → failed(stale_epoch); queued / queued_next_turn messages → cancelled
fn delegation_reconcile_startup(&self, now: i64) -> Result<ReconcileCounts>;   // §7.3
fn delegation_active_runs(&self, root: &str) -> Result<Vec<RunRow>>;
fn delegation_mark_presented(&self, ids: &[String], at: i64) -> Result<()>;
fn delegation_deferred(&self, worker: &str) -> Result<Vec<MessageRow>>;   // queued + queued_next_turn, FIFO by queued_at
```

If P3b slips, Lane A may add these in a new `crates/agent-kit/src/store/delegation_query.rs`, with one `mod` line in
`store.rs`, once the coordinator confirms that no P3b branch is open on those files.

An event and its links commit in one writer job, with no new writer API:
- `Store::append_event` (`store.rs:1223`, returns the seq), then `insert_delegation_event_link` (`delegation.rs:915`);
- the ordinary path is `StoreWriter::write_sized(.., force, job)` (`store_writer.rs:276`), with `force` set as in
  `store_writer.rs:513`;
- control transactions use `durable_call` (`store_writer.rs:451`).

## 1. Module layout

| File | Lane | Owns |
| --- | --- | --- |
| `src/delegation/mod.rs` | 0, then A | module list, singleton, `start`, `supervisor()`, `shutdown_all`, `final_cleanup` |
| `src/delegation/seam.rs` | 0 (frozen) | every cross-lane type and trait (§1.1) |
| `supervisor.rs`, `services.rs`, `ops.rs`, `child.rs`, `readback.rs`, `publish.rs`, `attest.rs`, `waits.rs`, `deferred.rs`, `capacity.rs`, `reconcile.rs` | A | §2–§4, §5.3–§5.4, §7 |
| `ipc.rs`, `ipc/conn.rs`, `ipc/peer.rs` | C | §4.1–§4.3 |
| `authority.rs`, `workspace.rs`, `agy_l2.rs`, `launch.rs`, `rundir.rs` | D | §5.1, §5.5, §6, L2 I/O |
| `src/chat/session/delegated.rs` | 0 (stubs), then B | the `ChatSession` methods of §3.4 |

### 1.1 `src/delegation/seam.rs` (Wave 0, compiled, frozen)

This is the committed file verbatim (Wave 0 commit on `feat/cross-agent-p4-wave0`). A change needs the coordinator.

```rust
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
pub(crate) trait DelegationHooks {
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
    fn registry(&self, root_thread: &str, workers: &[WorkerSummary]);
}

/// One supervised process the closing window must see exit.
pub(crate) struct ChildStop {
    pub agent: String,
    pub title: String,
    pub watch: ExitWatch,
}
```

`always_allow::Action`, `agent_proc::{AgentEnv, ExitWatch}` and `hook_config::HookTarget` are already `pub`, in
modules the crate root declares.

### 1.2 Wave 0 stubs (compiled; signatures frozen; bodies fail closed)

Each file below is committed verbatim. Lanes replace bodies, never signatures. The other Lane A modules (`attest.rs`, `capacity.rs`, `deferred.rs`, `ops.rs`, `publish.rs`, `reconcile.rs`, `waits.rs`) hold only a module comment. `src/main.rs` gains `mod delegation;` and `src/chat/session.rs` gains
`mod delegated;`. `agent_core::codex::mode_policy` is now `pub` (signature unchanged).

Notes on the compiled shapes:
- `ChatSession::with_options` takes a concrete `SessionStore` (not `impl Into<SessionStore>`), so `child.rs` can
  hold it as a `fn` pointer. It carries `#[allow(clippy::too_many_arguments)]`, like `terminal_stub.rs:184`.
- `child.rs` references every frozen `ChatSession` method, so the stubs outside `src/delegation` are not dead code.
  `#![allow(dead_code)]` is set in `src/delegation/mod.rs` only.
- `services` and `supervisor` are `pub(crate)` modules, so Lane E reaches `RootTurnGate`, `RouteTable` and the
  service type aliases through their paths.
- `readback.rs` already holds the real Codex extraction and comparison, with tests on the recorded result shape
  (`d6-sandbox-policy`): `codex_requested(&(approval, sandbox))` takes a `(&'static str, Value)` pair, as
  `mode_policy` and Lane B's child mapping return it. `codex_policy_read_back` compares `approvalPolicy` and
  `sandbox.type`; a missing field fails.
- Human input to a child is only `Supervisor::submit_human` (§4.5). There is no `ChatSession` method for it.

Gates at the Wave 0 commit: `cargo check --workspace --all-targets`, `cargo clippy --workspace --all-targets -- -D
warnings`, `cargo test --workspace --no-run` and `cargo fmt --all --check` pass.

`src/delegation/mod.rs`:

```rust
//! The cross-agent delegation supervisor (`docs/delegation/p4-supervisor.md`).
//!
//! Wave 0: the frozen seam ([`seam`]) and a fail-closed stub for every module. Lanes A–D replace
//! bodies, never the signatures listed in the design's §1.2. Nothing here is reachable from the
//! app until Lane E wires it, so it ships dark.
// Wave 0 stubs are not called yet; lanes A–E fill them in.
#![allow(dead_code)]

use std::cell::RefCell;
use std::rc::Rc;

pub(crate) mod seam;

mod agy_l2;
mod attest;
mod authority;
mod capacity;
mod child;
mod deferred;
mod ipc;
mod launch;
mod ops;
mod publish;
mod readback;
mod reconcile;
mod rundir;
pub(crate) mod services;
pub(crate) mod supervisor;
mod waits;
mod workspace;

pub(crate) use services::AppServices;
pub(crate) use supervisor::Supervisor;

thread_local! {
    /// The app's one supervisor, on the GTK main thread. No window holds it strongly.
    static SUPERVISOR: RefCell<Option<Rc<Supervisor>>> = const { RefCell::new(None) };
}

/// The running supervisor; `None` before [`start`] finished or after shutdown.
pub(crate) fn supervisor() -> Option<Rc<Supervisor>> {
    SUPERVISOR.with(|s| s.borrow().clone())
}

/// Starts the supervisor on the main context (reconcile, sweep, bind). Wave 0: does nothing.
pub(crate) fn start(_services: AppServices) {}

/// The AppShutdown revocation row: every supervised process still to be seen exiting.
pub(crate) fn shutdown_all() -> Vec<seam::ChildStop> {
    Vec::new()
}

/// Removes the socket and the overlays of exited generations. Idempotent.
pub(crate) fn final_cleanup() {}
```

`src/delegation/services.rs`:

```rust
//! Lane A: what the supervisor needs from the app (§2.1). Wave 0 stub.

use std::path::{Path, PathBuf};
use std::rc::Rc;

use agent_core::adapter::{Adapter, Driver};
use agent_core::delegation::{ErrorCode, ToolError};
use agent_kit::store::Store;
use agent_kit::store_writer::StoreHandle;

use super::seam::{LocalFuture, ResolvedChild};
use crate::config::Profile;
use crate::hook_config::HookTarget;

/// The narrow app services the supervisor holds (built by Lane E from `window::imp::threads`).
pub(crate) struct AppServices {
    pub handle: StoreHandle,
    /// `None`: memory or test store; jobs run in place.
    pub store_path: Option<PathBuf>,
    pub app_pid: u32,
    pub bridge_command: PathBuf,
    pub profiles: Rc<dyn Fn() -> Vec<Profile>>,
    pub resolve: Rc<ResolveChild>,
    pub make_adapter: Rc<MakeAdapter>,
    pub hook_check: Rc<HookCheck>,
    pub tab_open_in: Rc<dyn Fn(&Path) -> bool>,
    pub report: Rc<dyn Fn(&str)>,
}

/// `threads::cached_agent` plus `unset_list`, as a service.
pub(crate) type ResolveChild = dyn Fn(&Profile) -> Option<ResolvedChild>;
/// `threads::make_adapter`, as a service.
pub(crate) type MakeAdapter = dyn Fn(Driver, &str) -> Box<dyn Adapter>;
/// `threads::check_hook`, as a service.
pub(crate) type HookCheck = dyn Fn(HookTarget) -> LocalFuture<Result<(), String>>;

impl AppServices {
    /// A read on its own connection, after the writer's barrier (`threads::store_job`).
    pub(crate) async fn read<T: Send + 'static>(
        &self,
        _job: impl FnOnce(&Store) -> T + Send + 'static,
    ) -> Option<T> {
        None
    }

    /// A durable control write (`StoreWriter::durable_call`). Any failure is
    /// `persistence_failed`, and the caller dispatches nothing.
    pub(crate) async fn durable<T: Send + 'static>(
        &self,
        _what: &'static str,
        _job: impl FnOnce(&Store) -> agent_kit::store::Result<T> + Send + 'static,
    ) -> Result<T, ToolError> {
        Err(ToolError::new(
            ErrorCode::PersistenceFailed,
            "delegation is not built yet",
        ))
    }

    /// An ordinary write (`StoreWriter::write_sized`).
    pub(crate) fn write(
        &self,
        _what: &'static str,
        _thread: Option<&str>,
        _force: bool,
        _job: impl FnOnce(&Store) -> agent_kit::store::Result<()> + Send + 'static,
    ) {
    }
}
```

`src/delegation/supervisor.rs`:

```rust
//! Lane A: the supervisor (§1.3, §2, §5.4). Wave 0 stub: every entry point fails closed.

use std::rc::{Rc, Weak};

use agent_core::adapter::Driver;
use agent_core::delegation::authority::RevocationEvent;
use agent_core::delegation::route::{RouteQuery, RouteSpec};
use agent_core::delegation::{admit_route, ErrorCode, Limits, ToolError, WorkerId};

use super::seam::{ChildStop, DelegationHooks, WorkerSubscriber};
use super::services::AppServices;
use crate::chat::session::ChatSession;
use crate::config::Profile;

/// Which routes may be spawned. Production uses [`P1Routes`]; tests inject their own.
pub(crate) trait RouteTable {
    fn admit(&self, q: &RouteQuery) -> RouteSpec;
}

/// The P1 table: every route `Unverified` until P5/P8 flip it.
pub(crate) struct P1Routes;

impl RouteTable for P1Routes {
    fn admit(&self, q: &RouteQuery) -> RouteSpec {
        admit_route(q)
    }
}

/// What `SessionSlot::send_prompt` does with a root prompt.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum RootTurnGate {
    Proceed,
    /// A rebind runs first: queue the prompt; the supervisor dispatches it after `rebind_start`.
    Deferred,
}

/// The app-owned supervisor (§2).
pub(crate) struct Supervisor {
    services: AppServices,
    limits: Limits,
    routes: Rc<dyn RouteTable>,
}

fn not_built() -> ToolError {
    ToolError::new(ErrorCode::RuntimeUnavailable, "delegation is not built yet")
}

impl Supervisor {
    pub(crate) async fn start(
        _services: AppServices,
        _limits: Limits,
        _routes: Rc<dyn RouteTable>,
    ) -> Result<Rc<Self>, String> {
        Err("delegation is not built yet".to_owned())
    }

    /// Waits for the previous root generation's exit, then stages a binding (§2.3).
    pub(crate) async fn prepare_root(
        self: &Rc<Self>,
        _thread: &str,
        _profile: &Profile,
        _driver: Driver,
    ) -> Option<Rc<dyn DelegationHooks>> {
        None
    }

    pub(crate) fn attach_root_session(&self, _thread: &str, _session: Weak<ChatSession>) {}

    pub(crate) fn before_root_turn(self: &Rc<Self>, _thread: &str) -> RootTurnGate {
        RootTurnGate::Proceed
    }

    pub(crate) fn on_root_turn_end(&self, _thread: &str) {}

    pub(crate) fn root_stop(self: &Rc<Self>, _thread: &str) {}

    /// Root Stop for a closing root page: every unexited descendant generation (§2.3).
    pub(crate) fn root_closing(self: &Rc<Self>, _thread: &str) -> Vec<ChildStop> {
        Vec::new()
    }

    pub(crate) fn root_busy(&self, _thread: &str) -> bool {
        false
    }

    pub(crate) fn subscribe(&self, _root_thread: &str, _sub: Weak<dyn WorkerSubscriber>) {}

    /// A child's session, for scoped approvals only (P6).
    pub(crate) fn child_session(&self, _worker: &WorkerId) -> Option<Rc<ChatSession>> {
        None
    }

    /// Human inspector input to a child: queued, reserved and E7-checked like any delivery (§4.5).
    pub(crate) fn submit_human(self: &Rc<Self>, _worker: &WorkerId, _text: &str) {}

    pub(crate) async fn revoke(
        self: &Rc<Self>,
        _root: &WorkerId,
        _event: RevocationEvent,
    ) -> Result<(), ToolError> {
        Err(not_built())
    }
}
```

`src/delegation/child.rs`:

```rust
//! Lane A: the child session factory (§3.1). Wave 0: only the session API Lane B freezes, held
//! here so it is reachable before Lane A calls it.

use std::rc::Rc;

use agent_core::adapter::{Adapter, OpenSession};
use agent_kit::store::ThreadId;

use super::seam::{DelegatedInput, DelegatedOutcome, SessionOptions};
use crate::chat::session::{Approval, ChatSession, LaunchEnv, SessionStore};
use crate::chat::EnvelopeSink;

/// The constructor Lane A builds supervised children with (§3.4).
pub(crate) type WithOptions = fn(
    Box<dyn Adapter>,
    OpenSession,
    SessionStore,
    ThreadId,
    EnvelopeSink,
    Approval,
    LaunchEnv,
    SessionOptions,
) -> Rc<ChatSession>;

pub(crate) const WITH_OPTIONS: WithOptions = ChatSession::with_options;

/// Every other frozen `ChatSession` method of §1.2, in one place.
pub(crate) fn session_api(session: &ChatSession, input: DelegatedInput) -> DelegatedOutcome {
    let _ = session.interrupt_run();
    let _ = session.effective_mode();
    let _ = session.process_generation();
    let _ = session.exit_watches();
    let _ = session.rebind_stop();
    session.expire_approvals("");
    session.rebind_start();
    session.deliver_delegated(input)
}
```

`src/delegation/readback.rs`:

```rust
//! Lane A: child read-back (§3.2–§3.3). Wave 0 holds the Codex extraction and comparison, which
//! the launch mapping (Lane B) and the checker (Lane A) must share.

use serde_json::Value;

/// What a Codex child's `thread/start` or `thread/resume` result must read back as.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct CodexRequested {
    /// `approvalPolicy`, as sent.
    pub approval_policy: String,
    /// The sandbox policy's `type` (`readOnly`, `workspaceWrite`), as sent.
    pub sandbox_type: String,
}

/// Extracts the read-back expectation from a Codex `(approvalPolicy, sandboxPolicy)` pair, as
/// `agent_core::codex::mode_policy` (roots) or the child mapping (Lane B) returns it. `None` when
/// the sandbox policy has no string `type`: nothing could be checked, so the child must not start.
pub(crate) fn codex_requested(policy: &(&'static str, Value)) -> Option<CodexRequested> {
    let (approval, sandbox) = policy;
    let sandbox_type = sandbox.get("type")?.as_str()?;
    Some(CodexRequested {
        approval_policy: (*approval).to_owned(),
        sandbox_type: sandbox_type.to_owned(),
    })
}

/// Whether a `thread/start` or `thread/resume` `result` reads back exactly what was requested:
/// `approvalPolicy` equal, and `sandbox.type` equal. A missing field fails.
pub(crate) fn codex_policy_read_back(requested: &CodexRequested, result: &Value) -> bool {
    let policy = result.get("approvalPolicy").and_then(Value::as_str);
    let sandbox = result
        .get("sandbox")
        .and_then(|s| s.get("type"))
        .and_then(Value::as_str);
    policy == Some(requested.approval_policy.as_str())
        && sandbox == Some(requested.sandbox_type.as_str())
}

#[cfg(test)]
mod tests {
    use super::*;
    use agent_core::adapter::Mode;
    use serde_json::json;

    #[test]
    fn plan_mapping_reads_back_from_the_recorded_result_shape() {
        let requested = codex_requested(&agent_core::codex::mode_policy(Mode::Plan)).unwrap();
        assert_eq!(requested.approval_policy, "never");
        assert_eq!(requested.sandbox_type, "readOnly");
        // The shape recorded in tests/fixtures/real/delegation/codex/d6-sandbox-policy.
        let result = json!({"approvalPolicy": "never",
                            "sandbox": {"type": "readOnly", "networkAccess": false}});
        assert!(codex_policy_read_back(&requested, &result));
    }

    #[test]
    fn a_mismatch_or_a_missing_field_fails() {
        let requested = CodexRequested {
            approval_policy: "untrusted".into(),
            sandbox_type: "workspaceWrite".into(),
        };
        let wrong_sandbox = json!({"approvalPolicy": "untrusted", "sandbox": {"type": "readOnly"}});
        let wrong_policy =
            json!({"approvalPolicy": "never", "sandbox": {"type": "workspaceWrite"}});
        let flat_sandbox = json!({"approvalPolicy": "untrusted", "sandbox": "workspace-write"});
        let missing = json!({"sandbox": {"type": "workspaceWrite"}});
        for result in [wrong_sandbox, wrong_policy, flat_sandbox, missing] {
            assert!(!codex_policy_read_back(&requested, &result), "{result}");
        }
    }

    #[test]
    fn a_sandbox_without_a_type_gives_no_expectation() {
        assert_eq!(codex_requested(&("never", json!({}))), None);
        assert_eq!(codex_requested(&("never", json!({"type": 1}))), None);
    }
}
```

`src/delegation/authority.rs`:

```rust
//! Lane D: inherited authority for supervised children (§5.1–§5.3). Wave 0 stub: everything is
//! denied.

use agent_core::adapter::Driver;
use serde_json::Value;

use super::seam::{ApprovalGate, AuthorityParams, Effect, GateVerdict};

/// What a provider tool call does (§5.1 table). Anything not listed is `Unknown`.
pub(crate) fn effect(_driver: Driver, _tool: &str, _input: &Value) -> Effect {
    Effect::Unknown
}

/// The gate a supervised child's approvals pass first.
pub(crate) struct ChildAuthority {
    params: AuthorityParams,
}

impl ChildAuthority {
    pub(crate) fn new(params: AuthorityParams) -> Self {
        Self { params }
    }

    pub(crate) fn params(&self) -> &AuthorityParams {
        &self.params
    }
}

impl ApprovalGate for ChildAuthority {
    fn judge(&self, _tool: &str, _input: &Value, _cwd: Option<&str>) -> GateVerdict {
        GateVerdict::Deny {
            reason: "delegation is not built yet".to_owned(),
        }
    }

    fn rule_may_apply(&self, _action: &crate::always_allow::Action, _cwd: Option<&str>) -> bool {
        false
    }
}
```

`src/delegation/workspace.rs`:

```rust
//! Lane D: worktree leases for `isolated_worktree` children (§6). Wave 0 stub. Every function
//! here blocks (git): run it in `gio::spawn_blocking`.

use std::path::{Path, PathBuf};

use agent_core::delegation::authority::WorkspaceScope;
use agent_core::delegation::{ErrorCode, ToolError, WorkerId};

/// What `plan_lease` resolved at acceptance.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct LeasePlan {
    pub source_repo: PathBuf,
    pub base_commit: String,
    pub branch: String,
    pub path: PathBuf,
    /// The caller's tree had uncommitted changes (receipt warning `parent_changes_absent`).
    pub dirty_parent: bool,
}

/// A created worktree, canonical and checked.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct WorktreeLease {
    pub source_repo: PathBuf,
    pub worktree: PathBuf,
    pub branch: String,
    pub base_commit: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Cleanup {
    Removed,
    Retained(String),
}

fn not_built() -> ToolError {
    ToolError::new(ErrorCode::InternalError, "delegation is not built yet")
}

pub(crate) fn plan_lease(
    _caller_tree: &Path,
    _base: &str,
    _worker: &WorkerId,
    _root: Option<&Path>,
) -> Result<LeasePlan, ToolError> {
    Err(not_built())
}

pub(crate) fn create_lease(_plan: &LeasePlan) -> Result<WorktreeLease, ToolError> {
    Err(not_built())
}

pub(crate) fn verify_lease(_lease: &WorktreeLease) -> Result<(), ToolError> {
    Err(not_built())
}

pub(crate) fn cleanup_if_clean(
    _lease: &WorktreeLease,
    _tab_open_in: bool,
) -> Result<Cleanup, String> {
    Ok(Cleanup::Retained("delegation is not built yet".to_owned()))
}

/// The lease as the authority scope it grants.
pub(crate) fn scope(lease: &WorktreeLease) -> WorkspaceScope {
    WorkspaceScope::Lease {
        source_repo: lease.source_repo.to_string_lossy().into_owned(),
        worktree: lease.worktree.to_string_lossy().into_owned(),
        base_commit: lease.base_commit.clone(),
        allowed_relative: Vec::new(),
    }
}
```

`src/delegation/agy_l2.rs`:

```rust
//! Lane D: contract §7 L2, the file-system part (the pure parts are `agent_core::delegation::
//! agy_guard`). Wave 0 stub: always withholds. Blocking: run off the main thread.

use std::path::PathBuf;

use agent_core::delegation::agy_guard::L2Refusal;
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

/// `Ok` only when every L2 check passes; else why delegation is withheld.
pub(crate) fn scan(_input: &L2Input, _limits: &Limits) -> Result<(), L2Refusal> {
    Err(L2Refusal::DiscoveryFailed)
}
```

`src/delegation/launch.rs`:

```rust
//! Lane D: per-generation launch bindings and Claude overlay files (§5.5). Wave 0 stub.

use std::path::Path;

use super::seam::{BindingRequest, PreparedBinding, RuntimeDir};

/// Mints the token and, for Claude, writes the overlay (`O_EXCL|O_NOFOLLOW`, `0600`). Blocking:
/// run in `gio::spawn_blocking`.
pub(crate) fn prepare_binding(_req: &BindingRequest) -> Result<PreparedBinding, String> {
    Err("delegation is not built yet".to_owned())
}

/// Removes an overlay after its generation's observed exit.
pub(crate) fn unlink_overlay(_path: &Path) {}

/// Startup: removes every `mcp-<pid>-*.json` whose app pid is not a live agent-terminal.
pub(crate) fn sweep_overlays(_dir: &RuntimeDir, _live_app_pid: &dyn Fn(u32) -> bool) {}
```

`src/delegation/rundir.rs`:

```rust
//! Lane D: the verified runtime directory (§5.5). Wave 0 stub.

use super::seam::RuntimeDir;

/// `$XDG_RUNTIME_DIR/agent-terminal`, created `0700` if absent, then proven by `lstat`: a
/// directory, not a symlink, owned by this uid, mode `0700`. Blocking: run off the main thread.
pub(crate) fn verified() -> Result<RuntimeDir, String> {
    Err("delegation is not built yet".to_owned())
}
```

`src/delegation/ipc.rs`:

```rust
//! Lane C: the delegation IPC server (§4.1–§4.2): a GIO Unix socket on the main context. Wave 0
//! stub: binding always fails, so delegation stays unavailable.

mod conn;
pub(crate) mod peer;

use std::path::{Path, PathBuf};
use std::rc::Rc;

use agent_core::delegation::{Limits, ToolError};

use super::seam::{CallDispatcher, ConnId, RuntimeDir};

/// The listener on `<rundir>/delegation-<app_pid>.sock` and its connections.
pub(crate) struct IpcServer {
    path: PathBuf,
}

impl IpcServer {
    pub(crate) fn bind(
        _dir: &RuntimeDir,
        _app_pid: u32,
        _limits: Limits,
        _dispatcher: Rc<dyn CallDispatcher>,
    ) -> Result<Rc<Self>, String> {
        Err("delegation is not built yet".to_owned())
    }

    pub(crate) fn socket_path(&self) -> &Path {
        &self.path
    }

    /// Sends `revoked` with `error`, flushes (at most the write deadline), then closes.
    pub(crate) fn revoke(&self, _conn: ConnId, _error: ToolError) {}

    pub(crate) fn close_all(&self, _error: ToolError) {}

    /// (all connections, unauthenticated connections).
    pub(crate) fn counts(&self) -> (u32, u32) {
        (0, 0)
    }
}
```

`src/delegation/ipc/peer.rs`:

```rust
//! Lane C: `/proc` facts for the agy L3 peer check and the census (§4.3). Wave 0 stub: knows
//! nothing, so every agy hello fails L3.

use crate::delegation::seam::ProcFacts;

/// Reads `/proc`. A pid that vanished between scan and read is `None` (skipped).
pub(crate) struct RealProc;

impl ProcFacts for RealProc {
    fn stat(&self, _pid: u32) -> Option<String> {
        None
    }

    fn exe_id(&self, _pid: u32) -> Option<(u64, u64)> {
        None
    }

    fn cmdline(&self, _pid: u32) -> Option<Vec<u8>> {
        None
    }

    fn children_of(&self, _ppid: u32) -> Vec<u32> {
        Vec::new()
    }
}

/// The pids that are direct children of `agy_pid`, run the bridge executable `bridge_exe`
/// (`(dev, ino)`) and have a bridge argv.
pub(crate) fn census(_p: &dyn ProcFacts, _agy_pid: u32, _bridge_exe: (u64, u64)) -> Vec<u32> {
    Vec::new()
}
```

`src/delegation/ipc/conn.rs`:

```rust
//! Lane C: one connection's state machine (§4.2): pre-auth deadline, bounded frame reads, the
//! outbox and its write deadline. Wave 0: empty.
```

`src/chat/session/delegated.rs`:

```rust
//! Lane B: a session's typed delegated-input API (`docs/delegation/p4-supervisor.md` §3.4).
//!
//! Wave 0 stubs with frozen signatures. They fail closed: no delegated input is ever delivered,
//! and a session cannot be built with hooks or a child policy until Lane B lands.

use std::rc::Rc;

use agent_core::adapter::{Adapter, Mode, OpenSession};
use agent_core::delegation::{ErrorCode, ProcessGeneration};
use agent_kit::store::ThreadId;

use super::{Approval, ChatSession, LaunchEnv, SessionStore};
use crate::agent_proc::ExitWatch;
use crate::chat::{ChatBackend, EnvelopeSink};
use crate::delegation::seam::{DelegatedInput, DelegatedOutcome, SessionOptions};

impl ChatSession {
    /// [`ChatSession::with_env`] with delegation hooks and, for a supervised child, its policy.
    #[allow(clippy::too_many_arguments)] // `with_env`'s seven, plus the options
    pub(crate) fn with_options(
        adapter: Box<dyn Adapter>,
        open: OpenSession,
        store: SessionStore,
        thread: ThreadId,
        sink: EnvelopeSink,
        approval: Approval,
        env: LaunchEnv,
        opts: SessionOptions,
    ) -> Rc<Self> {
        // Wave 0: refuse to run a child or a bound root without the seam behind it. Routes are
        // all unverified, so nothing in the app reaches this with options set.
        assert!(
            opts.hooks.is_none() && opts.child.is_none(),
            "delegated sessions are not built yet"
        );
        Self::with_env(adapter, open, store, thread, sink, approval, env)
    }

    /// Delivers one reserved, E7-checked input (`StartTurn`, `WriteLive` or `SteerNow`).
    pub(crate) fn deliver_delegated(&self, _input: DelegatedInput) -> DelegatedOutcome {
        DelegatedOutcome::Refused(ErrorCode::InternalError)
    }

    /// Interrupts the running turn; false when there is none.
    pub(crate) fn interrupt_run(&self) -> bool {
        false
    }

    /// Ends every pending approval (revocation): Claude and Codex deny, agy expires.
    pub(crate) fn expire_approvals(&self, _reason: &str) {}

    /// Stops the process for a rebind and returns that generation's watch; nothing restarts it
    /// until [`Self::rebind_start`].
    pub(crate) fn rebind_stop(&self) -> Option<ExitWatch> {
        None
    }

    pub(crate) fn rebind_start(&self) {}

    pub(crate) fn effective_mode(&self) -> Mode {
        self.status().mode
    }

    /// The contract's process generation: bumped only when a process starts.
    pub(crate) fn process_generation(&self) -> ProcessGeneration {
        ProcessGeneration(0)
    }

    /// The watch of every generation not yet seen to exit.
    pub(crate) fn exit_watches(&self) -> Vec<ExitWatch> {
        self.exit_watch().into_iter().collect()
    }
}
```

### 1.3 Lane A public API (for Lane E, P5, P6 and P7)

```rust
impl Supervisor {
    pub(crate) async fn start(services: AppServices, limits: Limits, routes: Rc<dyn RouteTable>) -> Result<Rc<Self>, String>;
    pub(crate) async fn prepare_root(self: &Rc<Self>, thread: &str, profile: &Profile, driver: Driver)
        -> Option<Rc<dyn DelegationHooks>>;        // waits for the previous root generation's exit; stages a binding
    pub(crate) fn attach_root_session(&self, thread: &str, session: Weak<ChatSession>);
    pub(crate) fn before_root_turn(self: &Rc<Self>, thread: &str) -> RootTurnGate;   // Proceed | Deferred
    pub(crate) fn on_root_turn_end(&self, thread: &str);
    pub(crate) fn root_stop(self: &Rc<Self>, thread: &str);
    pub(crate) fn root_closing(self: &Rc<Self>, thread: &str) -> Vec<ChildStop>;      // §2.3
    pub(crate) fn root_busy(&self, thread: &str) -> bool;
    pub(crate) fn subscribe(&self, root_thread: &str, sub: Weak<dyn WorkerSubscriber>);
    pub(crate) fn child_session(&self, worker: &WorkerId) -> Option<Rc<ChatSession>>;  // approvals only (P6)
    pub(crate) fn submit_human(self: &Rc<Self>, worker: &WorkerId, text: &str);        // P6 inspector input (§4.5)
    pub(crate) async fn revoke(self: &Rc<Self>, root: &WorkerId, event: RevocationEvent) -> Result<(), ToolError>;
}
pub(crate) trait RouteTable { fn admit(&self, q: &RouteQuery) -> RouteSpec; }   // default: P1 admit_route
```

## 2. Ownership: app services, not a window

### 2.1 Singleton and services

- `thread_local! { static SUPERVISOR: RefCell<Option<Rc<Supervisor>>> }` in `delegation/mod.rs`, beside the store
  thread-locals (`threads.rs:429-448`). No window holds a strong reference. Lane E calls `start` once, from
  `setup_shell`, after `try_app_store()` (`threads.rs:465`).
- `AppServices` (`services.rs`):
  ```rust
  pub(crate) struct AppServices {
      pub handle: StoreHandle, pub store_path: Option<PathBuf>, pub app_pid: u32, pub bridge_command: PathBuf,
      pub profiles: Rc<dyn Fn() -> Vec<Profile>>,
      pub resolve: Rc<dyn Fn(&Profile) -> Option<ResolvedChild>>,     // threads::cached_agent + unset_list
      pub make_adapter: Rc<dyn Fn(Driver, &str) -> Box<dyn Adapter>>, // threads::make_adapter (:1400)
      pub hook_check: Rc<dyn Fn(HookTarget) -> LocalFuture<Result<(), String>>>, // threads::check_hook (:1444)
      pub tab_open_in: Rc<dyn Fn(&Path) -> bool>,
      pub report: Rc<dyn Fn(&str)>,
  }
  impl AppServices {
      pub(crate) async fn read<T: Send + 'static>(&self, job: impl FnOnce(&Store) -> T + Send + 'static) -> Option<T>;
      pub(crate) async fn durable<T: Send + 'static>(&self, what: &'static str,
          job: impl FnOnce(&Store) -> agent_kit::store::Result<T> + Send + 'static) -> Result<T, ToolError>;
      pub(crate) fn write(&self, what: &'static str, thread: Option<&str>, force: bool,
          job: impl FnOnce(&Store) -> agent_kit::store::Result<()> + Send + 'static);
  }
  ```
  - `read` copies `store_job` (`threads.rs:553-581`): it waits on the barrier, then uses a worker connection.
  - `durable` waits on the `durable_call` receiver in `gio::spawn_blocking`, as `write_job_or_late` does
    (`threads.rs:598-638`). Any failure is `persistence_failed`, and nothing is dispatched.
  - Without a writer (tests), both run in place through `Store::atomically`.

### 2.2 Weak subscribers

- `subscribe` keeps `Vec<(String, Weak<dyn WorkerSubscriber>)>`, pruned on every fan-out.
- Subscribing replays the `WorkerSummary` list at once.

### 2.3 Root page close and window moves (user decision; amendment A3)

- **Closing a root's page stops its children.** In `close_row_now`, and for every root in a closing window,
  Lane E calls `root_closing(thread)` before `session.shutdown`. That call:
  - runs `revoke(root, RootStop)`;
  - calls `shutdown(true)` on every descendant session;
  - returns every unexited `ExitWatch` of every descendant generation (§7.1).

  The closing window waits on them like its own tabs (`closing.rs:398-429`). A single tab close does not block the
  window, but the slots stay held until the exits are observed.
- **Moving a root to another window** is close, then reopen (`open_thread` redirects to the owner,
  `threads.rs:3175-3184`). The safe transition:
  - the children are stopped, as above;
  - the **root principal is kept**: the same `delegation_roots` and root `delegation_workers` rows, found by thread
    id;
  - the reopened root session's first binding comes from `prepare_root`. It first awaits `StopPhase::Exited` of the
    previous root generation's watch. If that has not happened within 5 s it calls `kill_now`, then waits for the
    exit. So two root generations never hold live tokens or overlays at once;
  - the stopped children stay readable. They can be sent work again, which is a rebind (§5.4).

## 3. Child sessions

### 3.1 Factory (`child.rs`)

```rust
pub(crate) struct ChildSpec { worker: WorkerId, root: WorkerId, parent: WorkerId, depth: u8, profile: Profile,
    driver: Driver, task_mode: TaskMode, model: Option<String>, effort: Option<String>,
    scope: WorkspaceScope, cwd: PathBuf, backing_thread: String, provider_thread: String }
impl Supervisor {
    async fn prepare_child(&self, spec: &ChildSpec, generation: ProcessGeneration) -> Result<PreparedChild, ToolError>;
    fn build_child(self: &Rc<Self>, spec: &ChildSpec, prep: PreparedChild) -> Rc<ChatSession>;   // synchronous; spawns
}
```

`prepare_child` runs everything off the main thread:
- `services.resolve(profile)`;
- for agy: `hook_check(HookTarget::new(&env, Some(cwd)))`, a fresh proof in the child's own worktree (contract §5),
  then `agy_l2::scan` (§5.5);
- `launch::prepare_binding`, which mints the token and writes the Claude overlay. It is staged under
  `(worker, generation 1)`.

`build_child` calls `ChatSession::with_options(adapter, open, store, sink, approval, env, opts)`:

| Argument | Value |
| --- | --- |
| `adapter` | `services.make_adapter(driver, program)` |
| `open` | `OpenSession { cwd, mode: task_mode.as_mode(), model, effort, resume: None, new_session_id: Some(uuid)` for Claude`, approval_hook: false, delegation: None }`; the binding arrives through `hooks.binding()` |
| `store` | `SessionStore { handle, provider: Some(Some(provider_thread)) }`; the backing and provider threads are created in the spawn accept transaction |
| `sink` | `move \|env\| sup.on_child_envelope(&worker, env)`, never a window (§3.6) |
| `approval` | agy: `ApprovalHandle::bind_child(hook, thread, cwd, mode)` (Lane B), which wires the always rules **and** `set_gate`. Claude and Codex: `Err(None)` |
| `env` | `resolved.env` (an `AgentEnv`, which is `LaunchEnv`) with every `STRIP_ON_SPAWN` name added to its `unset` |
| `opts` | `SessionOptions { hooks: Some(ChildHooks), child: Some(ChildPolicy { gate: Rc::new(ChildAuthority::new(params)), publish: ChildPublisher, mode }) }` |

A child gets no adapter factory, tab, drawer, `SessionSlot` or view. The view never sends it a prompt directly:
inspector input goes through `Supervisor::submit_human` (§4.5).

### 3.2 Child phases (`readback.rs`)

```rust
pub(crate) enum ChildPhase { Launching, Bootstrap /* Claude */, ReadBack, Ready, Failed(ToolError) }
pub(crate) struct ReadBack { driver: Driver, task_mode: TaskMode, codex: Option<CodexRequested> /* Wave 0 type */,
    phase: ChildPhase, bootstrap_violated: bool }
impl ReadBack {
    pub(crate) fn on_line(&mut self, line: &str) -> ReadBackStep;   // fed by DelegationHooks::line
    pub(crate) fn on_tool_use(&mut self) -> ReadBackStep;            // Claude bootstrap: latches the violation
    pub(crate) fn on_turn_end(&mut self, state: TurnState) -> ReadBackStep;
}
pub(crate) enum ReadBackStep { Wait, SendBootstrap, Interrupt, SendTask, Stop(ToolError), Ok }
```

- Lines are parsed as JSON and dropped at once. Nothing is stored or logged.
- `LiveAuthority.valid` stays false until `Ready`, so every approval request before then is denied.
- The Codex expectation is `readback::codex_requested(&agent_core::codex::child_mode_policy(task_mode.as_mode()))`.
  - The child mapping, `child_mode_policy`, is a new `pub fn` from Lane B (§10.1). `mode_policy` is `pub` since Wave 0
    and keeps serving roots.
  - The Codex adapter uses that same child mapping to request the policy for a child, so the launch and the read-back
    share one source.
  - A `None` from `codex_requested` stops the child.

### 3.3 Read-back by provider (fail closed, owned by P4)

**Claude (amendment A1: an inert bootstrap turn before the task).**
1. With the process up, the supervisor sends `InputSource::Bootstrap` with the fixed text
   `"agent-terminal startup check: reply with exactly OK and do not use any tool."`.
2. While `bootstrap == true`, every `can_use_tool` request is denied by the gate.
3. **A violation is latched.** Any `tool_use` the stream shows in the bootstrap turn sets `bootstrap_violated = true`
   and sends `interrupt_run()`. A latched violation never clears for that generation.
4. On `system/init`, check `claude_child_init_ok(task_mode, init.tools, init.mcp_servers)`.
   - The fixture `d6_restricted/frames.ndjson` shows init arrives right after the first user frame (lines 3–4).
   - Init's `model` is recorded for diagnostics only; the contract says it is not evidence.
   - The bootstrap turn's `assistant.message.model` becomes the identity's `provider_reported` value.
5. On the bootstrap turn's end, `SendTask` happens **only if all** of these hold:
   - the turn ended `TurnState::Completed` (not interrupted, not failed);
   - `bootstrap_violated == false`;
   - init was received and accepted;
   - the model was recognised.

   Otherwise:
   - a tool use, an interruption, a failed completion, a rejected init, or no init: `Stop(route_unverified)`, which
     runs `shutdown(true)` and sets the worker and the operation `failed`;
   - an unrecognised model (Claude's `unrecognized_model`): `provider_unavailable`.

   Nothing ever moves from a failed or latched bootstrap to `SendTask`.
6. **Every** new Claude child generation (rebind, respawn) repeats steps 1–5 before its first real input.

This costs one small turn per Claude child generation, about a system prompt plus "OK". It is visible in the child's
transcript as a labelled startup check.

**Codex.**
1. Before any input, the `thread/start` or `thread/resume` `result` must pass
   `readback::codex_policy_read_back(&requested, result)`: an equal `approvalPolicy` and an equal `sandbox.type`. The
   recorded shape is `d6-sandbox-policy.app-server.jsonl`. A missing field or a mismatch gives
   `Stop(route_unverified)`. For an Ask or Accept edits child, the expectation is `untrusted` + `workspaceWrite`
   (§10.1).
2. Every `mcpServer/startupStatus/updated` notification, **for the life of every generation**, must name
   `agent_terminal`. Any other name, including `codex_apps`, at any time:
   - `interrupt_run`;
   - `revoke(GenerationExited)` for that worker, which kills the token;
   - `shutdown(true)`;
   - worker `failed` (`provider_unavailable`, reason "an MCP server other than agent_terminal started").
3. The task is sent only once (1) has passed and `agent_terminal` has reported ready, or 10 s have passed with no
   foreign name and the bridge's hello is in. Otherwise `Stop`.

**agy.** The hook proof in the child's worktree and L2 run before the launch (§3.1). The post-start L2 recheck runs at
hello (§4.3). The canary (`session.rs:1356`) stays active. No bootstrap turn.

**P4 and P5 split.** P4 does all the checking, the reactions and the fail-closed tests, using raw lines from
`DelegationHooks::line`, so no adapter change is needed. P5 makes the Codex adapter request the right config
(`codex_child_config`, the G-X1 fallback home). Until then the route stays `Unverified`. Fixture lines for the tests
come from `scripted_codex.py` (Lane B extends it) and `scripted_claude.py` (Lane A).

### 3.4 Session changes (Lane B, `session.rs` and `session/delegated.rs`)

New `Inner` fields:
- `hooks`, `child`;
- `process_generation: Cell<u64>`, bumped only in `start_process`. The existing `generation` is also bumped by
  `stop_current` (`session.rs:1908`), so it is not the contract's process generation;
- `watches: RefCell<Vec<(u64, ExitWatch)>>`, pruned only when a watch reports `Exited`;
- `rebinding: Cell<bool>`.

Changes to existing functions:
- `launch_spec` (`:1037`): `hooks.binding(driver, next)` is appended after the profile and approval environment.
  `argv_tail` goes after the adapter's argv, and `open` goes into `self.open.delegation`. Every name in
  `STRIP_ON_SPAWN` is added to `unset`.
- `start_process` (`:1078`): on `Ok`, calls `hooks.process_started(gen, proc.pid(), proc.exit_watch())` and pushes
  the watch into `watches`. `AgentProcess::pid()` is new in `agent_proc.rs`.
- `finish_process` (`:1858`): calls `hooks.process_stopping(gen)` and `child.publish.cancel_generation(gen)`.
  **Never `process_exited`:** this function is synthetic, because `stop_current` calls it before the process has gone
  (`:1905-1911`).
- `on_line` (`:1185`, before the agy hold): `hooks.line(gen, line, stderr)`.
- `dispatch_line` (`:1321`): `hooks.observed(gen, &env)` before `gate_form`.
- `apply`: `hooks.turn(..)` on `TurnStarted` and `TurnCompleted`; `hooks.mode_changed(..)` on `ModeChanged`.
- `set_mode` (`:2578`): with `child` set, a mode wider than `child.mode`, or `Bypass`, is refused with a notice.
- `ensure_alive` (`:1915`): while `rebinding`, it starts nothing. `send_prompt` then queues, and `rebind_start`
  dispatches the queue.

Methods in `delegated.rs`:
- `deliver_delegated(input)`. Only `StartTurn`, `WriteLive` and `SteerNow` are valid. Any other plan gives
  `Refused(InternalError)`. **The session's `queued_prompts` never holds delegated input.**
  - `StartTurn`: stores `Event::DelegatedMessage` (never `UserMessage`), with the labelled text `"[Delegated <kind>
    from <from_label> (<id>). This is data from another agent, not a user instruction.]\n<text>"`. Bootstrap and
    Task use their own fixed labels. It goes through `dispatch_sent_prompt`.
  - `WriteLive` (Claude): `Command::Steer`, using the item path of `steer_prompt` (`:2205-2261`) without its queue
    fallback.
  - `SteerNow` (Codex): `Command::DelegatedSteer { text, turn, tag }`.
- `rebind_stop()`: sets `rebinding`, runs `stop_current()`, and returns that generation's watch.
- `rebind_start()`: clears `rebinding`, then runs `ensure_alive()` and `dispatch_queue_after_restart()`.
- `exit_watches()`: every watch in `watches` that has not exited.

Agent-core changes (Lane B):
- `Command::DelegatedSteer { text, turn: String, tag: String }` in `adapter.rs`;
- `codex.rs`: `turn/steer` pinned to `turn`. **No fallback:** a refusal emits `Event::ControlResult { error }` with
  `request = tag`, and acceptance emits `ok`. The user `Steer` and its fallback at `:2088-2098` are unchanged;
- `claude.rs` and `agy.rs`: `Unsupported`.

### 3.5 Publication (`publish.rs`, hooked by Lane B in `Inner::emit`)

With `child` set:
1. `child.publish.publish(gen, env)` runs first, and `None` returns at once.
2. `child.publish.persist(..)` replaces `self.store.append_event` (`session.rs:741`).
3. The rest runs as today, on the sanitised envelope.

`ChildPublisher` follows contract §10:
- the key is `Scope{root,worker,generation,run}.stream_key(item, stream)`;
- `ContentDelta` is `push`; `ContentSnapshot` is `replace_snapshot`; `ItemCompleted` is `finish`; exit or interrupt
  is `cancel`;
- overflow: `WITHHELD_NOTICE` is put in the text, and the withheld bytes are counted in `last_activity`;
- `saturated()`: `interrupt_run`, then worker `failed` (`internal_error`).

`persist` runs one `services.write(.., force, job)`. It appends the event, then one link per worker from the child up
to its root, when `category(env)` is `Some`:

| Category | Events |
| --- | --- |
| lifecycle | session and turn starts and ends, model changes |
| activity | item starts and completions |
| approval | approval events |
| message | `DelegatedMessage` |
| error | errors |
| result | §4.4 step 7 |

Token deltas are never linked. The event and its links commit together (plan §3, "atomic visibility").

### 3.6 Scoped routing

`on_child_envelope(worker, env)`:
1. Wraps it as a `ScopedEnvelope`.
2. Updates the worker's state: P1 `worker_step`/`run_step`, and identity through `attest.rs`.
3. Wakes the waits that match.
4. Fans out to the weak subscribers.

Root isolation is structural: a child's sink never reaches the root `ChatSession` or `on_thread_envelope`
(`threads.rs:3782-3788`). Two children can each use native request id 1, because every action is scoped by
`(worker, generation)`.

## 4. IPC and the write protocol

### 4.1 Listener (Lane C)

`IpcServer::bind(dir, app_pid, limits, dispatcher)` builds `dir/delegation-<app_pid>.sock`, chmod `0600`, with
`gio::SocketListener` and `UnixSocketAddress`. It follows the `ApprovalHandle::bind_with_home` pattern
(`approval_server.rs:616-693`) and runs the accept loop of `spawn_accept_loop` (`:1074`).
- Every connection is a `glib::spawn_future_local`.
- A connection over `admit_connection` (32 in total, 8 pre-auth) is closed at once.
- `Drop` closes the listener and unlinks the socket.

### 4.2 Connection state machine (`ipc/conn.rs`)

```
PreAuth --hello ≤5 s--> Authenticating --Welcome--> Authenticated
   |                         `--Refused (flush ≤5 s, close)
   `-- other frame / deadline / bad length → Closed
Authenticated: ConnState::on_frame → Admit(id) → dispatcher.call → enqueue result
                                    → RejectCapacity(id) → capacity_reached immediately
                                    → CancelCall(id) → dispatcher.cancel
               ProtocolViolation (second hello, duplicate id, unknown tag) → Closed
```

- **Reading:** read only while `FrameDecoder::wants_bytes() > 0`, with `read_future(min(wants, 64 KiB))`. Once a
  header is in, the body read gets a 5 s `future_with_timeout`.
- **Writing:** an outbox bounded at 16 frames and 4 MiB, with one pump task. Each write gets 5 s. Overflow, an error
  or a timeout closes the connection, then calls `dispatcher.closed`.
- **Peer facts:** `conn.socket().credentials()`, then `unix_pid()`.

### 4.3 Hello (Lane A `authenticate`, using C's `peer.rs`)

1. `negotiate`, then the instance nonce with `InstanceNonce::ct_eq`.
2. The token is compared with `CapabilityToken::ct_eq` against **every** live token slot, with no early exit:
   ```rust
   struct TokenSlot { worker: WorkerId, generation: ProcessGeneration, token: CapabilityToken, role: Role,
       driver: Driver, consumed_by: Option<u32>, live_conn: Option<ConnId>, agy_pid: Option<u32>, issued: Instant }
   ```
   No match: `authentication_failed`. A slot with a live connection is refused.
3. Claude and Codex: L3 is advisory, logged only. Sequential re-hello is allowed.
4. agy: P1 `hello_verdict`, using `ProcFacts` and `census` in `spawn_blocking`:
   - `RefuseNoConsume`: refused, with a notice coalesced per pid;
   - `RefuseLostIpc`: worker `rebind_required`;
   - `Compromise`: `revoke(GenerationExited)` and `workspace_untrusted`;
   - `Accept`: `census_ok`, then the post-start `agy_l2::scan`. Only then is the token consumed and `Welcome` sent;
   - the census and L2 run again before the first call is admitted;
   - the 30 s hello window starts at `process_started`.

### 4.4 Tool calls (`ops.rs`)

Every call:
- **E2:** the principal comes from the `ConnId`. `parse_request` and `validate`. Attestation through P1
  `ObservedCalls`, waiting up to 2 s; for agy every tool needs it.
- **E3:** the live ceiling is `ceiling(principal_mode())`, where `principal_mode()` is the root session's
  `effective_mode()` (with `session_safe`) or the child's task mode. Depth is checked too.
- `guard_output` on every result.

Read-only tools:
- `capabilities`: built, then `truncate_capabilities`.
- `list`, `read`, `wait`: `services.read`, then `reserve` and `settle`. Presenting messages queues
  `delegation_mark_presented` before the reply.
- `wait`: `admit_wait`, then a timer. Released on `closed`, on cancel, or at the principal's turn end.

Mutations follow plan §5:
1. **Idempotency read** (off-thread), then P1 `idempotency` and `replay`.
2. **Resolve:** the target, the cancel bound set and `workspace::plan_lease`. Admission: `RouteTable`,
   `available_for_delegation`, `workspace_allowed`, `admit_spawn`, `delivery_allowed`, `plan_delivery`.
3. **Accept, durable.** It rechecks the key and that the persisted epoch equals the memory epoch, then inserts the
   operation (receipt frozen) and the spawn rows or message row, plus an event and its links. When dispatch is
   immediate, the **reservation is in the same transaction**: op and message `dispatch_reserved`.
4. **Prepare** (spawn, async): `create_lease`, `prepare_child`. On failure the op is `failed`, and a clean fresh
   worktree is removed.
5. **E7, then the write, with no `.await` in between.** The checks:
   - the tree is not revoking;
   - `tree.epoch == op.epoch`;
   - the app is not shutting down;
   - the recipient's mode is still within the sender's live ceiling;
   - for `this_run`, the run is still live;
   - the worker is not `dispatching` another entry.

   Then `build_child`, `deliver_delegated` or `interrupt_run`. A failed check: `failed(stale_epoch|stale_run)`, and
   nothing is written.
6. **Record** (normal writes): `submitted`. Then, from hooks:
   - `ControlResult{tag}`: `provider_confirmed`, or a refusal (§4.5);
   - Codex `userMessage` completed: `provider_confirmed`;
   - Claude `EchoQueue` (needs P5's `--replay-user-messages`; until then the delivery ends `unknown`).
7. **Complete.** On the run's end, one writer job writes:
   - the terminal run state and usage;
   - a bounded `result` event, linked to every ancestor;
   - the parent's mailbox entry (`report`).

   Wake-ups follow the commit.

A spawn's reservation (step 3) covers launching the process **and** the task delivery. For Claude the task goes
after the bootstrap turn, so E7 is rechecked at that write, synchronously, in the `SendTask` handler.

### 4.5 Deferred deliveries (`deferred.rs`; fixes B2)

Every delivery not written at acceptance waits in the **supervisor's** per-worker queue, never in the session's:
- a `QueueNextTurn` plan;
- agy while a result is pending;
- a Codex `auto` steer refused (`ControlResult` error with `on_steer_refused(Auto) == QueuedNextTurn`): the message is
  recorded as `queued_next_turn` in a normal write;
- a send to a stopped worker that needs a rebind.

```rust
pub(crate) struct Deferred { operation: OperationId, message: MessageId, epoch: AuthorityEpoch,
    recipient_mode: TaskMode, sender: WorkerId, text: String, cancelled: Cell<bool> }
pub(crate) struct DeferredQueue { entries: VecDeque<Rc<Deferred>>, dispatching: Option<OperationId> }
impl Supervisor {
    fn drain_deferred(self: &Rc<Self>, worker: &WorkerId);   // on TurnEnded, on Ready, after rebind_start
}
```

`drain_deferred` takes the head entry only when the worker is idle and `dispatching` is `None`, then:
1. Sets `dispatching = Some(op)`, the single flight per worker.
2. Reserves durably: message `queued` or `queued_next_turn` to `dispatch_reserved`, op to `dispatch_reserved` when it
   was still `accepted`.
3. Back on the main context, **synchronously, with no yield**, runs E7. It fails when `entry.cancelled`,
   `entry.epoch != tree.epoch`, the tree is revoking, or `recipient_mode` is outside the sender's live ceiling. On
   success, `deliver_delegated(StartTurn)`.
4. Records the result and clears `dispatching`.

A reserved entry that fails E7 becomes `failed(stale_epoch)`. It never returns to the queue: P1 `delivery_step`
allows no `DispatchReserved → Queued`.

**Revocation:** `revoke` step 1 (§5.3) sets `cancelled` on every in-scope entry and removes it. The durable
`delegation_invalidate_unreserved` then marks those rows `cancelled`. An entry already reserved fails E7 on its epoch.

**Human input to a child:** `Supervisor::submit_human(worker, text)`, for P6. It is the same queue with source
"human". It still needs reserve and E7, so a revoked tree cannot be driven from the inspector.

After a restart, the queue is rebuilt from `delegation_deferred(worker)`. Every row was cancelled by the startup epoch
bump (§7.3), so it starts empty.

## 5. Authority

### 5.1 Provider-aware effect (`authority::effect`, Lane D; fixes B5)

| Driver | Read | Edit | Exec | Network | Delegate | Mcp |
| --- | --- | --- | --- | --- | --- | --- |
| Claude | `Read`, `Glob`, `Grep`, `LS`, `TodoWrite`, `AskUserQuestion` | `Edit`, `Write`, `MultiEdit`, `NotebookEdit` | `Bash`, `BashOutput`, `KillShell` | `WebFetch`, `WebSearch` | `Task`, `Agent` | `mcp__<server>__*` |
| Codex | none | `file_change`, `codex_permissions` (`PERMISSIONS_TOOL`) | `command_execution` | none | none | `mcp__<server>__*` |
| agy | `approval_server::classify` gives `ReadOnly` | `classify` gives `Edit` | `run_command`, `send_command_input` | `read_url_content`, `search_web` | `start_subagent`, `invoke_subagent`, `define_subagent`, `manage_subagents`, `browser_subagent` | `call_mcp_tool` (server from input) |

Anything else is `Unknown`. `own` is true only for server `agent_terminal`.

### 5.2 `ChildAuthority::judge` (deny first, before any auto-allow)

1. `!live().valid` or `live().bootstrap`: `Deny`.
2. `Delegate`, or `Mcp { own: false }`: `Deny`. Children get no native sub-agents and no foreign MCP.
3. `task_mode == Plan`: only `Read` and `Mcp { own: true }` pass to `Defer`. `Network` is `AskOnly`. **Edit, Exec
   and Unknown are `Deny`.**
4. `Unknown` in other modes: `AskOnly`, never auto-allowed.
5. Every edit target of an `Edit` (Claude `file_path` or `notebook_path`, Codex `codex_change_targets`, agy
   `edit_targets`) must pass `approval_server::path_auto_matchable(canonical, worktree, home)`
   (`approval_server.rs:1287`). Otherwise `AskOnly` ("outside this agent's worktree" or "sensitive file").
6. Otherwise `Defer`.

### 5.3 Rules in children (user decision; amendment A4)

```rust
fn rule_may_apply(&self, action: &Action, cwd: Option<&str>) -> bool
```

It is true only when **all** of these hold:
- `live().valid`;
- the action is `Edit { path }` or `Read { path }`. Shell and MCP rules never apply in a child;
- the canonical path is inside the child's own lease worktree:
  - `Edit`: `path_auto_matchable(path, worktree, home)`;
  - `Read`: `read_auto_matchable(path, worktree, home)`, or the shared root for a `shared_read_only` child;
- for `Edit`: `ceiling(parent_mode)` contains the child's `task_mode`, and `task_mode.writes()`. A Plan child or a
  restricted parent never gets a rule-allowed edit.

The filter sits exactly here:

| Path | Where | What changes |
| --- | --- | --- |
| Claude and Codex rule match | `gate_approval`, `session.rs:1595-1598` | `in_session \|\| saved_allow` becomes `gate.rule_may_apply(a, cwd) && (in_session \|\| saved_allow)` |
| Claude and Codex buttons | `gate_approval` `options.retain`, `:1566-1570` | `AllowForSession` and `AllowAlways` only if `rule_may_apply` |
| Claude and Codex keep | `respond_approval_with_rule` / `plan_rule` (`:2495`, `:1684`) | re-checked. If false, allow once and keep nothing, with a notice |
| Codex Accept edits auto | `gate_approval` `accept_edit`, `:1584-1587` | also requires `rule_may_apply(Edit{path})` for every target |
| Codex permissions | `hold_codex_permissions`, `:3250` | rules never apply (today's rule); gate `judge` first |
| agy rule match | `approval_server::serve`, `rules_allow` at `:1204-1217` | `gate.rule_may_apply(a, cwd) && rules_allow(..)` |
| agy buttons | `rule_options` call, `:1227` | grant options only if `rule_may_apply` |
| agy keep | `ServerInner::remember`, `:969` | re-checked |
| agy policy auto | `serve` before `policy_with_home`, `:1184` | `judge` runs first. Bypass never exists for children; the Accept-edits auto stays bounded by `workspaces = [worktree]` |

`judge` itself runs first on every path (§5.2 placement): at the top of `gate_approval` after the agy return, at the
top of `hold_codex_permissions`, and in `serve` before `policy_with_home`.
- `Deny` answers deny at once: Claude through `Command::Approve` and `ApprovalResolved`, agy through `deny(conn)`.
- `AskOnly` forces a card with no grant options.

Children keep the user's Always rules file (agy: `bind_child` calls `use_always_rules`; Claude/Codex: the normal
`saved_rules`) and their own session rules. The filter above is the only thing that limits them.

### 5.4 Revocation and rebind

`revoke(root, event)`:
1. **In memory, synchronously:**
   - `tree.revoking = true`, so E7 and `live()` fail from here on;
   - `effect = revocation(event, snapshot)` (P1);
   - every slot in `kill_tokens` is removed, with `ipc.revoke(conn, err)`;
   - in-scope deferred entries are cancelled;
   - in-scope waits are released;
   - `expire_approvals` runs on in-scope sessions.
2. **Stop work:** `interrupt_run` on every active run in scope, and `shutdown(true)` on the sessions in
   `stop_processes`. Their watches stay held (§7.1). A Root Stop also calls `root_session.interrupt()`.
3. **Durable:** `delegation_bump_epoch`, `delegation_invalidate_unreserved`, the statuses (`restricted`, `closed`),
   the states, and a lifecycle event with its links.
4. **After commit:** `tree.epoch = new`, `revoking = false`. On failure it stays revoking, everything stays refused,
   and `services.report` shows the error.

Triggers:
- Root Stop: `SessionSlot::interrupt` (`threads.rs:341`), and `root_closing`.
- Restriction: `mode_changed` with `ceiling(new) ⊂ ceiling(old)`. Bypass to Ask is not a restriction.
- Close: the close op.
- `GenerationExited`: `process_stopping` (fail closed: the token dies at the synthetic stop, not later).
- Shutdown: `shutdown_all`.

**Rebind** (§6 of the contract; for children before their next deferred or dispatched turn; for roots in
`before_root_turn`):
1. Stage a binding: `prepare_binding` in `spawn_blocking`, plus L2 for agy, for generation `current + 1`.
2. `let w = session.rebind_stop()`, then **await `w` reporting `Exited`**. After 5 s, `kill_now`, then wait for the
   exit. There are never two live generations of one worker.
3. On the main context: E7 for a child's pending delivery, or the tree check for a root, then `rebind_start()`. The
   new process takes the staged binding.

For Claude children the bootstrap read-back (§3.3) runs again before any input. Codex rebinds by restarting the
app-server with `thread/resume` (amendment A2). That **loses Codex's session grants and pending permission cards**:
`finish_process` clears them (`session.rs:1880-1885`), the cards expire and are asked again, and a notice says so.

`before_root_turn` returns `Deferred` whenever a rebind is needed, for every driver. `SessionSlot::send_prompt`
(Lane E) then calls `queue_prompt`. The supervisor rebinds, then `rebind_start` dispatches the queue. A session restart
the supervisor did not stage takes no binding, so it gets no token (fail closed). Those paths are
`reload_session` (`:1931`), `switch`, `trip_canary` (`:1764`) and `retry_without_thinking` (`:1813`). The worker then
becomes `rebind_required`.

### 5.5 Bindings prepared off the main thread (`launch.rs`, `rundir.rs`; fixes S7)

`prepare_binding(&BindingRequest) -> Result<PreparedBinding, String>` blocks and runs only in `gio::spawn_blocking`:
1. Mints 32 random bytes from `/dev/urandom` (a std `File` read; no new crate), then `CapabilityToken::from_random`.
2. Builds `DelegationBinding`.
3. Claude: writes `overlay_file_name(app_pid, worker, generation)` in the verified runtime dir with
   `O_WRONLY|O_CREAT|O_EXCL|O_NOFOLLOW|O_CLOEXEC`, mode `0600`, checking owner and mode with `fstat` before the
   write.
4. Fills the `LaunchBinding`:
   - Claude root: `claude_parent_args(overlay)`;
   - Claude child: `claude_child_args(mode, overlay)`;
   - agy: `env(app_pid)`;
   - Codex: `open` (root: `codex_thread_config`; child: P5 swaps in `codex_child_config`).

The supervisor stages the result under `(worker, generation)`. `DelegationHooks::binding(driver, g)` returns it only
if `g` and the driver match, else `None`. A staged binding is used at most once and dropped at the next stage.

`rundir::verified()` creates the directory `0700` if it is absent, then checks with `lstat` that it is a directory,
not a symlink, owned by our uid, mode `0700`. It runs once, off-thread, at `start`.

## 6. Worktree leases (Lane D, `workspace.rs`)

```rust
pub(crate) struct LeasePlan { pub source_repo: PathBuf, pub base_commit: String, pub branch: String, pub path: PathBuf, pub dirty_parent: bool }
pub(crate) struct WorktreeLease { pub source_repo: PathBuf, pub worktree: PathBuf, pub branch: String, pub base_commit: String }
pub(crate) enum Cleanup { Removed, Retained(String) }
```

All of these block, so they run in `spawn_blocking`.
- `plan_lease`, at acceptance:
  - `caller_tree` is the caller's own tree: the root thread's cwd, or the parent child's worktree;
  - `worktree::main_toplevel` (`worktree.rs:63`);
  - `resolve_base` (`:132`), stored in `resolution_json`;
  - branch `agent-terminal/<worker_id>`, checked with `check_new_branch` (`:101`);
  - `default_path` (`:32`);
  - when the caller's tree is not clean, the warning `parent_changes_absent`.
- `create_lease`: `worktree::add` (`:157`), canonicalisation, `HEAD == base_commit`, and no symlink component on the
  path.
- `verify_lease` runs before every relaunch.
- `cleanup_if_clean` runs only after close, only when the worktree is clean and no tab is open in it. It uses
  `worktree::remove` (`:228`), never forced, and the branch is kept. A dirty worktree is `Retained`.
- `scope(lease)` gives `WorkspaceScope::Lease`.

`shared_read_only` needs `plan` and gives `WorkspaceScope::Shared { root: caller_tree }`.

## 7. Lifetime

### 7.1 Observed exits only (fixes B3)

```rust
struct GenRt { generation: ProcessGeneration, watch: ExitWatch, overlay: Option<PathBuf>, exited: bool }
// WorkerRt.generations: BTreeMap<ProcessGeneration, GenRt>; the root principal has the same map.
```

- `process_started` stores the watch and connects `connect_phase`. If `watch.phase()` is already `Exited`, it is
  handled at once.
- Only `StopPhase::Exited`, for that generation's own watch, runs `observed_exit(worker, g)`:
  - `capacity.process_exited`;
  - `launch::unlink_overlay`;
  - the entry is removed;
  - if it is the current generation and no deliberate stop was asked for, the `GenerationExited` revocation (it is a
    no-op if `process_stopping` already ran it).
- `process_stopping`, the synthetic stop, kills the token and ends the runs. It frees nothing else.
- `shutdown_all` and `root_closing` return a `ChildStop` for **every** `GenRt` not yet exited, across all
  generations, rebind leftovers included.

### 7.2 Closing (Lane E)

- `close_request` (`imp.rs:1250`): before `shutdown_thread_sessions()`, run `root_closing(thread)` for every root in
  this window. If it is the last window not closing, also `shutdown_all()`. Map each `ChildStop` to `StopTarget`
  ("Codex (delegated: <name>)") and add it to `stopping`.
- `close_row_now` (`threads.rs:2814`): `root_closing(thread)` before the session shutdown. The tab does not wait;
  the slots do (§7.1).
- `ChatTab::busy` (`threads.rs:222`) also returns `root_busy(thread)`, so the existing confirmation asks first.
- `shutdown_all`: `close_all(runtime_unavailable)`, then `shutdown(true)` on every child, then all unexited watches,
  then the socket is removed.
- `main.rs` `connect_shutdown` (`:219-230`): `final_cleanup()` before `terminate_all()`. That unlinks only overlays
  whose generations have exited, and `sweep_overlays` handles the rest at the next start. Children are
  `AgentProcess`es in `LIVE`, so `terminate_all` still catches stragglers.

### 7.3 Startup reconciliation (`reconcile.rs`)

One durable `delegation_reconcile_startup` runs before the socket is bound:
- runs in `reserved`, `starting`, `running` or `stopping` become `unknown`;
- operations in `dispatch_reserved` or `submitted` become `unknown`;
- every root gets `authority_epoch + 1` and `stream_epoch + 1`, and `active` becomes `stopped`;
- `accepted` operations become `failed(stale_epoch)`;
- `queued` and `queued_next_turn` messages become `cancelled`; `dispatch_reserved` and `submitted` messages become
  `unknown`;
- open workers become `unknown`, with status `unavailable` ("app restarted").

No pid is signalled and nothing is re-dispatched. Then `sweep_overlays`, which keeps only the files of live
agent-terminal pids, checked through `/proc/<pid>/exe`. Until reconciliation finishes, every call gets
`runtime_unavailable`.

### 7.4 Capacity (`capacity.rs`)

- A run slot is taken at accept and freed at the run's terminal state.
- A pending start counts until the run is `running`.
- A process slot is taken at `process_started` and freed **only** in `observed_exit`. A rebind briefly counts both
  generations, which is truthful.
- A spawn calls `admit_spawn(needs_process = true)`.
- A turn on an idle live child calls `admit_spawn(false)`; on an exited child, `admit_spawn(true)`.
- The run deadline (default 15 min) calls `interrupt_run`, and the terminal reason is `deadline`.

### 7.5 Config (Lane E)

`Profile.available_for_delegation: bool` (`#[serde(default)]`), with a round-trip test.

## 8. Tests

**Pattern.** Copy `session/scripted_tests.rs`: a private store, `python3 -I tests/support/<fake>.py`, `minimal_env`,
`in_loop` and `pump_until`.

New fakes:
- `scripted_claude.py` and `scripted_agy.py` (Lane A);
- `scripted_codex.py` extended (Lane B) with steer refusal, `thread/start` sandbox and policy results, and
  `mcpServer/startupStatus/updated` frames at any point.

Fakes read `tests/fixtures/delegation/<scenario>.json` and write `frames.ndjson` and `env-digest.json`. The digest
holds names and sha256 values, never the token. IPC tests use an in-process `UnixStream` client; L3 uses a fake
`ProcFacts`. Nothing reaches a provider or the network. Everything runs under `make test`.

| # | Test | Lane |
| --- | --- | --- |
| 1 | `spawn_fake_child_runs_task_records_run_and_result` | A |
| 2 | `claude_bootstrap_precedes_task`, `claude_bootstrap_denies_every_can_use_tool`, `claude_init_with_task_tool_stops_route_unverified_task_never_sent`, `claude_valid_init_then_tool_use_latches_and_never_sends_task`, `claude_bootstrap_interrupted_completion_stops_route_unverified`, `claude_bootstrap_failed_completion_stops`, `claude_unrecognised_model_is_provider_unavailable`, `claude_rebind_repeats_bootstrap` | A+B |
| 3 | `codex_ask_child_requests_and_reads_back_workspace_write` (Lane B mapping), `codex_root_ask_keeps_read_only`, `codex_sandbox_mismatch_stops_before_input`, `codex_policy_mismatch_stops`, `codex_missing_result_fields_stop`, `codex_foreign_server_at_start_stops`, `codex_foreign_server_mid_turn_stops_and_kills_token`, `codex_no_ready_in_10s_stops` | A |
| 4 | `plan_denies_edit_exec_unknown_for_claude_codex_agy` (table-driven over §5.1), `unknown_tool_never_auto_in_ask`, `delegate_and_foreign_mcp_always_denied` | D (+B wiring) |
| 5 | Rules: `always_edit_rule_inside_lease_auto_allows`, `always_rule_for_source_tree_path_never_applies`, `shell_and_mcp_rules_never_apply_in_child`, `session_folder_offered_only_inside_lease`, `restricted_parent_disables_edit_rules`, `rule_rechecked_at_keep` (all three providers) | B+D |
| 6 | `edit_relative_in_lease_auto_in_accept_edits`; `absolute_into_source_tree_asks_without_grant_buttons`; `sensitive_in_worktree_asks`; `symlinked_dir_in_worktree_never_auto` | D |
| 7 | `queued_next_turn_goes_through_reserve_and_e7`; `revocation_cancels_queued_entry_before_drain`; `revocation_between_reserve_and_write_fails_stale_epoch_writes_nothing`; `codex_auto_refused_steer_requeued_then_reserved_again`; `strict_steer_refused_is_stale_run_no_turn`; `human_inspector_input_refused_when_tree_revoked` | A (+B) |
| 8 | `stop_current_frees_no_slot_until_observed_exit`; `overlay_unlinked_only_after_exited`; `rebind_waits_for_old_generation_exit`; `shutdown_collects_every_unexited_generation` | A+B |
| 9 | `cancel_bound_set_fixed_at_accept`; `close_revokes_subtree_keeps_transcript`; `root_stop_kills_descendant_tokens_not_roots`; `root_restriction_marks_restricted_root_token_survives`; `restriction_between_accept_and_reserve_is_stale_epoch` | A |
| 10 | `root_page_close_stops_children_and_waits`; `root_reopen_keeps_principal_no_overlapping_generations` | A+E |
| 11 | capacity: `idle_child_holds_process_slot_not_run_slot`; `ninth_process_refused`; `followup_on_live_idle_child_needs_no_process_slot` | A |
| 12 | `delayed_link_never_skipped_by_cursor`; `no_cross_root_control`; `two_children_request_id_1_route_by_scope`; `child_turn_model_usage_exit_never_touch_root` | A |
| 13 | `split_secret_never_persisted_or_shown`; `token_only_in_env_overlay_or_open_never_argv`; `prepare_binding_runs_off_main_thread` (asserts the thread name); `overlay_0600_excl_refuses_symlinked_rundir` | A+B+D |
| 14 | `reserved_then_restart_is_delivery_unknown`; `accepted_before_restart_is_stale_epoch`; `reconcile_never_yields_running`; `durable_failure_refuses_dispatch` | A |
| 15 | IPC bounds (frames, 8/32 connections, hello, body and write deadlines, outbox overflow, duplicate id, second hello, 5th in-flight, one connection per token); agy hello verdicts and the 30 s window | C (+A) |
| 16 | L2: git stops at the toplevel; non-git walks to `/`; every source name; symlink cycles; the caps; unreadable entries | D |

## 9. Lanes

**Wave 0 (done on `feat/cross-agent-p4-wave0`).**
- `seam.rs` (§1.1) and every stub of §1.2 are committed.
- `session/delegated.rs` holds the session stubs; `mod delegated;` is in `session.rs` and `mod delegation;` in
  `main.rs`.
- `agent_core::codex::mode_policy` is `pub`.
- check, clippy `-D warnings`, `test --no-run` and fmt all pass.
- P3b still has to merge before Lane A's store work.

| Lane | New files | Existing files it may edit | Depends on |
| --- | --- | --- | --- |
| **B: session seam** | `src/chat/session/tests/delegated_tests.rs` (fills `delegated.rs`) | **`src/chat/session.rs` (only this lane)**, `src/approval_server.rs` (`set_gate`, `bind_child`, the `serve` and `rules_allow`/`rule_options`/`remember` filter), `src/agent_proc.rs` (`pid()`), `crates/agent-core/src/adapter.rs` (`DelegatedSteer`), `crates/agent-core/src/codex.rs` (`DelegatedSteer` arm; `pub fn child_mode_policy` and its use for supervised children, §10.1), `crates/agent-core/src/claude.rs` and `agy.rs` (one `Unsupported` arm each), `tests/support/scripted_codex.py` | Wave 0 |
| **C: IPC** | none (fills `ipc.rs`, `ipc/conn.rs`, `ipc/peer.rs`) | none | Wave 0 |
| **D: authority, workspace, launch** | none (fills `authority.rs`, `workspace.rs`, `agy_l2.rs`, `launch.rs`, `rundir.rs`) | none | Wave 0 |
| **A: supervisor core** | `tests/support/scripted_claude.py`, `tests/support/scripted_agy.py`, `tests/fixtures/delegation/**` (fills `mod.rs` and its own modules) | none | Wave 0, P3b. It compiles against the stubs, and its end-to-end tests turn green as B, C and D merge |
| **E: app integration** | none | `src/main.rs`, `src/window/mod.rs`, `src/window/imp.rs`, `src/window/imp/threads.rs`, `src/window/imp/closing.rs`, `src/config.rs` | A and B merged |

The threads.rs edits in Lane E are:
- `setup_shell` calls `start`;
- `prepare_root` is awaited before `with_options` at `:3858`;
- `SessionSlot::interrupt` and `send_prompt`;
- `close_row_now` and `ChatTab::busy`;
- `AppServices` closures over `make_adapter`, `cached_agent`, `unset_list` and `check_hook`.

B, C, D and A run in parallel; E runs last. No existing file has two lanes. No new crate.

## 10. Risks and open items

1. **Codex Ask child sandbox (coordinator decision: follow the contract).** Roots keep today's `mode_policy` (Ask is
   `untrusted` + `readOnly`, `codex.rs:2729`). Supervised children use a child-specific mapping, which Lane B adds:
   ```rust
   pub fn child_mode_policy(mode: Mode) -> (&'static str, Value);
   ```
   | Mode | `approvalPolicy` | `sandboxPolicy` |
   | --- | --- | --- |
   | Ask | `untrusted` (today's Ask policy: writes and commands still ask) | `{"type": "workspaceWrite"}` |
   | AcceptEdits | `untrusted` | `{"type": "workspaceWrite"}` |
   | Plan | `never` | `{"type": "readOnly"}` |
   | Bypass | never used: a child refuses Bypass | |

   - The child's cwd is its lease worktree, so `workspaceWrite` is bounded to that worktree.
   - The Codex adapter uses `child_mode_policy` when `OpenSession.delegation` marks a child launch. Roots are
     unchanged.
   - The read-back (§3.3) expects the same pair, through `readback::codex_requested`.
   - **Codex Ask (and Accept edits) child routes stay `Unverified`** until `codex_ask_child_requests_and_reads_back_
     workspace_write` passes against the scripted fixture and a recorded P5/P8 run confirms the read-back.
2. **Claude bootstrap residual.** Init arrives before any assistant output in the fixture, and the model is told to
   use no tool. Read-only tools in `--tools` (`Read`, `Glob`, `Grep`) do not raise `can_use_tool`. In the worst case
   the bootstrap turn reads files in its own worktree, which is plainly permitted. Any `tool_use` latches a violation:
   an interrupt, then `route_unverified`, and the task is never sent.
3. **Claude echo confirmation** needs P5's `--replay-user-messages`. Until then deliveries end `unknown`.
4. **Unstaged agy and Claude restarts** degrade delegation until the next staged rebind. P6's banner must say so.
5. **`queued` spawn state.** P1 `admit_spawn` refuses at 4 active runs (`limits.rs:172-196`), so `queued` is never
   returned. This is compliant.
6. **Native import.** A Claude child's session file under `~/.claude/projects/<worktree>` must stay out of "Resume
   session". Lane E verifies that the importer skips native ids the store already knows.
7. **PR #44** is still unmerged, so agy child routes stay blocked.
