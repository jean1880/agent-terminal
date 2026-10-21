//! Supervised child construction and guarded process startup.

use std::path::PathBuf;
use std::rc::Rc;

use agent_core::adapter::OpenSession;

use super::seam::SessionOptions;
use super::seam::{AuthorityParams, ChildPolicy, DelegatedInput, InputSource, LiveAuthority};
use super::supervisor::{error, opted_in, Supervisor};
use crate::chat::session::{Approval, ChatSession, SessionStore};
use crate::chat::EnvelopeSink;
use crate::config::Profile;
use agent_core::adapter::Driver;
use agent_core::delegation::authority::{InheritedAuthority, TaskMode, WorkspaceScope};
use agent_core::delegation::identity::{observe, Identity, IdentityObservation};
use agent_core::delegation::wire::*;
use agent_core::delegation::{AuthorityEpoch, ErrorCode, ProcessGeneration, ToolError, WorkerId};
use agent_kit::store::delegation_typed::*;

#[derive(Clone)]
pub(super) struct ChildSpec {
    pub worker: WorkerId,
    pub root: WorkerId,
    pub parent: WorkerId,
    pub depth: u8,
    pub profile: Profile,
    pub driver: Driver,
    pub mode: TaskMode,
    pub model: Option<String>,
    pub effort: Option<String>,
    pub scope: WorkspaceScope,
    pub cwd: PathBuf,
    pub thread: String,
    pub provider: String,
    pub lease: Option<super::workspace::WorktreeLease>,
    pub resume: Option<String>,
}
impl Supervisor {
    pub(super) async fn ensure_child(
        self: &Rc<Self>,
        worker: &WorkerId,
        input: &DelegatedInput,
    ) -> Result<(), ToolError> {
        self.dispatch_check(&input.from, worker, input.epoch, None)?;
        self.epoch(worker)?;
        let (session, ready, generation) = {
            let state = self.state.borrow();
            let w = state
                .workers
                .get(worker)
                .ok_or_else(|| error(ErrorCode::NotFound, "unknown worker"))?;
            (
                w.session(),
                w.token.is_some()
                    && w.readback.ready
                    && !w.generations.is_empty()
                    && w.row.delegation_status != "rebind_required"
                    && w.token.as_ref().is_some_and(|t| {
                        t.epoch.0 == state.roots[&w.root].row.authority_epoch.max(0) as u64
                    }),
                ProcessGeneration(w.generation.0 + 1),
            )
        };
        if ready {
            return Ok(());
        }
        let mut spec = self
            .state
            .borrow()
            .child_specs
            .get(worker)
            .cloned()
            .ok_or_else(|| {
                error(
                    ErrorCode::ProviderUnavailable,
                    "child launch must be restored before receiving work",
                )
            })?;
        if spec.provider.is_empty() {
            let thread = spec.thread.clone();
            let driver = spec.driver;
            spec.provider = self
                .services
                .durable("restore child provider thread", move |s| {
                    s.add_provider_thread(&thread, driver.info().key, "")
                })
                .await?;
            self.state
                .borrow_mut()
                .child_specs
                .insert(worker.clone(), spec.clone());
        }
        let approval = self.prepare_launch(&spec, generation).await?;
        self.dispatch_check(&input.from, worker, input.epoch, None)?;
        {
            let mut state = self.state.borrow_mut();
            let w = state
                .workers
                .get_mut(worker)
                .ok_or_else(|| error(ErrorCode::NotFound, "worker closed"))?;
            w.readback = super::readback::Readback::child(spec.driver, spec.mode);
        }
        let session = if let Some(session) = session {
            self.rotate_before_stop(worker, session.process_generation());
            if let Some(watch) = session.rebind_stop() {
                super::capacity::wait_exit(&watch).await;
            }
            self.dispatch_check(&input.from, worker, input.epoch, None)?;
            session.rebind_start();
            session
        } else {
            self.build_child(&spec, approval)?
        };
        if spec.driver == Driver::Claude {
            let mut bootstrap = input.clone();
            bootstrap.source = InputSource::Bootstrap;
            bootstrap.text =
                "agent-terminal startup check: reply with exactly OK and do not use any tool."
                    .into();
            bootstrap.plan = agent_core::delegation::mailbox::DeliveryPlan::StartTurn;
            let _ = session.deliver_delegated(bootstrap);
        }
        self.arm_startup(worker, generation);
        let start = std::time::Instant::now();
        loop {
            self.epoch(worker)?;
            let (ready, failed) = {
                let state = self.state.borrow();
                let w = &state.workers[worker];
                (w.readback.ready, w.row.state == "failed")
            };
            if ready {
                return Ok(());
            }
            if failed || start.elapsed() > std::time::Duration::from_secs(11) {
                return Err(error(
                    ErrorCode::ProviderUnavailable,
                    "child startup read-back failed",
                ));
            }
            gtk4::glib::timeout_future(std::time::Duration::from_millis(10)).await;
        }
    }
    pub(super) async fn spawn_worker(
        self: &Rc<Self>,
        caller: &WorkerId,
        r: SpawnRequest,
    ) -> Result<serde_json::Value, ToolError> {
        if let Some(v) = self
            .existing(caller, &ToolRequest::Spawn(r.clone()))
            .await?
        {
            return Ok(v);
        }
        let epoch = self.epoch(caller)?;
        if !agent_core::delegation::authority::permits(self.principal_mode(caller), r.task_mode) {
            return Err(error(
                ErrorCode::AuthorityDenied,
                "requested child mode exceeds the caller's live authority",
            ));
        }
        agent_core::delegation::authority::workspace_allowed(r.task_mode, &r.workspace)
            .map_err(|c| error(c, "workspace does not support the task mode"))?;
        let profile = (self.services.profiles)()
            .into_iter()
            .find(|p| p.id == r.profile_id.as_str() && opted_in(p))
            .ok_or_else(|| {
                error(
                    ErrorCode::ProviderUnavailable,
                    "profile unavailable for delegation",
                )
            })?;
        let resolved = (self.services.resolve)(&profile).ok_or_else(|| {
            error(
                ErrorCode::ProviderUnavailable,
                "agent executable unavailable",
            )
        })?;
        let driver = crate::config::profile_driver(&profile)
            .ok_or_else(|| error(ErrorCode::ProviderUnavailable, "profile has no adapter"))?;
        let (parent_driver, root, cwd, depth) = {
            let state = self.state.borrow();
            let w = &state.workers[caller];
            (w.driver, w.root.clone(), w.cwd.clone(), w.row.depth + 1)
        };
        let route = self
            .routes
            .admit(&agent_core::delegation::route::RouteQuery {
                parent: parent_driver,
                child: driver,
                task_mode: r.task_mode,
                child_bridge: agent_core::delegation::route::ChildBridge::WithBridge,
            });
        if route.status != agent_core::delegation::route::RouteStatus::Verified {
            return Err(error(
                ErrorCode::RouteUnverified,
                "provider route lacks reviewed evidence",
            )
            .with_detail("route_status", serde_json::json!(route.status))
            .with_detail("blockers", serde_json::json!(route.blockers)));
        }
        if depth > self.limits.max_depth as i64 {
            return Err(error(
                ErrorCode::AuthorityDenied,
                "maximum delegation depth reached",
            ));
        }
        agent_core::delegation::limits::admit_spawn(&self.limits, &self.usage(&root), true)?;
        let model = r.model.clone().or(resolved.default_model.clone());
        let effort = r.effort.clone().or(resolved.default_effort.clone());
        if model
            .iter()
            .chain(effort.iter())
            .any(|value| agent_core::redact::redact(value) != *value)
        {
            return Err(error(
                ErrorCode::InvalidArgument,
                "model or effort contains protected text",
            ));
        }
        let models = crate::model_catalog::ModelCatalog::shared().models_of(driver);
        if driver == Driver::Claude
            && effort.as_ref().is_some_and(|e| {
                !models
                    .iter()
                    .any(|m| Some(&m.id) == model.as_ref() && m.efforts.contains(e))
            })
        {
            return Err(error(
                ErrorCode::UnsupportedEffort,
                "Claude effort is not listed by the model catalogue",
            ));
        }
        let mut warnings = Vec::new();
        if driver == Driver::Codex
            && effort.as_ref().is_some_and(|e| {
                !models
                    .iter()
                    .any(|m| Some(&m.id) == model.as_ref() && m.efforts.contains(e))
            })
        {
            warnings.push(Warning::new(
                "effort_not_listed",
                "The provider has not listed this effort.",
            ));
        }
        // agy takes no `--effort` for a catalogue model ("--effort is not supported for model …"):
        // its effort rides in the model id (`gemini-4-argon-high`), as a thread's picker does.
        let model = if driver == Driver::Agy {
            if effort.as_ref().is_some_and(|e| {
                models
                    .iter()
                    .any(|m| Some(&m.id) == model.as_ref() && !m.efforts.contains(e))
            }) {
                warnings.push(Warning::new(
                    "effort_not_listed",
                    "The provider has not listed this effort; the model's default is used.",
                ));
            }
            super::catalog_info::agy_launch_model(&models, model, effort.as_deref())
        } else {
            model
        };
        // A lease namespace is random and independent of the authority worker minted at accept.
        let lease_plan = if let WorkspaceRequest::IsolatedWorktree { base } = &r.workspace {
            let namespace =
                WorkerId::from_suffix(&gtk4::glib::uuid_string_random().replace('-', ""))
                    .map_err(|_| error(ErrorCode::InternalError, "cannot allocate lease name"))?;
            let cwd = cwd.clone();
            let base = base.clone().unwrap_or_else(|| "HEAD".into());
            Some(
                gtk4::gio::spawn_blocking(move || {
                    super::workspace::plan_lease(&cwd, &base, &namespace, None)
                })
                .await
                .map_err(|_| {
                    error(ErrorCode::InternalError, "workspace planning worker failed")
                })??,
            )
        } else {
            None
        };
        let child_cwd = lease_plan.as_ref().map(|l| l.path.clone()).unwrap_or(cwd);
        if lease_plan.as_ref().is_some_and(|l| l.dirty_parent) {
            warnings.push(Warning::new("parent_changes_absent","The child worktree starts at the selected commit and does not include uncommitted parent changes."));
        }
        let workspace = WorkspaceInfo {
            kind: r.workspace.kind(),
            path: Some(child_cwd.to_string_lossy().into_owned()),
            base_commit: lease_plan.as_ref().map(|p| p.base_commit.clone()),
        };
        let mut identity = Identity::new(driver, r.profile_id.clone());
        observe(
            &mut identity,
            IdentityObservation::Launched {
                model: model.clone(),
                effort: effort.clone(),
            },
        );
        super::attest::sanitise_identity(&mut identity);
        let plan=SpawnPlan{driver,identity,workspace,launch_json:serde_json::json!({"model":model,"effort":effort,"cwd":child_cwd,"source_repo":lease_plan.as_ref().map(|l|&l.source_repo),"branch":lease_plan.as_ref().map(|l|&l.branch),"base_commit":lease_plan.as_ref().map(|l|&l.base_commit)}).to_string(),cwd:child_cwd.to_string_lossy().into_owned(),state:SpawnState::Starting,process_generation:1};
        let accept = Accept {
            principal: caller.clone(),
            request: ToolRequest::Spawn(r.clone()),
            resolution: agent_core::delegation::operation::Resolution {
                target: None,
                bound_runs: Vec::new(),
                base_commit: lease_plan.as_ref().map(|l| l.base_commit.clone()),
            },
            plan: AcceptPlan::Spawn(Box::new(plan)),
            warnings,
            limits: self.limits,
            live_processes: self.usage(&root).live_processes,
            dispatching: self.state.borrow().dispatching.clone(),
        };
        let (accepted, _) = self.accept(accept, true).await?;
        let accepted = match accepted {
            Accepted::Replay(r) => return super::ops::replay_value(r),
            Accepted::New(a) => a,
        };
        let worker = accepted
            .worker
            .clone()
            .ok_or_else(|| error(ErrorCode::InternalError, "spawn accepted without a worker"))?;
        tracing::info!(
            %worker,
            parent = %caller,
            ?driver,
            mode = ?r.task_mode,
            run = ?accepted.run.as_ref().map(|r| r.to_string()),
            "delegated worker spawned"
        );
        let lease = if let Some(plan) = lease_plan {
            match gtk4::gio::spawn_blocking(move || super::workspace::create_lease(&plan)).await {
                Ok(Ok(l)) => Some(l),
                _ => {
                    self.persist_worker_failure(
                        worker.clone(),
                        accepted.run.clone(),
                        Some(accepted.operation.clone()),
                        error(
                            ErrorCode::UnsupportedWorkspace,
                            "cannot create child worktree",
                        ),
                    )
                    .await;
                    return Ok(accepted.receipt);
                }
            }
        } else {
            None
        };
        let wid = worker.clone();
        let prepared = self
            .services
            .durable("prepare child backing thread", move |s| {
                let row = s
                    .delegation_worker(wid.as_str())?
                    .ok_or(agent_kit::store::StoreError::WriterGone)?;
                let thread = row
                    .backing_thread_id
                    .as_deref()
                    .ok_or(agent_kit::store::StoreError::WriterGone)?;
                let provider = s.add_provider_thread(thread, driver.info().key, "")?;
                Ok((row, provider))
            })
            .await;
        let (row, provider) = match prepared {
            Ok(prepared) => prepared,
            Err(e) => {
                self.persist_worker_failure(
                    worker.clone(),
                    accepted.run.clone(),
                    Some(accepted.operation.clone()),
                    e,
                )
                .await;
                return Ok(accepted.receipt);
            }
        };
        let spec = ChildSpec {
            worker: worker.clone(),
            root: root.clone(),
            parent: caller.clone(),
            depth: depth as u8,
            profile,
            driver,
            mode: r.task_mode,
            model,
            effort,
            scope: lease
                .as_ref()
                .map(super::workspace::scope)
                .unwrap_or_else(|| WorkspaceScope::Shared {
                    root: child_cwd.to_string_lossy().into_owned(),
                }),
            cwd: child_cwd,
            thread: row.backing_thread_id.clone().unwrap_or_default(),
            provider,
            lease,
            resume: None,
        };
        let mut runtime = self.worker_rt(row, root, spec.cwd.clone());
        runtime.run = accepted.run.clone();
        runtime.readback = super::readback::Readback::child(driver, r.task_mode);
        runtime.pending = Some(DelegatedInput {
            operation: accepted.operation.clone(),
            epoch,
            source: InputSource::Task,
            from: caller.clone(),
            from_label: self.state.borrow().workers[caller].row.name.clone(),
            text: agent_core::redact::redact(&r.task),
            plan: agent_core::delegation::mailbox::DeliveryPlan::StartTurn,
            tag: accepted.operation.to_string(),
        });
        {
            let mut state = self.state.borrow_mut();
            state.workers.insert(worker.clone(), runtime);
            state.child_specs.insert(worker.clone(), spec.clone());
            state.dispatching.insert(accepted.operation.clone());
        }
        self.notify_registry();
        let prepared = self.prepare_launch(&spec, ProcessGeneration(1)).await;
        match prepared {
            Ok(approval) => {
                if let Err(e) = self.dispatch_check(caller, &worker, epoch, accepted.run.as_ref()) {
                    self.fail_worker(&worker, e);
                } else {
                    if let Err(e) = self.build_child(&spec, approval) {
                        self.fail_worker(&worker, e);
                        return Ok(accepted.receipt);
                    }
                    // agy's task waits for its process (`Readback::started`); Codex's for its
                    // read-back; Claude's for its startup check.
                    if driver == Driver::Claude {
                        self.send_bootstrap(&worker);
                    }
                    self.arm_startup(&worker, ProcessGeneration(1));
                }
            }
            Err(e) => self.fail_worker(&worker, e),
        }
        self.notify_registry();
        Ok(accepted.receipt)
    }
    pub(super) async fn prepare_launch(
        self: &Rc<Self>,
        spec: &ChildSpec,
        generation: ProcessGeneration,
    ) -> Result<Approval, ToolError> {
        if let Some(lease) = spec.lease.clone() {
            gtk4::gio::spawn_blocking(move || super::workspace::verify_lease(&lease))
                .await
                .map_err(|_| {
                    error(
                        ErrorCode::UnsupportedWorkspace,
                        "lease verification worker failed",
                    )
                })??;
        }
        let approval = if spec.driver == Driver::Agy {
            let resolved = (self.services.resolve)(&spec.profile)
                .ok_or_else(|| error(ErrorCode::ProviderUnavailable, "agent unavailable"))?;
            let hook = (self.services.hook_check)(crate::hook_config::HookTarget::new(
                &resolved.env,
                Some(spec.cwd.clone()),
            ))
            .await;
            hook.map_err(|_| {
                error(
                    ErrorCode::ProviderUnavailable,
                    "agy child requires a proven approval hook",
                )
            })?;
            self.recheck_agy(&spec.worker).await?;
            crate::approval_server::ApprovalHandle::bind_child(
                Ok(()),
                &spec.thread,
                &spec.cwd,
                spec.mode.as_mode(),
            )
            .map_err(|_| {
                error(
                    ErrorCode::ProviderUnavailable,
                    "cannot bind child approval socket",
                )
            })
            .map(Ok)?
        } else {
            Err(None)
        };
        self.stage(&spec.worker, generation, &spec.profile).await?;
        Ok(approval)
    }
    pub(super) fn build_child(
        self: &Rc<Self>,
        spec: &ChildSpec,
        approval: Approval,
    ) -> Result<Rc<ChatSession>, ToolError> {
        let resolved = (self.services.resolve)(&spec.profile).ok_or_else(|| {
            error(
                ErrorCode::ProviderUnavailable,
                "child environment is unavailable",
            )
        })?;
        let weak = self.weak.clone();
        let worker = spec.worker.clone();
        let epoch = self.epoch(&spec.parent).unwrap_or(AuthorityEpoch(u64::MAX));
        let root = spec.root.clone();
        let parent = spec.parent.clone();
        let policy_revision = self
            .state
            .borrow()
            .roots
            .get(&root)
            .map_or(0, |r| r.row.policy_revision.max(0) as u64);
        let inherited = InheritedAuthority {
            worker: worker.clone(),
            root,
            parent: Some(parent.clone()),
            epoch,
            policy_revision,
            mode_ceiling: spec.mode.as_mode(),
            depth: spec.depth,
            workspace: spec.scope.clone(),
        };
        let live_worker = worker.clone();
        let live = Rc::new(move || {
            let Some(s) = weak.upgrade() else {
                return LiveAuthority {
                    valid: false,
                    bootstrap: false,
                    parent_mode: agent_core::adapter::Mode::Plan,
                };
            };
            let current = s.epoch(&live_worker).ok();
            let valid = current.is_some()
                && current
                    == s.state
                        .borrow()
                        .workers
                        .get(&live_worker)
                        .and_then(|w| w.token.as_ref().map(|t| t.epoch));
            let (ready, bootstrap) = {
                let state = s.state.borrow();
                state
                    .workers
                    .get(&live_worker)
                    .map(|w| (w.readback.ready, w.readback.bootstrap))
                    .unwrap_or((false, false))
            };
            LiveAuthority {
                valid: valid && ready,
                bootstrap,
                parent_mode: s.principal_mode(&parent),
            }
        });
        let gate = Rc::new(super::authority::ChildAuthority::new(AuthorityParams {
            inherited,
            driver: spec.driver,
            task_mode: spec.mode,
            home: resolved
                .env
                .env
                .iter()
                .rev()
                .find(|(name, _)| name == "HOME")
                .map(|(_, value)| PathBuf::from(value))
                .or_else(|| {
                    (!resolved.env.unset.iter().any(|name| name == "HOME"))
                        .then(|| std::env::var_os("HOME").map(PathBuf::from))
                        .flatten()
                }),
            live,
        }));
        let publisher = Rc::new(super::publish::Publisher::new(
            self.weak.clone(),
            worker.clone(),
        ));
        // The publisher emits scoped activity only after its store receipt commits.
        let sink: EnvelopeSink = Rc::new(|_| {});
        let mut env = resolved.env;
        for name in agent_core::delegation::env::STRIP_ON_SPAWN {
            if !env.unset.iter().any(|n| n == name) {
                env.unset.push(name.into());
            }
        }
        let open = OpenSession {
            program: resolved.program.clone(),
            extra_args: resolved.extra_args,
            cwd: spec.cwd.to_string_lossy().into_owned(),
            model: spec.model.clone(),
            effort: spec.effort.clone(),
            mode: spec.mode.as_mode(),
            resume: spec.resume.clone(),
            new_session_id: (spec.driver == Driver::Claude && spec.resume.is_none())
                .then(|| gtk4::glib::uuid_string_random().to_string()),
            approval_hook: false,
            delegation: None,
        };
        let generation_base = self.state.borrow().workers[&worker].generation;
        let session = ChatSession::with_options(
            (self.services.make_adapter)(spec.driver, &resolved.program),
            open,
            SessionStore {
                handle: self.services.handle.clone(),
                provider: Some(Some(spec.provider.clone())),
            },
            spec.thread.clone(),
            sink,
            approval,
            env,
            SessionOptions {
                generation_base,
                hooks: Some(Rc::new(super::attest::Hooks {
                    supervisor: self.weak.clone(),
                    worker: worker.clone(),
                })),
                child: Some(ChildPolicy {
                    gate,
                    publish: publisher,
                    mode: spec.mode.as_mode(),
                }),
            },
        );
        session.set_delegation_worker(worker.clone());
        if let Some(w) = self.state.borrow_mut().workers.get_mut(&worker) {
            w.session = Some(session.clone());
        }
        Ok(session)
    }
    pub(super) fn send_bootstrap(&self, worker: &WorkerId) {
        let data = {
            let state = self.state.borrow();
            state
                .workers
                .get(worker)
                .and_then(|w| Some((w.session()?, w.pending.clone()?)))
        };
        if let Some((session, mut input)) = data {
            input.source = InputSource::Bootstrap;
            input.text =
                "agent-terminal startup check: reply with exactly OK and do not use any tool."
                    .into();
            let _ = session.deliver_delegated(input);
        }
    }
    pub(super) fn arm_startup(self: &Rc<Self>, worker: &WorkerId, generation: ProcessGeneration) {
        let sup = self.weak.clone();
        let worker = worker.clone();
        gtk4::glib::spawn_future_local(async move {
            gtk4::glib::timeout_future(std::time::Duration::from_secs(10)).await;
            if let Some(s) = sup.upgrade() {
                let step = {
                    let mut state = s.state.borrow_mut();
                    let Some(w) = state
                        .workers
                        .get_mut(&worker)
                        .filter(|w| w.generation <= generation)
                    else {
                        return;
                    };
                    w.readback
                        .hello_timeout(w.token.as_ref().is_some_and(|t| t.live_conn.is_some()))
                };
                s.readback_step(&worker, step);
            }
        });
    }
    pub(super) async fn persist_worker_failure(
        &self,
        worker: WorkerId,
        run: Option<agent_core::delegation::RunId>,
        operation: Option<agent_core::delegation::OperationId>,
        e: ToolError,
    ) {
        // The failure is a lifecycle event on the worker's page, so its parent learns why
        // without the journal; a worker never registered in memory has no page to show it on.
        let notice = self.publication(
            &worker,
            agent_core::event::Envelope::new(agent_core::event::Event::Notice {
                text: format!("worker failed: {}", e.message),
            }),
            Some(EventCategory::Lifecycle),
        );
        let failed = self
            .services
            .durable("fail delegated worker", move |s| {
                let at = super::supervisor::now();
                if let Some(run) = run {
                    if s.delegation_run(run.as_str())?.is_some_and(|r| {
                        !matches!(
                            r.state.as_str(),
                            "completed" | "interrupted" | "failed" | "unknown"
                        )
                    }) {
                        s.set_delegation_run_state(
                            run.as_str(),
                            "failed",
                            None,
                            Some(&e.message),
                            None,
                            at,
                        )?;
                    }
                }
                if s.delegation_worker(worker.as_str())?
                    .is_some_and(|w| w.state != "closed")
                {
                    s.set_delegation_worker_state(worker.as_str(), "failed", Some(&e.message), at)?;
                    if let Some(notice) = &notice {
                        s.publish_delegation_event(notice)?;
                    }
                }
                if let Some(operation) = operation {
                    s.record_delegation_dispatch(&operation, &DispatchFact::Failed(e))?;
                }
                Ok(())
            })
            .await;
        if failed.is_err() {
            (self.services.report)("Delegated worker stopped, but its failure state could not be committed. Its durable outcome remains unknown.");
        }
    }
    /// Ends the open run of each stopped worker as interrupted, unless it already ended. Call
    /// after their sessions shut down: a turn in flight was ended as interrupted then, but a stop
    /// that landed after the task was dispatched and before its turn opened had no turn to end,
    /// and would leave the run open for good (a parent waiting on it would wait out its
    /// deadline). In the writer's order, so whichever ending comes first wins.
    pub(super) fn interrupt_open_runs(self: &Rc<Self>, workers: &[WorkerId]) {
        let runs: Vec<_> = {
            let mut state = self.state.borrow_mut();
            workers
                .iter()
                .filter_map(|id| {
                    let w = state.workers.get_mut(id)?;
                    let run = w.run.take()?;
                    w.last_run = Some(run.clone());
                    Some(run)
                })
                .collect()
        };
        if runs.is_empty() {
            return;
        }
        let sup = self.clone();
        gtk4::glib::spawn_future_local(async move {
            let ended = sup
                .services
                .durable("interrupt stopped delegated runs", move |s| {
                    let at = super::supervisor::now();
                    for run in &runs {
                        if s.delegation_run(run.as_str())?.is_some_and(|r| {
                            !matches!(
                                r.state.as_str(),
                                "completed" | "interrupted" | "failed" | "unknown"
                            )
                        }) {
                            s.set_delegation_run_state(
                                run.as_str(),
                                "interrupted",
                                None,
                                Some("stopped"),
                                None,
                                at,
                            )?;
                        }
                    }
                    Ok(())
                })
                .await;
            if ended.is_err() {
                (sup.services.report)("Delegated workers stopped, but their runs could not be closed. Their durable outcome remains unknown.");
            }
        });
    }
    pub(super) fn fail_worker(self: &Rc<Self>, worker: &WorkerId, e: ToolError) {
        tracing::warn!(%worker, code = %e.code, reason = %e.message, "delegated worker failed");
        let data = {
            let mut state = self.state.borrow_mut();
            state.workers.get_mut(worker).map(|w| {
                w.row.state = "failed".into();
                w.row.delegation_reason = Some(e.message.clone());
                w.row.last_activity = Some(e.message.clone());
                let run = w.run.take();
                w.last_run = run.clone();
                w.token = None;
                (run, w.pending.take(), w.root.clone())
            })
        };
        if let Some((run, pending, root)) = data {
            let sessions = {
                let state = self.state.borrow();
                state
                    .workers
                    .iter()
                    .filter(|(id, _)| self.visible(worker, id, true))
                    .filter_map(|(id, w)| Some((id.clone(), w.session()?)))
                    .collect::<Vec<_>>()
            };
            self.begin_revoke(
                &root,
                agent_core::delegation::authority::RevocationEvent::GenerationExited(
                    worker.clone(),
                ),
            );
            let mut stopped = Vec::with_capacity(sessions.len());
            for (id, session) in sessions {
                session.expire_approvals("Delegated worker failed");
                session.shutdown(true);
                stopped.push(id);
            }
            self.interrupt_open_runs(&stopped);
            let operation = pending.as_ref().map(|p| p.operation.clone());
            if let Some(input) = pending {
                self.state.borrow_mut().dispatching.remove(&input.operation);
            }
            let sup = self.clone();
            let worker = worker.clone();
            gtk4::glib::spawn_future_local(async move {
                sup.persist_worker_failure(worker, run, operation, e).await;
            });
        }
        self.notify_registry();
    }
}
