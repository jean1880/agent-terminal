//! Authenticated tools and the durable acceptance/reservation/write protocol.
use super::seam::{CallDispatcher, ConnId, LocalFuture, PeerFacts, ProcFacts};
use super::supervisor::{error, opted_in, Supervisor};
use agent_core::adapter::Driver;
use agent_core::delegation::authority::{ceiling, permits, RevocationEvent, TaskMode};
use agent_core::delegation::correlation::{attestation_arguments, Attestation};
use agent_core::delegation::ipc::{
    negotiate, Call, CallOutcome, Hello, IpcLimits, Principal, Welcome,
};
use agent_core::delegation::operation::{ReplayOutcome, Resolution};
use agent_core::delegation::page::{
    decode_cursor, encode_cursor, filter_digest, CursorPayload, PageFilter,
};
use agent_core::delegation::route::{ChildBridge, DelegationCaps, RouteQuery};
use agent_core::delegation::wire::*;
use agent_core::delegation::{AuthorityEpoch, ErrorCode, OperationId, ToolError, WorkerId};
use agent_core::serde_name::parse_lenient;
use agent_kit::store::delegation_typed::*;
use gtk4::{gio, glib};
use serde_json::{json, Value};
use std::rc::Rc;
use std::time::{Duration, Instant};
use tracing::{info, warn};

/// Why a worker whose bridge connection closed needs a rebind.
pub(super) const LOST_BRIDGE: &str =
    "the delegation bridge connection was lost; it rebinds before the next turn";

/// `cross_agent_capabilities` with `refresh` waits at most this long for usage probes.
const USAGE_REFRESH_WAIT: Duration = Duration::from_secs(20);

impl CallDispatcher for Supervisor {
    fn authenticate(
        &self,
        conn: ConnId,
        hello: Hello,
        peer: PeerFacts,
    ) -> LocalFuture<Result<Welcome, ToolError>> {
        let weak = self.weak.clone();
        Box::pin(async move {
            let sup = weak
                .upgrade()
                .ok_or_else(|| error(ErrorCode::RuntimeUnavailable, "runtime stopped"))?;
            let api =
                negotiate(&hello.api).map_err(|c| error(c, "incompatible delegation protocol"))?;
            if !sup.instance.ct_eq(hello.instance.as_str()) {
                return Err(error(
                    ErrorCode::AuthenticationFailed,
                    "unknown app instance",
                ));
            }
            let worker = {
                let state = sup.state.borrow();
                let mut found = None;
                for (id, w) in &state.workers {
                    if let Some(t) = &w.token {
                        if t.token.ct_eq(hello.cap.expose_secret()) {
                            found = Some(id.clone());
                        }
                    }
                }
                found
            }
            .ok_or_else(|| {
                error(
                    ErrorCode::AuthenticationFailed,
                    "invalid or expired capability",
                )
            })?;
            sup.epoch(&worker)?;
            let (driver, pid, consumed, live, issued) = {
                let state = sup.state.borrow();
                let w = &state.workers[&worker];
                let t = w
                    .token
                    .as_ref()
                    .ok_or_else(|| error(ErrorCode::AuthenticationFailed, "expired capability"))?;
                (w.driver, t.pid, t.consumed_by, t.live_conn, t.issued)
            };
            if pid.is_none() {
                return Err(error(
                    ErrorCode::AuthenticationFailed,
                    "principal process has not started",
                ));
            }
            {
                let state = sup.state.borrow();
                let w = &state.workers[&worker];
                if w.row.parent_id.is_some() && !state.token_current(w) {
                    return Err(error(
                        ErrorCode::StaleEpoch,
                        "child credential belongs to an older authority epoch",
                    ));
                }
            }
            if driver == Driver::Agy {
                use agent_core::delegation::agy_guard::{
                    census_ok, hello_verdict, is_bridge_cmdline, stat_ppid, HelloVerdict,
                };
                let bridge = sup.services.bridge_command.clone();
                let peer_pid = peer.pid.filter(|p| *p == hello.bridge.pid).ok_or_else(|| {
                    error(ErrorCode::AuthenticationFailed, "missing bridge peer pid")
                })?;
                let facts = gio::spawn_blocking(move || {
                    use std::os::unix::fs::MetadataExt;
                    let p = super::ipc::peer::RealProc;
                    let exe = std::fs::metadata(bridge).ok().map(|m| (m.dev(), m.ino()));
                    let direct = p.stat(peer_pid).as_deref().and_then(stat_ppid) == pid;
                    let matches = exe.is_some() && p.exe_id(peer_pid) == exe;
                    let argv = p.cmdline(peer_pid).is_some_and(|v| is_bridge_cmdline(&v));
                    let census = pid
                        .zip(exe)
                        .map(|(pid, exe)| super::ipc::peer::census(&p, pid, exe))
                        .unwrap_or_default();
                    (direct, matches, argv, census)
                })
                .await
                .map_err(|_| error(ErrorCode::AuthenticationFailed, "cannot verify bridge peer"))?;
                match hello_verdict(
                    facts.0,
                    facts.1,
                    facts.2,
                    consumed,
                    peer_pid,
                    live.is_some(),
                ) {
                    HelloVerdict::Accept
                        if issued.elapsed()
                            <= Duration::from_millis(sup.limits.agy_hello_window_ms)
                            && census_ok(&facts.3, peer_pid) => {}
                    HelloVerdict::Compromise => {
                        let root = sup.state.borrow().workers[&worker].root.clone();
                        sup.begin_revoke(&root, RevocationEvent::GenerationExited(worker.clone()));
                        return Err(error(
                            ErrorCode::WorkspaceUntrusted,
                            "multiple direct-child bridges",
                        ));
                    }
                    HelloVerdict::RefuseLostIpc => {
                        sup.set_status(&worker, "rebind_required", "bridge connection was lost");
                        return Err(error(ErrorCode::AuthenticationFailed, "bridge must rebind"));
                    }
                    _ => {
                        return Err(error(
                            ErrorCode::AuthenticationFailed,
                            "bridge peer or launch window rejected",
                        ))
                    }
                }
                sup.recheck_agy(&worker).await?;
            } else if live.is_some() {
                return Err(error(
                    ErrorCode::AuthenticationFailed,
                    "capability already has a live connection",
                ));
            }
            let epoch = sup.epoch(&worker)?;
            let mut state = sup.state.borrow_mut();
            let w = state
                .workers
                .get_mut(&worker)
                .ok_or_else(|| error(ErrorCode::AuthenticationFailed, "expired worker"))?;
            let t = w
                .token
                .as_mut()
                .filter(|t| t.token.ct_eq(hello.cap.expose_secret()))
                .ok_or_else(|| {
                    error(
                        ErrorCode::AuthenticationFailed,
                        "capability revoked during hello",
                    )
                })?;
            if driver == Driver::Agy {
                if let Some(consumed) = t.consumed_by {
                    let compromise = Some(consumed) != peer.pid;
                    let root = w.root.clone();
                    drop(state);
                    if compromise {
                        sup.begin_revoke(&root, RevocationEvent::GenerationExited(worker.clone()));
                        return Err(error(
                            ErrorCode::WorkspaceUntrusted,
                            "another direct-child bridge consumed this capability",
                        ));
                    }
                    sup.set_status(&worker, "rebind_required", "bridge connection was lost");
                    return Err(error(
                        ErrorCode::AuthenticationFailed,
                        "single-use capability was consumed during hello",
                    ));
                }
                if t.issued.elapsed() > Duration::from_millis(sup.limits.agy_hello_window_ms) {
                    return Err(error(
                        ErrorCode::AuthenticationFailed,
                        "bridge hello window expired during verification",
                    ));
                }
            }
            if t.live_conn.is_some() {
                return Err(error(
                    ErrorCode::AuthenticationFailed,
                    "capability already connected",
                ));
            }
            t.live_conn = Some(conn);
            if driver == Driver::Agy {
                t.consumed_by = peer.pid;
            }
            let principal = Principal {
                worker_id: worker.clone(),
                root_id: w.root.clone(),
                driver,
                depth: w.row.depth as u8,
                can_spawn: w.row.depth < (sup.limits.max_depth as i64),
                task_mode_ceiling: ceiling(w.mode).to_vec(),
            };
            state.connections.insert(conn, worker);
            Ok(Welcome {
                api,
                connection: format!("c_{}", conn.0),
                principal,
                authority_epoch: epoch.0,
                limits: IpcLimits {
                    max_frame: sup.limits.max_frame,
                    max_inflight: sup.limits.max_inflight,
                },
            })
        })
    }
    fn call(&self, conn: ConnId, call: Call) -> LocalFuture<CallOutcome> {
        let weak = self.weak.clone();
        Box::pin(async move {
            let result = match weak.upgrade() {
                Some(s) => s.handle_call(conn, call).await,
                None => Err(error(ErrorCode::RuntimeUnavailable, "runtime stopped")),
            };
            match result.and_then(|v| guard_output(v, None)) {
                Ok(v) => CallOutcome::Ok(v),
                Err(e) => CallOutcome::Err(e),
            }
        })
    }
    fn cancel(&self, conn: ConnId, id: u64) {
        self.state.borrow_mut().waits.remove(&(conn, id));
    }
    fn closed(&self, conn: ConnId) {
        // A connection its own credential still held: the bridge went away (a rotation or a
        // revocation takes the token first, so neither lands here). Claude's bridge may
        // reconnect with the same token; the others cannot, so the worker needs a rebind, and
        // says why, durably, where `list` and the worker dock can see it.
        let lost = {
            let mut state = self.state.borrow_mut();
            state.waits.retain(|(c, _), _| *c != conn);
            let worker = state.connections.remove(&conn);
            worker.and_then(|worker| {
                let w = state.workers.get_mut(&worker)?;
                let t = w.token.as_mut().filter(|t| t.live_conn == Some(conn))?;
                t.live_conn = None;
                Some((worker, w.driver))
            })
        };
        let Some((worker, driver)) = lost else {
            return;
        };
        if driver == Driver::Claude {
            info!(%worker, "delegation bridge disconnected; it may reconnect");
            return;
        }
        warn!(%worker, ?driver, "delegation bridge connection lost; rebind required");
        self.set_status(&worker, "rebind_required", LOST_BRIDGE);
        self.notify_registry();
    }
}
impl Supervisor {
    pub(super) async fn existing(
        &self,
        caller: &WorkerId,
        request: &ToolRequest,
    ) -> Result<Option<Value>, ToolError> {
        use agent_core::delegation::operation::{
            canonical_request, idempotency, replay, IdempotencyDecision, OperationKind,
            StoredOperation,
        };
        // An acknowledging read has a key too, but its replay is the read path's own.
        let Some((key, expected)) = agent_kit::store::delegation_typed::key_and_epoch(request)
            .filter(|_| !matches!(request, ToolRequest::Read(_)))
        else {
            return Ok(None);
        };
        let epoch = self.epoch(caller)?;
        if expected.is_some_and(|e| e != epoch.0) {
            return Err(error(ErrorCode::StaleEpoch, "authority epoch changed"));
        }
        let principal = caller.to_string();
        let key = key.to_string();
        let kind = OperationKind::of(request).map_or_else(
            || "unknown".to_owned(),
            |k| super::supervisor::enum_value(&k),
        );
        let row = self
            .services
            .read(move |s| s.delegation_operation_by_key(&principal, &kind, &key))
            .await
            .ok_or_else(|| {
                error(
                    ErrorCode::PersistenceFailed,
                    "cannot read operation idempotency",
                )
            })?
            .map_err(|_| {
                error(
                    ErrorCode::PersistenceFailed,
                    "cannot read operation idempotency",
                )
            })?;
        let Some(row) = row else {
            return Ok(None);
        };
        let parse = |text: &str| {
            serde_json::from_str(text)
                .map_err(|_| error(ErrorCode::InternalError, "stored operation is invalid"))
        };
        let op = StoredOperation {
            id: OperationId::parse(&row.id)
                .map_err(|_| error(ErrorCode::InternalError, "stored operation id is invalid"))?,
            kind: parse_lenient(&row.kind).ok_or_else(|| {
                error(ErrorCode::InternalError, "stored operation kind is invalid")
            })?,
            epoch: AuthorityEpoch::from_stored(row.authority_epoch),
            canonical: agent_core::delegation::operation::CanonicalRequest::from_stored(
                row.canonical_request,
            ),
            resolution: serde_json::from_str(&row.resolution_json)
                .map_err(|_| error(ErrorCode::InternalError, "stored resolution is invalid"))?,
            receipt: Some(parse(&row.receipt_json)?),
            dispatch: parse_lenient(&row.dispatch_state)
                .ok_or_else(|| error(ErrorCode::InternalError, "stored dispatch state invalid"))?,
            error: row
                .error_json
                .as_deref()
                .map(|s| {
                    serde_json::from_str(s).map_err(|_| {
                        error(ErrorCode::InternalError, "stored operation error invalid")
                    })
                })
                .transpose()?,
        };
        let canonical = canonical_request(request, agent_core::redact::redact);
        match idempotency(
            Some(&op),
            op.kind,
            &canonical,
            expected.map(AuthorityEpoch),
            epoch,
        )
        .map_err(|c| error(c, "operation idempotency check failed"))?
        {
            IdempotencyDecision::Replay(_) => replay_value(replay(
                &op,
                self.state.borrow().dispatching.contains(&op.id),
            ))
            .map(Some),
            IdempotencyDecision::Conflict(_) => Err(error(
                ErrorCode::IdempotencyConflict,
                "idempotency key already names another request",
            )),
            IdempotencyDecision::StaleEpoch(_) => Err(error(
                ErrorCode::StaleEpoch,
                "operation belongs to an older authority epoch",
            )),
            IdempotencyDecision::New => Ok(None),
        }
    }
    pub(super) async fn cancel_worker_call(
        self: &Rc<Self>,
        caller: &WorkerId,
        r: CancelRequest,
    ) -> Result<Value, ToolError> {
        if let Some(v) = self
            .existing(caller, &ToolRequest::Cancel(r.clone()))
            .await?
        {
            return Ok(v);
        }
        let epoch = self.epoch(caller)?;
        if !self.visible(caller, &r.worker_id, false) {
            return Err(error(
                ErrorCode::AuthorityDenied,
                "a worker can cancel only its descendants",
            ));
        }
        let (root, bound) = {
            let state = self.state.borrow();
            let root = state.workers[caller].root.clone();
            let bound = state
                .workers
                .iter()
                .filter(|(id, _)| self.visible(&r.worker_id, id, true))
                .filter_map(|(_, w)| w.run.clone())
                .collect::<Vec<_>>();
            (root, bound)
        };
        let a = self.accept_request(
            caller,
            &root,
            ToolRequest::Cancel(r.clone()),
            Resolution {
                target: Some(r.worker_id.clone()),
                bound_runs: bound.clone(),
                base_commit: None,
            },
            AcceptPlan::Cancel,
            Vec::new(),
        );
        let (a, _) = self.accept(a, !bound.is_empty()).await?;
        let a = match a {
            Accepted::Replay(r) => return replay_value(r),
            Accepted::New(a) => a,
        };
        info!(
            caller = %caller,
            worker = %r.worker_id,
            runs = bound.len(),
            "delegated cancel accepted"
        );
        if bound.is_empty() {
            return Ok(a.receipt);
        }
        self.dispatch_check(caller, &r.worker_id, epoch, None)?;
        let workers = {
            let state = self.state.borrow();
            state
                .workers
                .iter()
                .filter_map(|(id, w)| {
                    w.run
                        .as_ref()
                        .filter(|run| bound.contains(run))
                        .map(|run| (id.clone(), run.clone()))
                })
                .collect::<Vec<_>>()
        };
        for (worker, run) in workers {
            self.cancel_bound_run(&worker, &run, "Delegated run cancelled");
        }
        self.record_fact(a.operation.clone(), DispatchFact::Submitted);
        let sup = self.clone();
        let op = a.operation.clone();
        gtk4::glib::spawn_future_local(async move {
            loop {
                let still_live = {
                    let state = sup.state.borrow();
                    state
                        .workers
                        .values()
                        .any(|w| w.run.as_ref().is_some_and(|r| bound.contains(r)))
                };
                if !still_live {
                    sup.record_fact(op, DispatchFact::Confirmed);
                    return;
                }
                if sup.state.borrow().shutting_down {
                    return;
                }
                glib::timeout_future(Duration::from_millis(20)).await;
            }
        });
        Ok(a.receipt)
    }
    pub(super) fn cancel_bound_run(
        self: &Rc<Self>,
        worker: &WorkerId,
        run: &agent_core::delegation::RunId,
        reason: &str,
    ) {
        let data = {
            let mut state = self.state.borrow_mut();
            let Some(w) = state
                .workers
                .get_mut(worker)
                .filter(|w| w.run.as_ref() == Some(run))
            else {
                return;
            };
            w.row.state = "stopping".into();
            (w.session(), w.generation, w.pending.take())
        };
        let pending = data.2.is_some();
        if let Some(input) = data.2 {
            self.state.borrow_mut().dispatching.remove(&input.operation);
            self.record_fact(
                input.operation,
                DispatchFact::Failed(error(ErrorCode::StaleRun, "run cancelled before dispatch")),
            );
            self.complete_run(worker, agent_core::event::TurnState::Interrupted, None);
        }
        if let Some(session) = data.0 {
            session.expire_approvals(reason);
            session.interrupt_run();
            let sup = self.weak.clone();
            let worker = worker.clone();
            let run = run.clone();
            let generation = data.1;
            if pending {
                session.shutdown(true);
                return;
            }
            gtk4::glib::spawn_future_local(async move {
                glib::timeout_future(Duration::from_millis(500)).await;
                if let Some(sup) = sup.upgrade() {
                    let current = {
                        let state = sup.state.borrow();
                        state.workers.get(&worker).is_some_and(|w| {
                            w.generation == generation && w.run.as_ref() == Some(&run)
                        })
                    };
                    if current {
                        session.shutdown(true);
                    }
                }
            });
        }
    }
    pub(super) async fn close_worker_call(
        self: &Rc<Self>,
        caller: &WorkerId,
        r: CloseRequest,
    ) -> Result<Value, ToolError> {
        if let Some(v) = self
            .existing(caller, &ToolRequest::Close(r.clone()))
            .await?
        {
            return Ok(v);
        }
        self.epoch(caller)?;
        if !self.visible(caller, &r.worker_id, false) {
            return Err(error(
                ErrorCode::AuthorityDenied,
                "a worker can close only descendants",
            ));
        }
        let root = self.state.borrow().workers[caller].root.clone();
        let a = self.accept_request(
            caller,
            &root,
            ToolRequest::Close(r.clone()),
            Resolution {
                target: Some(r.worker_id.clone()),
                ..Default::default()
            },
            AcceptPlan::Close,
            Vec::new(),
        );
        let (a, _) = self.accept(a, true).await?;
        let a = match a {
            Accepted::Replay(r) => return replay_value(r),
            Accepted::New(a) => a,
        };
        info!(caller = %caller, worker = %r.worker_id, "delegated worker closed");
        self.revoke(&root, RevocationEvent::Close(r.worker_id.clone()))
            .await?;
        self.record_fact(a.operation, DispatchFact::Confirmed);
        let lease = self
            .state
            .borrow()
            .child_specs
            .get(&r.worker_id)
            .and_then(|s| s.lease.clone());
        if let Some(lease) = lease {
            let open = (self.services.tab_open_in)(&lease.worktree);
            let watches = self
                .state
                .borrow()
                .workers
                .get(&r.worker_id)
                .map(|w| {
                    w.generations
                        .values()
                        .map(|g| g.watch.clone())
                        .collect::<Vec<_>>()
                })
                .unwrap_or_default();
            glib::spawn_future_local(async move {
                for watch in watches {
                    super::capacity::wait_exit(&watch).await;
                }
                gio::spawn_blocking(move || super::workspace::cleanup_if_clean(&lease, open))
                    .await
                    .ok();
            });
        }
        Ok(a.receipt)
    }
    pub(crate) fn scope_is_current(&self, scope: &agent_core::delegation::scope::Scope) -> bool {
        let state = self.state.borrow();
        state.workers.get(&scope.worker).is_some_and(|w| {
            w.row.parent_id.is_some()
                && w.root == scope.root
                && w.generation == scope.generation
                && w.run.as_ref().or(w.last_run.as_ref()) == scope.run.as_ref()
        })
    }
    pub(crate) fn scope_belongs_to_thread(
        &self,
        scope: &agent_core::delegation::scope::Scope,
        thread: &str,
    ) -> bool {
        self.state.borrow().threads.get(thread) == Some(&scope.root)
    }
    pub(crate) fn checked_child_session(
        &self,
        scope: &agent_core::delegation::scope::Scope,
    ) -> Option<Rc<crate::chat::session::ChatSession>> {
        self.scope_is_current(scope)
            .then(|| self.child_session(&scope.worker))
            .flatten()
    }
    pub(crate) fn child_workspace(&self, worker: &WorkerId) -> Option<std::path::PathBuf> {
        self.state
            .borrow()
            .workers
            .get(worker)
            .filter(|w| w.row.parent_id.is_some())
            .map(|w| w.cwd.clone())
    }
    pub(crate) fn cancel_human(self: &Rc<Self>, scope: &agent_core::delegation::scope::Scope) {
        if !self.scope_is_current(scope) {
            return;
        }
        let expected_run_id = scope.run.clone();
        self.act_as_parent(&scope.worker, move |sup, caller, worker, key| async move {
            let r = CancelRequest {
                worker_id: worker,
                expected_run_id,
                expected_authority_epoch: None,
                idempotency_key: key,
            };
            sup.cancel_worker_call(&caller, r).await
        });
    }
    pub(crate) fn close_human(self: &Rc<Self>, scope: &agent_core::delegation::scope::Scope) {
        if !self.scope_is_current(scope) {
            return;
        }
        self.act_as_parent(&scope.worker, |sup, caller, worker, key| async move {
            let r = CloseRequest {
                worker_id: worker,
                expected_authority_epoch: None,
                idempotency_key: key,
            };
            sup.close_worker_call(&caller, r).await
        });
    }
    /// Runs `act` for the user, as `worker`'s parent, under a fresh `human-<uuid>` key, and
    /// reports a refusal. A worker with no parent (a root) is left alone.
    pub(super) fn act_as_parent<F, Fut>(self: &Rc<Self>, worker: &WorkerId, act: F)
    where
        F: FnOnce(Rc<Self>, WorkerId, WorkerId, agent_core::delegation::IdempotencyKey) -> Fut
            + 'static,
        Fut: std::future::Future<Output = Result<Value, ToolError>>,
    {
        let caller = {
            let state = self.state.borrow();
            state.workers.get(worker).and_then(|w| w.row.parent())
        };
        let Some(caller) = caller else {
            return;
        };
        let worker = worker.clone();
        let sup = self.clone();
        glib::spawn_future_local(async move {
            let Ok(key) = agent_core::delegation::IdempotencyKey::parse(&format!(
                "human-{}",
                glib::uuid_string_random()
            )) else {
                return;
            };
            if let Err(e) = act(sup.clone(), caller, worker, key).await {
                (sup.services.report)(&e.to_string());
            }
        });
    }
    pub(super) fn set_status(&self, worker: &WorkerId, status: &str, reason: &str) {
        if let Some(w) = self.state.borrow_mut().workers.get_mut(worker) {
            w.row.delegation_status = status.into();
            w.row.delegation_reason = Some(reason.into());
        }
        let worker = worker.to_string();
        let status = status.to_owned();
        let reason = reason.to_owned();
        self.services
            .write("delegation status", None, true, move |s| {
                s.set_delegation_worker_status(
                    &worker,
                    &status,
                    Some(&reason),
                    "[]",
                    super::supervisor::now(),
                )
            });
    }
    pub(super) async fn recheck_agy(self: &Rc<Self>, worker: &WorkerId) -> Result<(), ToolError> {
        let generation = self
            .state
            .borrow()
            .workers
            .get(worker)
            .map(|w| w.generation);
        let result = self.check_agy(worker).await;
        if let Err(e) = &result {
            let (current, live) = self
                .state
                .borrow()
                .workers
                .get(worker)
                .filter(|w| Some(w.generation) == generation)
                .map_or((false, false), |w| (true, !w.generations.is_empty()));
            // A child refused before its first launch is failed by the launch path, but its
            // status must still say why, never the `available` it was created with.
            if current {
                self.set_status(worker, "workspace_untrusted", &e.message);
            }
            if live {
                self.fail_worker(worker, e.clone());
            }
        }
        result
    }
    /// Refuses `worker` if the agy bridge that presented its capability no longer passes the
    /// process census. A capability not yet presented has no bridge to check.
    async fn bridge_census_unchanged(&self, worker: &WorkerId) -> Result<(), ToolError> {
        let peer_info = {
            let state = self.state.borrow();
            state
                .workers
                .get(worker)
                .and_then(|w| w.token.as_ref())
                .and_then(|t| t.pid.zip(t.consumed_by))
        };
        let Some((pid, peer)) = peer_info else {
            return Ok(());
        };
        let bridge = self.services.bridge_command.clone();
        let valid = gio::spawn_blocking(move || {
            use std::os::unix::fs::MetadataExt;
            let facts = super::ipc::peer::RealProc;
            std::fs::metadata(bridge).ok().is_some_and(|m| {
                agent_core::delegation::agy_guard::census_ok(
                    &super::ipc::peer::census(&facts, pid, (m.dev(), m.ino())),
                    peer,
                )
            })
        })
        .await
        .unwrap_or(false);
        if !valid {
            return Err(error(
                ErrorCode::WorkspaceUntrusted,
                "agy bridge process census changed",
            ));
        }
        Ok(())
    }
    async fn check_agy(&self, worker: &WorkerId) -> Result<(), ToolError> {
        if !self.profile_available(worker) {
            return Err(error(ErrorCode::ProviderUnavailable, "profile unavailable"));
        }
        let actual = {
            let state = self.state.borrow();
            state.workers.get(worker).and_then(|w| {
                w.session()
                    .and_then(|s| s.delegation_launch_facts())
                    .filter(|facts| facts.generation == w.generation)
            })
        };
        if actual.is_none()
            && self
                .state
                .borrow()
                .workers
                .get(worker)
                .is_some_and(|w| !w.generations.is_empty())
        {
            return Err(error(
                ErrorCode::WorkspaceUntrusted,
                "live agy launch facts are unavailable",
            ));
        }
        if let Some(actual) = actual {
            self.bridge_census_unchanged(worker).await?;
            return self.check_agy_launch_facts(actual).await;
        }
        let (profile, cwd) = {
            let state = self.state.borrow();
            let w = state
                .workers
                .get(worker)
                .ok_or_else(|| error(ErrorCode::NotFound, "unknown worker"))?;
            (w.row.profile_id.clone(), w.cwd.clone())
        };
        let profile = (self.services.profiles)()
            .into_iter()
            .find(|p| p.id == profile && opted_in(p))
            .ok_or_else(|| error(ErrorCode::ProviderUnavailable, "profile unavailable"))?;
        let resolved = (self.services.resolve)(&profile)
            .ok_or_else(|| error(ErrorCode::ProviderUnavailable, "agent unavailable"))?;
        let hookless = (self.services.hook_check)(crate::hook_config::HookTarget::new(
            &resolved.env,
            Some(cwd.clone()),
        ))
        .await
        .is_err();
        self.bridge_census_unchanged(worker).await?;
        let limits = self.limits;
        let bridge_command = self.services.bridge_command.clone();
        gio::spawn_blocking(move || {
            // The child's environment as it is spawned; a variable that is not UTF-8 is read
            // lossily rather than panicking, and such a HOME proves no home.
            let child = crate::agent_home::child_env(&resolved.env);
            let home = crate::agent_home::home_in_child(&child)
                .ok_or_else(|| error(ErrorCode::WorkspaceUntrusted, "agent home unavailable"))?;
            let env = child
                .into_iter()
                .collect::<std::collections::BTreeMap<_, _>>();
            managed_registration_ready(&home, &bridge_command)?;
            let input = super::agy_l2::L2Input {
                argv: std::iter::once(resolved.program)
                    .chain(resolved.extra_args)
                    .collect(),
                env: env.into_iter().collect(),
                physical_cwd: cwd,
                verified_home: home,
                hookless,
            };
            super::agy_l2::scan(&input, &limits).map_err(|_| {
                error(
                    ErrorCode::WorkspaceUntrusted,
                    "agy workspace MCP configuration is untrusted",
                )
            })
        })
        .await
        .map_err(|_| error(ErrorCode::WorkspaceUntrusted, "agy workspace scan failed"))?
    }
    pub(super) async fn check_agy_launch_facts(
        &self,
        facts: super::seam::DelegationLaunchFacts,
    ) -> Result<(), ToolError> {
        let bridge = self.services.bridge_command.clone();
        let limits = self.limits;
        gio::spawn_blocking(move || {
            let home = facts
                .effective_home
                .ok_or_else(|| error(ErrorCode::WorkspaceUntrusted, "agent home unavailable"))?;
            managed_registration_ready(&home, &bridge)?;
            let mut env = vec![("HOME".into(), home.to_string_lossy().into_owned())];
            env.extend(
                facts
                    .config_redirect_vars
                    .into_iter()
                    .map(|name| (name, "present".into())),
            );
            super::agy_l2::scan(
                &super::agy_l2::L2Input {
                    argv: facts.argv,
                    env,
                    physical_cwd: facts.physical_cwd,
                    verified_home: home,
                    hookless: !facts.hook_proven,
                },
                &limits,
            )
            .map_err(|_| {
                error(
                    ErrorCode::WorkspaceUntrusted,
                    "agy actual launch configuration is untrusted",
                )
            })
        })
        .await
        .map_err(|_| {
            error(
                ErrorCode::WorkspaceUntrusted,
                "agy actual launch inspection failed",
            )
        })?
    }
    async fn handle_call(self: &Rc<Self>, conn: ConnId, call: Call) -> Result<Value, ToolError> {
        let worker = self
            .state
            .borrow()
            .connections
            .get(&conn)
            .cloned()
            .ok_or_else(|| {
                error(
                    ErrorCode::AuthenticationFailed,
                    "connection unauthenticated",
                )
            })?;
        self.still_authorized(conn, &worker)?;
        if !self.profile_available(&worker) {
            return Err(error(
                ErrorCode::ProviderUnavailable,
                "profile disabled for delegation",
            ));
        }
        self.epoch(&worker)?;
        let req = parse_request(call.tool, &call.args, &self.limits)?.normalised();
        let driver = self.state.borrow().workers[&worker].driver;
        let args = attestation_arguments(call.tool, &call.args, &self.limits)?;
        let until = Instant::now() + Duration::from_millis(self.limits.attestation_wait_ms);
        let attested = loop {
            let matched = self
                .state
                .borrow_mut()
                .workers
                .get_mut(&worker)
                .is_some_and(|w| {
                    w.observed
                        .attest(call.invocation.as_ref(), call.tool, &args)
                        == Attestation::Attested
                });
            if matched {
                break true;
            }
            if call.invocation.is_none() || Instant::now() >= until {
                break false;
            }
            glib::timeout_future(Duration::from_millis(10)).await;
            self.epoch(&worker)?;
        };
        if driver == Driver::Agy {
            if !attested {
                return Err(error(
                    ErrorCode::AuthorityDenied,
                    "tool call was not observed in this principal's stream",
                )
                .with_detail("reason", "unattested"));
            }
            self.recheck_agy(&worker).await?;
        }
        // Attestation and the agy recheck awaited: the same checks again before dispatch.
        self.still_authorized(conn, &worker)?;
        match req {
            ToolRequest::Capabilities(r) => self.capabilities(conn, &worker, r).await,
            ToolRequest::List(r) => self.list_workers(&worker, r).await,
            ToolRequest::Read(r) => self.read_page(&worker, r).await,
            ToolRequest::Wait(r) => self.wait_page(conn, call.id, &worker, r).await,
            ToolRequest::Spawn(r) => self.spawn_worker(&worker, r).await,
            ToolRequest::Send(r) => self.send_worker(&worker, r).await,
            ToolRequest::Cancel(r) => self.cancel_worker_call(&worker, r).await,
            ToolRequest::Close(r) => self.close_worker_call(&worker, r).await,
        }
    }
    /// Whether an agy profile's managed MCP registration would let a child launch: the same
    /// check `check_agy` makes before every agy spawn, run off the main thread.
    async fn agy_registration(&self, profile: &str) -> Result<(), ToolError> {
        let profile = (self.services.profiles)()
            .into_iter()
            .find(|p| p.id == profile)
            .ok_or_else(|| error(ErrorCode::ProviderUnavailable, "profile unavailable"))?;
        let resolved = (self.services.resolve)(&profile)
            .ok_or_else(|| error(ErrorCode::ProviderUnavailable, "agent unavailable"))?;
        let bridge = self.services.bridge_command.clone();
        gio::spawn_blocking(move || {
            managed_registration_ready(&agent_home(&resolved.env)?, &bridge)
        })
        .await
        .map_err(|_| {
            error(
                ErrorCode::WorkspaceUntrusted,
                "agy registration check failed",
            )
        })?
    }
    /// Why an opted-in Antigravity root could not be bound now (its managed registration is
    /// missing or unsafe), or `None` when it can, or when the profile is not an opted-in agy one.
    /// Such a root starts unbound, like an opted-out one, rather than not at all.
    pub(crate) async fn agy_root_problem(
        &self,
        profile: &crate::config::Profile,
        driver: Driver,
    ) -> Option<String> {
        if driver != Driver::Agy || !opted_in(profile) {
            return None;
        }
        self.agy_registration(&profile.id)
            .await
            .err()
            .map(|e| e.message)
    }
    /// The caller checks `handle_call` makes before dispatch, and again after any `await`: the
    /// connection still holds the worker's live capability, the worker's root epoch is current,
    /// and a child's credential belongs to that epoch. Returns the current epoch.
    pub(super) fn still_authorized(
        &self,
        conn: ConnId,
        worker: &WorkerId,
    ) -> Result<AuthorityEpoch, ToolError> {
        let live = self
            .state
            .borrow()
            .workers
            .get(worker)
            .and_then(|w| w.token.as_ref())
            .is_some_and(|t| t.live_conn == Some(conn));
        if !live {
            return Err(error(
                ErrorCode::AuthenticationFailed,
                "connection capability was revoked",
            ));
        }
        let epoch = self.epoch(worker)?;
        let state = self.state.borrow();
        let stale = state
            .workers
            .get(worker)
            .is_none_or(|w| w.row.parent_id.is_some() && !state.token_current(w));
        if stale {
            return Err(error(
                ErrorCode::StaleEpoch,
                "child credential belongs to an older authority epoch",
            ));
        }
        Ok(epoch)
    }
    async fn capabilities(
        &self,
        conn: ConnId,
        worker: &WorkerId,
        request: CapabilitiesRequest,
    ) -> Result<Value, ToolError> {
        self.epoch(worker)?;
        if request.refresh {
            // At most once a minute per agent (the status's own gate); the wait never blocks the
            // main loop, and a probe still running when it ends is left to finish.
            let wait = Duration::from_millis(u64::from(self.limits.wait_timeout_max_ms))
                .min(USAGE_REFRESH_WAIT);
            (self.services.catalog.refresh_usage)(wait).await;
        }
        let tags = (self.services.catalog.tags)().await;
        // The world may have moved during those awaits: answer only a caller still authorized.
        self.still_authorized(conn, worker)?;
        let now = crate::freshness::now_epoch();
        let caller = {
            let state = self.state.borrow();
            let w = &state.workers[worker];
            Principal {
                worker_id: worker.clone(),
                root_id: w.root.clone(),
                driver: w.driver,
                depth: w.row.depth as u8,
                can_spawn: w.row.depth < self.limits.max_depth as i64,
                task_mode_ceiling: ceiling(self.principal_mode(worker)).to_vec(),
            }
        };
        let profiles = (self.services.profiles)()
            .into_iter()
            .filter(|p| {
                request
                    .profile_id
                    .as_ref()
                    .is_none_or(|id| id.as_str() == p.id)
            })
            .filter_map(|p| {
                let driver = crate::config::profile_driver(&p)?;
                let id = agent_core::delegation::ProfileId::parse(&p.id).ok()?;
                let available = opted_in(&p) && (self.services.resolve)(&p).is_some();
                Some(ProfileCapabilities {
                    profile_id: id,
                    name: p.name,
                    driver,
                    available,
                    reason: (!available)
                        .then(|| "profile is disabled or unavailable for delegation".into()),
                    modes: TaskMode::ALL
                        .into_iter()
                        .map(|task_mode| {
                            let route = self.routes.admit(&RouteQuery {
                                parent: caller.driver,
                                child: driver,
                                task_mode,
                                child_bridge: ChildBridge::WithBridge,
                            });
                            ModeAdmission {
                                task_mode,
                                route_status: route.status,
                                route_blockers: route
                                    .blockers
                                    .into_iter()
                                    .map(|b| b.as_str().into())
                                    .collect(),
                                route_caveats: agent_core::delegation::route::route_caveats(driver)
                                    .iter()
                                    .map(|c| c.as_str().into())
                                    .collect(),
                                child_bridge: ChildBridge::WithBridge,
                            }
                        })
                        .collect(),
                    models: (self.services.catalog.models)(driver)
                        .iter()
                        .filter_map(|m| super::catalog_info::model_info(m, tags.tags.as_deref()))
                        .collect(),
                    usage: (self.services.catalog.usage)(driver)
                        .map(|s| super::catalog_info::usage_info(&s, now)),
                    workspaces: vec![
                        WorkspaceKind::SharedReadOnly,
                        WorkspaceKind::IsolatedWorktree,
                    ],
                    delivery: DeliverySupport {
                        this_run: DelegationCaps::of(driver).this_run,
                        next_turn: true,
                    },
                    controls: ProfileControls {
                        cancel: true,
                        close: true,
                    },
                })
            })
            .collect::<Vec<_>>();
        // agy refuses every launch without its managed registration: say so here rather than
        // accept a spawn that can only fail.
        let mut profiles = profiles;
        for p in profiles
            .iter_mut()
            .filter(|p| p.available && p.driver == Driver::Agy)
        {
            if let Err(e) = self.agy_registration(p.profile_id.as_str()).await {
                p.available = false;
                p.reason = Some(e.message);
            }
        }
        // The last await is behind us: check again, and report the epoch as it is now.
        let epoch = self.still_authorized(conn, worker)?;
        // The caller's own profile may have been switched off for delegation meanwhile.
        if !self.profile_available(worker) {
            return Err(error(
                ErrorCode::ProviderUnavailable,
                "profile disabled for delegation",
            ));
        }
        // The schema's bound on models per profile; what goes is counted like truncation's cuts.
        let mut omitted = Omitted::default();
        for p in &mut profiles {
            let over = p
                .models
                .len()
                .saturating_sub(super::catalog_info::MAX_MODELS);
            p.models.truncate(super::catalog_info::MAX_MODELS);
            omitted.models += u32::try_from(over).unwrap_or(u32::MAX);
        }
        let mut response = CapabilitiesResponse {
            api_version: 1,
            caller,
            authority_epoch: epoch.0,
            delegation_scope: DELEGATION_SCOPE_SUPERVISED.into(),
            profiles,
            limits: CapabilityLimits::from_limits(&self.limits),
            truncated: omitted.models > 0,
            omitted,
            tag_vocabulary: tags
                .tags
                .as_ref()
                .map(|t| t.vocabulary.clone())
                .unwrap_or_default(),
            tags_status: tags.status,
            effort_policy: tags.tags.as_ref().and_then(|t| t.effort_policy.clone()),
        };
        truncate_capabilities(&mut response);
        serde_json::to_value(response)
            .map_err(|_| error(ErrorCode::InternalError, "cannot encode capabilities"))
    }
    pub(super) fn visible(&self, caller: &WorkerId, target: &WorkerId, include_self: bool) -> bool {
        let state = self.state.borrow();
        let Some(from) = state.workers.get(caller) else {
            return false;
        };
        let Some(to) = state.workers.get(target) else {
            return false;
        };
        if from.root != to.root {
            return false;
        }
        let mut current = Some(target.clone());
        while let Some(w) = current {
            if &w == caller {
                return include_self || caller != target;
            }
            current = state.workers.get(&w).and_then(|w| w.row.parent());
        }
        false
    }
    /// `target` names the subtree root, which is never listed itself: a worker with no
    /// children lists `[]`. Each worker's run is the store's active run, the same one `cancel`
    /// checks, so a listed `run_id` is one `cancel(expected_run_id)` accepts (barring a real race
    /// with the run's end, which `cancel` reports as `stale_run`).
    async fn list_workers(&self, caller: &WorkerId, r: ListRequest) -> Result<Value, ToolError> {
        let target = r.target.clone().unwrap_or_else(|| caller.clone());
        if !self.visible(caller, &target, true) {
            return Err(error(ErrorCode::NotFound, "unknown worker"));
        }
        let filter = PageFilter {
            caller: caller.clone(),
            target: r.target,
            workers: Vec::new(),
            mailbox: false,
        };
        let (offset, ids, root_id, stream_epoch) = {
            let state = self.state.borrow();
            let w = &state.workers[caller];
            let stream_epoch = state.roots[&w.root].row.stream_epoch.max(0) as u64;
            let offset = match &r.cursor {
                Some(c) => {
                    self.check_cursor(caller, "cursor", c, &filter, stream_epoch)?
                        .last_seq as usize
                }
                None => 0,
            };
            let mut ids = state
                .workers
                .keys()
                .filter(|id| *id != &target && self.visible(&target, id, false))
                .cloned()
                .collect::<Vec<_>>();
            ids.sort();
            (offset, ids, w.root.clone(), stream_epoch)
        };
        // A page holds at most `limit` workers: look up only those, plus one so the page knows
        // whether more remain (`truncated`).
        let limit = r.limit.unwrap_or(50);
        let ids = ids
            .into_iter()
            .skip(offset)
            .take(limit as usize + 1)
            .collect::<Vec<_>>();
        let lookup = ids.clone();
        let runs = self
            .services
            .read(move |s| {
                lookup
                    .iter()
                    .map(|id| s.active_delegation_run(id.as_str()))
                    .collect::<Result<Vec<_>, _>>()
            })
            .await
            .ok_or_else(|| error(ErrorCode::PersistenceFailed, "cannot read delegated runs"))?
            .map_err(|_| error(ErrorCode::PersistenceFailed, "cannot read delegated runs"))?;
        let values = {
            let state = self.state.borrow();
            ids.iter()
                .zip(runs)
                .filter_map(|(id, run)| {
                    let mut summary = self.summary(id, state.workers.get(id)?)?;
                    let run = run.and_then(|row| {
                        Some((
                            agent_core::delegation::RunId::parse(&row.id).ok()?,
                            parse_lenient(&row.state)?,
                        ))
                    });
                    summary.controls.cancel = run.is_some();
                    summary.run_state = run.as_ref().map(|(_, s)| *s);
                    summary.run_id = run.map(|(id, _)| id);
                    Some(summary)
                })
                .filter_map(|s| serde_json::to_value(s).ok())
                .collect::<Vec<_>>()
        };
        let render = |workers: &[&Value], truncated: bool| json!({"api_version":1,"workers":workers,"next_cursor":if truncated{Some(encode_cursor(&CursorPayload{root:root_id.clone(),stream_epoch,last_seq:(offset+workers.len())as u64,filter_digest:filter_digest(&filter)}))}else{None},"truncated":truncated});
        let envelope = agent_core::delegation::page::Reservation::max_envelope(&render(&[], true));
        let (page, _) = agent_core::delegation::page::assemble_list(
            envelope,
            &values,
            limit,
            self.limits.output_budget as usize,
            render,
        )?;
        Ok(page)
    }
    async fn page(
        &self,
        caller: &WorkerId,
        filter: PageFilter,
        after: Option<agent_core::delegation::Cursor>,
        events: Option<u32>,
        bytes: Option<u32>,
        ack: Option<Value>,
    ) -> Result<Value, ToolError> {
        self.check_page(caller, &filter, after.as_ref())?;
        let target = filter.target.as_ref().unwrap_or(caller);
        let (root, scope, snapshot) = {
            let state = self.state.borrow();
            let root = state.workers[caller].root.clone();
            let scope = state
                .workers
                .keys()
                .filter(|id| {
                    if filter.workers.is_empty() {
                        self.visible(target, id, true)
                    } else {
                        filter.workers.iter().any(|w| self.visible(w, id, true))
                    }
                })
                .cloned()
                .collect::<Vec<_>>();
            let snapshot = filter
                .target
                .as_ref()
                .and_then(|id| self.summary(id, &state.workers[id]))
                .and_then(|s| serde_json::to_value(s).ok());
            (root, scope, snapshot)
        };
        let shown_scope = scope.clone();
        let query = PageQuery {
            root_worker: root,
            filter,
            scope,
            after,
            max_events: events.unwrap_or(100),
            max_bytes: bytes.unwrap_or(65536),
            snapshot,
            ack,
            budget: self.limits.output_budget as usize,
        };
        let out = self
            .services
            .read(move |s| s.read_delegation_page(&query))
            .await
            .ok_or_else(|| error(ErrorCode::PersistenceFailed, "cannot read delegation page"))?
            .map_err(|_| error(ErrorCode::PersistenceFailed, "cannot read delegation page"))??;
        // What this page shows its caller is never woken for again.
        self.wake_shown(caller, &shown_scope, out.last_seq, &out.presented);
        let ids = out.presented;
        let principal = caller.clone();
        self.services
            .durable("present delegation messages", move |s| {
                s.mark_delegation_presented(&principal, &ids)
            })
            .await?;
        Ok(out.page)
    }
    /// What a page request can be refused for before anything is read or written: a target or
    /// waited worker the caller cannot see, or a cursor issued for another filter or root.
    /// `read` checks it before applying its acknowledgement, so a refused request records no
    /// idempotency key and a corrected retry with the same key is not a conflict.
    fn check_page(
        &self,
        caller: &WorkerId,
        filter: &PageFilter,
        after: Option<&agent_core::delegation::Cursor>,
    ) -> Result<(), ToolError> {
        let target = filter.target.as_ref().unwrap_or(caller);
        if !self.visible(caller, target, true)
            || filter
                .workers
                .iter()
                .any(|w| !self.visible(caller, w, true))
        {
            return Err(error(ErrorCode::NotFound, "unknown worker"));
        }
        let Some(after) = after else {
            return Ok(());
        };
        let stream_epoch = {
            let state = self.state.borrow();
            state.roots[&state.workers[caller].root]
                .row
                .stream_epoch
                .max(0) as u64
        };
        // Expiry is the store's to decide: only a cursor that can never fit this call is refused.
        match self.check_cursor(caller, "after", after, filter, stream_epoch) {
            Err(e) if e.code == ErrorCode::InvalidArgument => Err(e),
            _ => Ok(()),
        }
    }
    /// Decodes a cursor for `filter`. One issued for another filter is refused naming that
    /// filter when it is one this caller could have used (no target or one target, no waited
    /// workers, one, all its children or all its descendants, either mailbox setting), so the
    /// caller can repeat it; otherwise the error says it matches none of them.
    pub(super) fn check_cursor(
        &self,
        caller: &WorkerId,
        field: &str,
        cursor: &agent_core::delegation::Cursor,
        filter: &PageFilter,
        stream_epoch: u64,
    ) -> Result<CursorPayload, ToolError> {
        let root = self.state.borrow().workers[caller].root.clone();
        match decode_cursor(cursor, filter, stream_epoch) {
            Ok(p) if p.root == root => return Ok(p),
            Ok(_) => return Err(ToolError::invalid(field, "root")),
            Err(ErrorCode::CursorExpired) => {
                return Err(error(
                    ErrorCode::CursorExpired,
                    "the stream was reset; read again with no cursor",
                ))
            }
            Err(_) => {}
        }
        let issued = self.cursor_candidates(caller).into_iter().find(|f| {
            matches!(
                decode_cursor(cursor, f, stream_epoch),
                Ok(_) | Err(ErrorCode::CursorExpired)
            )
        });
        let refused = match &issued {
            Some(f) => ToolError::new(
                ErrorCode::InvalidArgument,
                format!(
                    "invalid argument `{field}`: the cursor was issued for another filter ({}), \
                     not this call's ({}); repeat that filter, or call again without `{field}`",
                    describe_filter(f),
                    describe_filter(filter)
                ),
            )
            .with_detail("issued_for", filter_json(f)),
            None => ToolError::new(
                ErrorCode::InvalidArgument,
                format!(
                    "invalid argument `{field}`: the cursor is malformed or was issued for a \
                     filter other than this call's ({}), for example a wait over several \
                     chosen workers; repeat that exact filter, or call again without `{field}`",
                    describe_filter(filter)
                ),
            ),
        };
        Err(refused
            .with_detail("field", field)
            .with_detail("reason", "filter")
            .with_detail("expected", filter_json(filter)))
    }
    /// The filters this caller could have been issued a cursor for, as far as they can be
    /// enumerated (a wait over an arbitrary set of workers cannot).
    fn cursor_candidates(&self, caller: &WorkerId) -> Vec<PageFilter> {
        let state = self.state.borrow();
        let mut seen = state
            .workers
            .keys()
            .filter(|id| self.visible(caller, id, true))
            .cloned()
            .collect::<Vec<_>>();
        seen.sort();
        let children = seen
            .iter()
            .filter(|id| state.workers[*id].row.parent_id.as_deref() == Some(caller.as_str()))
            .cloned()
            .collect::<Vec<_>>();
        let descendants = seen
            .iter()
            .filter(|id| *id != caller)
            .cloned()
            .collect::<Vec<_>>();
        let filter = |target: Option<&WorkerId>, workers: Vec<WorkerId>, mailbox| PageFilter {
            caller: caller.clone(),
            target: target.cloned(),
            workers,
            mailbox,
        };
        let mut out = Vec::new();
        for mailbox in [true, false] {
            out.push(filter(None, Vec::new(), mailbox));
            out.push(filter(None, children.clone(), mailbox));
            out.push(filter(None, descendants.clone(), mailbox));
            for id in &seen {
                out.push(filter(Some(id), Vec::new(), mailbox));
                out.push(filter(None, vec![id.clone()], mailbox));
            }
        }
        out
    }
    async fn read_page(&self, caller: &WorkerId, r: ReadRequest) -> Result<Value, ToolError> {
        let filter = PageFilter {
            caller: caller.clone(),
            target: r.target.clone(),
            workers: Vec::new(),
            mailbox: true,
        };
        self.check_page(caller, &filter, r.after.as_ref())?;
        let ack = if r.acknowledge.is_some() {
            let a = AckRequest {
                principal: caller.clone(),
                request: ToolRequest::Read(r.clone()),
            };
            Some(
                match self
                    .services
                    .durable("acknowledge delegation messages", move |s| {
                        s.acknowledge_delegation_messages(&a)
                    })
                    .await??
                {
                    AckResult::New(v) => v,
                    AckResult::Replay(r) => replay_value(r)?,
                },
            )
        } else {
            None
        };
        self.page(caller, filter, r.after, r.max_events, r.max_bytes, ack)
            .await
    }
    async fn wait_page(
        &self,
        conn: ConnId,
        id: u64,
        caller: &WorkerId,
        r: WaitRequest,
    ) -> Result<Value, ToolError> {
        let root = self.state.borrow().workers[caller].root.clone();
        let mut usage = self.usage(&root);
        usage.waits_principal = self
            .state
            .borrow()
            .waits
            .values()
            .filter(|w| *w == caller)
            .count() as u32;
        agent_core::delegation::limits::admit_wait(&self.limits, &usage)?;
        self.state
            .borrow_mut()
            .waits
            .insert((conn, id), caller.clone());
        let until = Instant::now() + Duration::from_millis(r.timeout_ms.unwrap_or(10000) as u64);
        loop {
            let filter = PageFilter {
                caller: caller.clone(),
                target: None,
                workers: r.workers.clone().unwrap_or_default(),
                mailbox: r.mailbox.unwrap_or(true),
            };
            let result = self
                .page(
                    caller,
                    filter,
                    r.after.clone(),
                    r.max_events,
                    r.max_bytes,
                    None,
                )
                .await;
            let mut page = match result {
                Ok(p) => p,
                Err(e) => {
                    self.state.borrow_mut().waits.remove(&(conn, id));
                    return Err(e);
                }
            };
            let available = page["events"].as_array().is_some_and(|a| !a.is_empty())
                || page["messages"].as_array().is_some_and(|a| !a.is_empty());
            let expired = Instant::now() >= until;
            let cancelled = !self.state.borrow().waits.contains_key(&(conn, id));
            if available || expired || cancelled {
                self.state.borrow_mut().waits.remove(&(conn, id));
                page["timed_out"] = json!(expired && !available);
                return Ok(page);
            }
            glib::timeout_future(Duration::from_millis(25)).await;
        }
    }
    /// `request` from `caller`, ready for [`Self::accept`]: the limits, the live processes of
    /// `root` and the operations being dispatched are read now.
    pub(super) fn accept_request(
        &self,
        caller: &WorkerId,
        root: &WorkerId,
        request: ToolRequest,
        resolution: Resolution,
        plan: AcceptPlan,
        warnings: Vec<Warning>,
    ) -> Accept {
        Accept {
            principal: caller.clone(),
            request,
            resolution,
            plan,
            warnings,
            limits: self.limits,
            live_processes: self.usage(root).live_processes,
            dispatching: self.state.borrow().dispatching.clone(),
        }
    }
    pub(super) async fn accept(
        &self,
        a: Accept,
        reserve: bool,
    ) -> Result<(Accepted, Option<Reserved>), ToolError> {
        self.services
            .durable("accept delegated operation", move |s| {
                s.atomically(|s| {
                    let accepted = s.accept_delegation(&a)?;
                    let accepted = match accepted {
                        Ok(a) => a,
                        Err(e) => return Ok(Err(e)),
                    };
                    let reserved = if reserve {
                        if let Accepted::New(op) = &accepted {
                            Some(match s.reserve_delegation_dispatch(&op.operation)? {
                                Ok(r) => r,
                                Err(e) => return Ok(Err(e)),
                            })
                        } else {
                            None
                        }
                    } else {
                        None
                    };
                    Ok(Ok((accepted, reserved)))
                })
            })
            .await?
    }
    pub(super) fn dispatch_check(
        &self,
        from: &WorkerId,
        target: &WorkerId,
        epoch: AuthorityEpoch,
        expected: Option<&agent_core::delegation::RunId>,
    ) -> Result<(), ToolError> {
        if !self.profile_available(from) || !self.profile_available(target) {
            return Err(error(
                ErrorCode::ProviderUnavailable,
                "profile disabled before dispatch",
            ));
        }
        if self.epoch(from)? != epoch {
            return Err(error(
                ErrorCode::StaleEpoch,
                "authority changed before dispatch",
            ));
        }
        let state = self.state.borrow();
        let w = state
            .workers
            .get(target)
            .ok_or_else(|| error(ErrorCode::NotFound, "unknown worker"))?;
        if w.row.state == "closed" {
            return Err(error(ErrorCode::WorkerClosed, "worker closed"));
        }
        if w.row.state == "stopping" {
            return Err(error(ErrorCode::StaleRun, "target run is cancelled"));
        }
        if w.row.delegation_status == "restricted" {
            return Err(error(ErrorCode::AuthorityDenied, "worker restricted"));
        }
        if let Some(expected) = expected {
            if w.run.as_ref() != Some(expected) {
                return Err(error(ErrorCode::StaleRun, "target run ended"));
            }
        }
        if w.row.parent_id.as_deref() == Some(from.as_str()) {
            let mode = w.row.task_mode_kind();
            if mode.is_none_or(|m| !permits(self.principal_mode(from), m)) {
                return Err(error(
                    ErrorCode::AuthorityDenied,
                    "child mode exceeds sender authority",
                ));
            }
        }
        Ok(())
    }
    pub(super) fn record_fact(&self, op: OperationId, fact: DispatchFact) {
        self.services
            .write("record delegated dispatch", None, true, move |s| {
                let target = match &fact {
                    DispatchFact::Submitted => "submitted",
                    DispatchFact::Confirmed => "confirmed",
                    DispatchFact::Unknown => "unknown",
                    DispatchFact::Failed(_) => "failed",
                };
                if s.delegation_operation(op.as_str())?
                    .is_some_and(|o| o.dispatch_state == target)
                {
                    return Ok(());
                }
                s.record_delegation_dispatch(&op, &fact)?;
                Ok(())
            });
    }
}
/// A page filter as the caller wrote it: `target`, `workers` and `mailbox`.
fn describe_filter(f: &PageFilter) -> String {
    let mut parts = Vec::new();
    if let Some(t) = &f.target {
        parts.push(format!("target {t}"));
    }
    if !f.workers.is_empty() {
        let workers = f.workers.iter().map(WorkerId::as_str).collect::<Vec<_>>();
        parts.push(format!("workers [{}]", workers.join(", ")));
    }
    parts.push(format!("mailbox {}", f.mailbox));
    parts.join(", ")
}
fn filter_json(f: &PageFilter) -> Value {
    json!({"target": f.target, "workers": f.workers, "mailbox": f.mailbox})
}
/// The home an agent launched with `env` sees, as its approval socket proves it
/// ([`crate::agent_home::effective_home`]). A home it cannot prove (none, relative, with `..`)
/// refuses the check.
fn agent_home(env: &crate::agent_proc::AgentEnv) -> Result<std::path::PathBuf, ToolError> {
    crate::agent_home::effective_home(env)
        .ok_or_else(|| error(ErrorCode::WorkspaceUntrusted, "agent home unavailable"))
}
pub(super) fn managed_registration_ready(
    home: &std::path::Path,
    command: &std::path::Path,
) -> Result<(), ToolError> {
    let state = home.join(".config/agent-terminal/delegation");
    let plan = agent_kit::delegation_config::preview(
        home,
        &state,
        command,
        agent_kit::delegation_config::Action::Configure,
    )
    // `preview`'s messages name a condition, never file contents: safe to pass on.
    .map_err(|e| {
        error(
            ErrorCode::WorkspaceUntrusted,
            &format!("agy managed MCP registration is unusable: {e}"),
        )
    })?;
    if plan.changed() {
        return Err(error(
            ErrorCode::WorkspaceUntrusted,
            "agy managed MCP registration and exact allow rules require configuration",
        ));
    }
    Ok(())
}
pub(super) fn replay_value(r: ReplayOutcome) -> Result<Value, ToolError> {
    match r {
        ReplayOutcome::Receipt(v) => Ok(v),
        ReplayOutcome::Error(e) | ReplayOutcome::DeliveryUnknown(e) => Err(e),
        ReplayOutcome::WaitForDispatch => Err(error(
            ErrorCode::CapacityReached,
            "operation dispatch is in progress; retry the same key",
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent_proc::AgentEnv;

    #[test]
    fn the_registration_home_is_the_one_the_approval_socket_proves() {
        let home = |value: &str| AgentEnv {
            env: vec![("HOME".into(), value.into())],
            unset: vec![],
        };
        assert_eq!(
            agent_home(&home("/profile/home")).ok(),
            Some("/profile/home".into())
        );
        for value in ["/app/../profile", "relative", ""] {
            assert!(agent_home(&home(value)).is_err(), "{value}");
        }
        let unset = AgentEnv {
            env: vec![],
            unset: vec!["HOME".into()],
        };
        assert!(agent_home(&unset).is_err());
    }
}
