//! Hooks hold only a weak supervisor; no session/runtime ownership cycle.
use super::seam::{DelegationHooks, LaunchBinding, TurnBoundary};
use super::supervisor::{GenRt, Supervisor, TokenSlot};
use crate::agent_proc::{ExitWatch, StopPhase};
use agent_core::adapter::{Driver, Mode};
use agent_core::delegation::authority::{ceiling, RevocationEvent};
use agent_core::delegation::correlation::{attestation_arguments, InvocationKey, ObservedCalls};
use agent_core::delegation::{ProcessGeneration, WorkerId};
use agent_core::event::{Envelope, Event};
use gtk4::{gio, glib};
use serde_json::Value;
use std::rc::Weak;
use std::time::Instant;

pub(super) struct Hooks {
    pub supervisor: Weak<Supervisor>,
    pub worker: WorkerId,
}
pub(super) fn sanitise_identity(identity: &mut agent_core::delegation::identity::Identity) {
    for (value, max) in [
        (&mut identity.model, 128usize),
        (&mut identity.effort, 32usize),
    ] {
        for text in [&mut value.requested, &mut value.effective]
            .into_iter()
            .flatten()
        {
            *text = agent_core::redact::redact(text);
            let mut end = max.min(text.len());
            while !text.is_char_boundary(end) {
                end -= 1;
            }
            text.truncate(end);
        }
    }
}
impl DelegationHooks for Hooks {
    fn before_spawn(
        &self,
        facts: super::seam::DelegationLaunchFacts,
    ) -> super::seam::LocalFuture<Result<(), String>> {
        let weak = self.supervisor.clone();
        let worker = self.worker.clone();
        Box::pin(async move {
            let sup = weak
                .upgrade()
                .ok_or_else(|| "delegation runtime stopped".to_owned())?;
            let (driver, unbound_root) = {
                let state = sup.state.borrow();
                let w = state
                    .workers
                    .get(&worker)
                    .ok_or_else(|| "delegation principal disappeared".to_owned())?;
                (w.driver, w.row.parent_id.is_none() && w.token.is_none())
            };
            if driver != Driver::Agy {
                return Ok(());
            }
            // Disabling delegation does not disable the user's ordinary root session.
            // There is no bound capability; children and opted-in roots still fail closed.
            if unbound_root && !sup.profile_available(&worker) {
                return Ok(());
            }
            let generation = facts.generation;
            let epoch = sup.epoch(&worker).map_err(|e| e.message)?;
            let valid = |sup: &Supervisor| {
                let state = sup.state.borrow();
                state
                    .workers
                    .get(&worker)
                    .and_then(|w| w.token.as_ref())
                    .is_some_and(|t| {
                        t.generation == generation && t.epoch == epoch && t.pid.is_none()
                    })
            };
            if !valid(&sup) {
                return Err("delegation launch authority changed".into());
            }
            if let Err(e) = sup.check_agy_launch_facts(facts).await {
                sup.set_status(&worker, "workspace_untrusted", &e.message);
                sup.fail_worker(&worker, e.clone());
                return Err(e.message);
            }
            if sup.epoch(&worker).ok() != Some(epoch) || !valid(&sup) {
                return Err("delegation launch authority changed".into());
            }
            Ok(())
        })
    }
    fn before_root_prompt(&self) -> bool {
        let Some(sup) = self.supervisor.upgrade() else {
            return false;
        };
        let thread = {
            let state = sup.state.borrow();
            let Some(w) = state.workers.get(&self.worker) else {
                return false;
            };
            if w.row.parent_id.is_some() {
                return true;
            }
            state
                .roots
                .get(&w.root)
                .map(|r| r.row.root_thread_id.clone())
        };
        if !sup.profile_available(&self.worker) {
            return true;
        }
        thread.is_some_and(|thread| {
            sup.before_root_turn(&thread) == super::supervisor::RootTurnGate::Proceed
        })
    }
    fn binding(&self, driver: Driver, generation: ProcessGeneration) -> Option<LaunchBinding> {
        let sup = self.supervisor.upgrade()?;
        let epoch = sup.epoch(&self.worker).ok()?;
        let mut state = sup.state.borrow_mut();
        let w = state.workers.get_mut(&self.worker)?;
        if w.driver != driver
            || w.staged.as_ref()?.generation != generation
            || w.staged.as_ref()?.worker != self.worker
        {
            return None;
        }
        let prep = w.staged.take()?;
        w.token = Some(TokenSlot {
            epoch,
            generation,
            token: prep.token.clone(),
            consumed_by: None,
            live_conn: None,
            pid: None,
            issued: Instant::now(),
        });
        // The watch is attached only after a successful spawn. Keep the overlay until then.
        w.pending_overlay = prep.overlay;
        w.row.delegation_status = "available".into();
        w.observed = ObservedCalls::new(sup.salt, sup.limits.observed_calls as usize);
        w.seen_calls.clear();
        Some(prep.launch)
    }
    fn process_started(&self, generation: ProcessGeneration, pid: Option<i32>, watch: ExitWatch) {
        let Some(sup) = self.supervisor.upgrade() else {
            return;
        };
        {
            let mut state = sup.state.borrow_mut();
            let Some(w) = state.workers.get_mut(&self.worker) else {
                return;
            };
            w.generation = generation;
            if let Some(t) = w.token.as_mut().filter(|t| t.generation == generation) {
                t.pid = pid.and_then(|p| u32::try_from(p).ok());
                t.issued = Instant::now();
            }
            let overlay = w.pending_overlay.take();
            w.generations.insert(
                generation,
                GenRt {
                    watch: watch.clone(),
                    overlay,
                },
            );
        }
        let weak = self.supervisor.clone();
        let worker = self.worker.clone();
        watch.connect_phase(move |p| {
            if p == StopPhase::Exited {
                if let Some(sup) = weak.upgrade() {
                    sup.observed_exit(&worker, generation);
                }
            }
        });
        if watch.phase() == StopPhase::Exited {
            sup.observed_exit(&self.worker, generation);
        }
        let cap = {
            let state = sup.state.borrow();
            state
                .workers
                .get(&self.worker)
                .filter(|w| w.driver == Driver::Agy)
                .and_then(|w| w.token.as_ref().map(|t| t.token.clone()))
        };
        if let Some(cap) = cap {
            sup.arm_agy_hello_window(
                &self.worker,
                generation,
                cap,
                std::time::Duration::from_millis(sup.limits.agy_hello_window_ms),
            );
        }
    }
    fn process_stopping(&self, generation: ProcessGeneration) {
        let Some(sup) = self.supervisor.upgrade() else {
            return;
        };
        let root = {
            let mut state = sup.state.borrow_mut();
            let Some(w) = state.workers.get_mut(&self.worker) else {
                return;
            };
            if w.generation != generation {
                return;
            }
            if w.rotating_generation == Some(generation) {
                w.rotating_generation = None;
                w.token = None;
                return;
            }
            if w.token.take().is_none() {
                return;
            }
            w.root.clone()
        };
        sup.begin_revoke(
            &root,
            RevocationEvent::GenerationExited(self.worker.clone()),
        );
    }
    fn line(&self, generation: ProcessGeneration, line: &str, stderr: bool) {
        if stderr {
            return;
        }
        let Some(sup) = self.supervisor.upgrade() else {
            return;
        };
        let Ok(v) = serde_json::from_str::<Value>(line) else {
            return;
        };
        let step = {
            let mut state = sup.state.borrow_mut();
            let Some(w) = state.workers.get_mut(&self.worker) else {
                return;
            };
            if w.generation != generation {
                return;
            }
            observe_calls(
                &mut w.observed,
                &mut w.seen_calls,
                w.driver,
                &v,
                &sup.limits,
            );
            w.readback.line(&v)
        };
        sup.readback_step(&self.worker, step);
        sup.delivery_echo(&self.worker, &v);
        let identity = {
            use agent_core::delegation::identity::{observe, Identity, IdentityObservation};
            let mut state = sup.state.borrow_mut();
            let Some(w) = state.workers.get_mut(&self.worker) else {
                return;
            };
            let mut identity = w
                .row
                .identity_json
                .as_deref()
                .and_then(|s| serde_json::from_str::<Identity>(s).ok());
            if let Some(identity) = identity.as_mut() {
                let model = match w.driver {
                    Driver::Claude => v.pointer("/message/model").and_then(Value::as_str),
                    Driver::Agy => v.get("model").and_then(Value::as_str),
                    Driver::Codex => None,
                };
                if let Some(model) = model.filter(|m| !m.is_empty() && *m != "unrecognized_model") {
                    observe(identity, IdentityObservation::ReportedModel(model.into()));
                }
                sanitise_identity(identity);
                w.row.identity_json = serde_json::to_string(identity).ok();
            }
            identity
        };
        if let Some(identity) = identity {
            let worker = self.worker.to_string();
            sup.services
                .write("delegated identity", None, false, move |s| {
                    s.set_delegation_worker_identity(&worker, &identity)
                });
        }
    }
    fn observed(&self, generation: ProcessGeneration, env: &Envelope) {
        let Some(sup) = self.supervisor.upgrade() else {
            return;
        };
        if sup
            .state
            .borrow()
            .workers
            .get(&self.worker)
            .is_none_or(|w| w.generation != generation)
        {
            return;
        }
        if let Event::ControlResult { .. } = env.event {
            sup.delivery_control(&self.worker, env);
        }
    }
    fn turn(&self, generation: ProcessGeneration, boundary: TurnBoundary<'_>) {
        let Some(sup) = self.supervisor.upgrade() else {
            return;
        };
        let ended = matches!(boundary, TurnBoundary::Ended { .. });
        let step = {
            let mut state = sup.state.borrow_mut();
            let Some(w) = state.workers.get_mut(&self.worker) else {
                return;
            };
            if w.generation != generation {
                return;
            }
            match boundary {
                TurnBoundary::Ended { state } => w.readback.ended(state),
                TurnBoundary::Started { native_turn } => {
                    // Canonical envelopes provide authoritative turn identity; this callback
                    // is only a lifecycle hint and may precede that envelope.
                    let _ = native_turn;
                    super::readback::Step::Wait
                }
            }
        };
        // A bootstrap completion is published before the real task begins.
        let worker = self.worker.clone();
        let later = sup.clone();
        glib::spawn_future_local(async move {
            glib::timeout_future(std::time::Duration::ZERO).await;
            later.readback_step(&worker, step);
        });
        if ended {
            sup.release_waits(&self.worker);
        }
    }
    fn mode_changed(&self, effective: Mode) {
        let Some(sup) = self.supervisor.upgrade() else {
            return;
        };
        let event = {
            let mut state = sup.state.borrow_mut();
            let Some(w) = state.workers.get_mut(&self.worker) else {
                return;
            };
            let old = w.mode;
            w.mode = effective.session_safe();
            if w.row.parent_id.is_none()
                && ceiling(old).iter().any(|m| !ceiling(w.mode).contains(m))
            {
                Some((
                    w.root.clone(),
                    RevocationEvent::RootRestriction { new_mode: w.mode },
                ))
            } else {
                None
            }
        };
        if let Some((root, event)) = event {
            sup.begin_revoke(&root, event);
        }
    }
}
impl Supervisor {
    pub(super) fn arm_agy_hello_window(
        self: &std::rc::Rc<Self>,
        worker: &WorkerId,
        generation: ProcessGeneration,
        cap: agent_core::delegation::CapabilityToken,
        window: std::time::Duration,
    ) {
        let weak = self.weak.clone();
        let worker = worker.clone();
        glib::spawn_future_local(async move {
            glib::timeout_future(window).await;
            let Some(sup) = weak.upgrade() else {
                return;
            };
            let data = {
                let mut state = sup.state.borrow_mut();
                let Some(w) = state.workers.get_mut(&worker).filter(|w| {
                    w.generation == generation
                        && w.token.as_ref().is_some_and(|t| {
                            t.token.ct_eq(cap.expose_secret()) && t.consumed_by.is_none()
                        })
                }) else {
                    return;
                };
                w.token = None;
                (w.root.clone(), w.session())
            };
            sup.set_status(
                &worker,
                "rebind_required",
                "agy bridge hello window expired",
            );
            sup.begin_revoke(&data.0, RevocationEvent::GenerationExited(worker.clone()));
            if let Some(session) = data.1 {
                session.expire_approvals("agy bridge hello window expired");
                session.rebind_stop();
            }
        });
    }
    /// Same authority, fresh process credentials. A real concurrent revocation still latches
    /// the tree and makes the later E7 check fail.
    pub(super) fn rotate_before_stop(&self, worker: &WorkerId, generation: ProcessGeneration) {
        let (conn, session) = {
            let mut state = self.state.borrow_mut();
            let Some(w) = state
                .workers
                .get_mut(worker)
                .filter(|w| w.generation == generation)
            else {
                return;
            };
            w.rotating_generation = Some(generation);
            (w.token.take().and_then(|t| t.live_conn), w.session())
        };
        if let (Some(conn), Some(ipc)) = (conn, self.ipc.borrow().as_ref()) {
            ipc.revoke(
                conn,
                super::supervisor::error(
                    agent_core::delegation::ErrorCode::TransportLost,
                    "process credentials rotated",
                ),
            );
        }
        if let Some(session) = session {
            session.expire_approvals("Delegated process credentials rotated");
        }
    }
    pub(super) fn observed_exit(
        self: &std::rc::Rc<Self>,
        worker: &WorkerId,
        generation: ProcessGeneration,
    ) {
        let (overlay, root, unexpected) = {
            let mut state = self.state.borrow_mut();
            let Some(w) = state.workers.get_mut(worker) else {
                return;
            };
            let Some(g) = w.generations.remove(&generation) else {
                return;
            };
            let unexpected = w.token.as_ref().is_some_and(|t| t.generation == generation);
            if unexpected {
                w.token = None;
            }
            (g.overlay, w.root.clone(), unexpected)
        };
        if let Some(path) = overlay {
            gio::spawn_blocking(move || super::launch::unlink_overlay(&path));
        }
        if unexpected {
            self.begin_revoke(&root, RevocationEvent::GenerationExited(worker.clone()));
        }
        self.notify_registry();
    }
    pub(super) fn readback_step(
        self: &std::rc::Rc<Self>,
        worker: &WorkerId,
        step: super::readback::Step,
    ) {
        match step {
            super::readback::Step::Task => self.dispatch_pending(worker),
            super::readback::Step::Wait => {}
            super::readback::Step::Stop(e) => {
                self.set_status(worker, "unavailable", &e.message);
                self.fail_worker(worker, e);
            }
        }
    }
}
fn observe_calls(
    calls: &mut ObservedCalls,
    seen: &mut std::collections::VecDeque<InvocationKey>,
    driver: Driver,
    v: &Value,
    limits: &agent_core::delegation::Limits,
) {
    let mut record = |key: InvocationKey, name: &str, args: &Value| {
        let name = name.strip_prefix("mcp__agent_terminal__").unwrap_or(name);
        if let Some(tool) = agent_core::delegation::ToolName::parse(name) {
            if let Ok(args) = attestation_arguments(tool, args, limits) {
                if seen.contains(&key) {
                    return;
                }
                if seen.len() >= limits.observed_calls as usize {
                    seen.pop_front();
                }
                seen.push_back(key.clone());
                calls.record(key, tool, &args);
            }
        }
    };
    match driver {
        Driver::Claude => {
            if let Some(a) = v.pointer("/message/content").and_then(Value::as_array) {
                for item in a {
                    if item.get("type").and_then(Value::as_str) == Some("tool_use") {
                        if let (Some(id), Some(name), Some(args)) = (
                            item["id"].as_str(),
                            item["name"].as_str(),
                            item.get("input"),
                        ) {
                            record(
                                InvocationKey::Claude {
                                    tool_use_id: id.into(),
                                },
                                name,
                                args,
                            );
                        }
                    }
                }
            }
        }
        Driver::Codex => {
            if v.get("method")
                .and_then(Value::as_str)
                .is_some_and(|m| m == "item/started" || m == "item/completed")
            {
                let p = &v["params"];
                let item = &p["item"];
                if item["type"] == "mcpToolCall" && item["server"] == "agent_terminal" {
                    if let (Some(thread), Some(id), Some(tool), Some(args)) = (
                        p["threadId"].as_str(),
                        item["id"].as_str(),
                        item["tool"].as_str(),
                        item.get("arguments"),
                    ) {
                        record(
                            InvocationKey::Codex {
                                thread_id: thread.into(),
                                call_id: id.into(),
                            },
                            tool,
                            args,
                        );
                    }
                }
            }
        }
        Driver::Agy => {
            let p = v.get("step_update").unwrap_or(v);
            let params = p.pointer("/tool_info/parameters");
            if p.get("tool_name").and_then(Value::as_str) != Some("call_mcp_tool")
                || params
                    .and_then(|p| p.get("ServerName"))
                    .and_then(Value::as_str)
                    != Some("agent_terminal")
            {
                return;
            }
            if let (Some(c), Some(i), Some(t), Some(a)) = (
                p.get("conversation_id")
                    .or_else(|| v.get("conversation_id"))
                    .and_then(Value::as_str),
                p["step_index"].as_u64(),
                params
                    .and_then(|p| p.get("ToolName"))
                    .and_then(Value::as_str),
                params.and_then(|p| p.get("Arguments")),
            ) {
                record(
                    InvocationKey::Agy {
                        conversation_id: c.into(),
                        step_index: i,
                    },
                    t,
                    a,
                );
            }
        }
    }
}
