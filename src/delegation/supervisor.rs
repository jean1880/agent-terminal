//! App-owned runtime. All mutable state belongs to the main context.
use super::ipc::IpcServer;
use super::seam::{
    BindingRequest, ChildStop, ConnId, DelegationHooks, PreparedBinding, Role, RuntimeDir,
    WorkerSubscriber,
};
use super::services::AppServices;
use crate::agent_proc::ExitWatch;
use crate::chat::session::ChatSession;
use crate::config::Profile;
use agent_core::adapter::{Driver, Mode};
use agent_core::delegation::authority::{revocation, RevocationEvent, TaskMode};
use agent_core::delegation::correlation::ObservedCalls;
use agent_core::delegation::route::{RouteQuery, RouteSpec};
use agent_core::delegation::wire::WorkerSummary;
use agent_core::delegation::{
    admit_route, AuthorityEpoch, CapabilityToken, ErrorCode, InstanceNonce, Limits, OperationId,
    ProcessGeneration, RunId, ToolError, WorkerId,
};
use agent_core::event::WorkerState;
use agent_core::serde_name::parse_lenient;
use agent_kit::store::delegation::{RootRow, WorkerRow};
use agent_kit::store::delegation_typed::NewRoot;
use gtk4::{gio, glib};
use std::cell::RefCell;
use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::PathBuf;
use std::rc::{Rc, Weak};
use std::time::Instant;

pub(crate) trait RouteTable {
    fn admit(&self, q: &RouteQuery) -> RouteSpec;
}

/// The most delegated events held in memory at once: the replay gate's buffer, an inspector
/// replay, a worker's unpublished events. Each says what happens past it.
pub(super) const HISTORY_EVENTS: usize = 4096;
/// The most serialised envelope bytes held with them. An envelope that cannot be measured
/// counts as all of it.
pub(super) const HISTORY_BYTES: usize = 4 * 1024 * 1024;

/// Buffers committed live delivery while a history snapshot is read, retaining sequence
/// numbers so events included in that snapshot are never delivered twice.
struct ReplayGate {
    subscriber: Weak<dyn WorkerSubscriber>,
    buffered: RefCell<Vec<(i64, agent_core::delegation::scope::ScopedEnvelope)>>,
    bytes: std::cell::Cell<usize>,
    overflow: std::cell::Cell<bool>,
}
impl WorkerSubscriber for ReplayGate {
    fn registry(&self, thread: &str, workers: &[WorkerSummary]) {
        if let Some(s) = self.subscriber.upgrade() {
            s.registry(thread, workers);
        }
    }
    fn scoped(&self, env: &agent_core::delegation::scope::ScopedEnvelope) {
        self.committed(env, i64::MAX);
    }
    fn committed(&self, env: &agent_core::delegation::scope::ScopedEnvelope, seq: i64) {
        let bytes = serde_json::to_vec(&env.envelope).map_or(HISTORY_BYTES, |v| v.len());
        if self.buffered.borrow().len() >= HISTORY_EVENTS
            || self.bytes.get().saturating_add(bytes) > HISTORY_BYTES
        {
            self.overflow.set(true);
            return;
        }
        self.bytes.set(self.bytes.get() + bytes);
        self.buffered.borrow_mut().push((seq, env.clone()));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::delegation::seam::{CallDispatcher, PeerFacts};
    use agent_core::delegation::ipc::{ApiRange, BridgeInfo, Hello};
    use agent_core::delegation::wire::*;
    use agent_core::delegation::{IdempotencyKey, ProfileId};
    use agent_kit::store::Store;
    use serde_json::Value;
    use std::time::Duration;

    struct Verified;
    impl RouteTable for Verified {
        fn admit(&self, _: &RouteQuery) -> RouteSpec {
            RouteSpec {
                status: agent_core::delegation::route::RouteStatus::Verified,
                blockers: Vec::new(),
            }
        }
    }
    fn harness() -> (tempfile::TempDir, Rc<Supervisor>, WorkerId) {
        harness_with("claude", false)
    }
    thread_local! {
        /// Catalog services for the next harness on this thread (default: none).
        static TEST_CATALOG: RefCell<Option<super::super::services::CatalogServices>> =
            const { RefCell::new(None) };
    }
    /// `request` as the bridge sends it, with call id `id`.
    fn call(id: u64, request: ToolRequest) -> agent_core::delegation::ipc::Call {
        agent_core::delegation::ipc::Call {
            id,
            tool: request.tool(),
            args: request.args_value(),
            timeout_ms: 1000,
            invocation: None,
        }
    }
    fn caps_call(id: u64, refresh: bool) -> agent_core::delegation::ipc::Call {
        call(
            id,
            ToolRequest::Capabilities(agent_core::delegation::wire::CapabilitiesRequest {
                profile_id: None,
                refresh,
            }),
        )
    }
    /// Capabilities lists the catalogue's models with their tags and effort per tier, each
    /// profile's usage (never its account), the vocabulary and the effort policy.
    #[test]
    fn capabilities_carry_models_tags_and_usage() {
        use crate::model_tags_file::TagsState;
        let tags = agent_core::model_tags::parse(
            r#"{"version": 1, "vocabulary": {"code": "writes code"},
                "models": [{"match": "*opus*", "tier": "deep", "tags": ["code"]}],
                "effort_policy": {"ladder": ["low", "medium", "high"], "fast": "low",
                                  "standard": "medium", "deep": "high", "reserved": []}}"#,
        )
        .unwrap();
        let tags = std::sync::Arc::new(tags);
        TEST_CATALOG.with(|c| {
            *c.borrow_mut() = Some(super::super::services::CatalogServices {
                models: Rc::new(|driver| {
                    vec![agent_core::catalog::CatalogModel {
                        driver,
                        id: "opus".into(),
                        display: "Opus".into(),
                        description: None,
                        efforts: vec!["low".into(), "high".into()],
                        default_effort: None,
                        via: None,
                    }]
                }),
                usage: Rc::new(|_| {
                    Some(crate::account_status::Snapshot {
                        account: Some(agent_core::event::Account {
                            label: "person@example.test".into(),
                            plan: None,
                            provider: None,
                        }),
                        windows: vec![agent_core::event::QuotaWindow {
                            group: None,
                            label: "5-hour".into(),
                            used: 0.85,
                            resets_at: None,
                        }],
                        read_at: Some(crate::freshness::now_epoch()),
                        ..Default::default()
                    })
                }),
                tags: Rc::new(move || -> super::super::seam::LocalFuture<TagsState> {
                    let tags = tags.clone();
                    Box::pin(async move {
                        TagsState {
                            tags: Some(tags),
                            status: None,
                        }
                    })
                }),
                refresh_usage: Rc::new(|_: Duration| -> super::super::seam::LocalFuture<()> {
                    Box::pin(std::future::ready(()))
                }),
            })
        });
        in_context(async {
            let (_dir, sup, root) = harness();
            connect(&sup, &root, ConnId(31)).await;
            let caps = match sup.call(ConnId(31), caps_call(1, true)).await {
                agent_core::delegation::ipc::CallOutcome::Ok(v) => v,
                agent_core::delegation::ipc::CallOutcome::Err(e) => panic!("{e:?}"),
            };
            let p = &caps["profiles"][0];
            assert_eq!(p["models"][0]["id"], "opus");
            assert_eq!(p["models"][0]["tags"], serde_json::json!(["code"]));
            assert_eq!(p["models"][0]["tier"], "deep");
            assert_eq!(
                p["models"][0]["effort_for"],
                serde_json::json!({"fast": "low", "standard": "low", "deep": "high"})
            );
            assert_eq!(p["usage"]["headroom"], "low");
            assert_eq!(p["usage"]["load"], 0.85);
            assert_eq!(caps["tag_vocabulary"]["code"], "writes code");
            assert_eq!(caps["effort_policy"]["deep"], "high");
            assert!(caps.get("tags_status").is_none());
            assert!(!caps.to_string().contains("person@example.test"));
        });
    }
    /// A capability revoked while capabilities awaited (the usage refresh) gets no answer.
    #[test]
    fn capabilities_recheck_the_caller_after_their_awaits() {
        let sup_slot: Rc<RefCell<Option<std::rc::Weak<Supervisor>>>> = Rc::default();
        let slot = sup_slot.clone();
        TEST_CATALOG.with(|c| {
            *c.borrow_mut() = Some(super::super::services::CatalogServices {
                refresh_usage: Rc::new(move |_: Duration| -> super::super::seam::LocalFuture<()> {
                    // Revoke every live capability mid-call.
                    if let Some(sup) = slot.borrow().as_ref().and_then(|w| w.upgrade()) {
                        for w in sup.state.borrow_mut().workers.values_mut() {
                            if let Some(t) = w.token.as_mut() {
                                t.live_conn = None;
                            }
                        }
                    }
                    Box::pin(std::future::ready(()))
                }),
                ..Default::default()
            })
        });
        in_context(async {
            let (_dir, sup, root) = harness();
            *sup_slot.borrow_mut() = Some(Rc::downgrade(&sup));
            connect(&sup, &root, ConnId(32)).await;
            // Without refresh, the call is answered.
            assert!(matches!(
                sup.call(ConnId(32), caps_call(1, false)).await,
                agent_core::delegation::ipc::CallOutcome::Ok(_)
            ));
            match sup.call(ConnId(32), caps_call(2, true)).await {
                agent_core::delegation::ipc::CallOutcome::Err(e) => {
                    assert_eq!(e.code, ErrorCode::AuthenticationFailed, "{e:?}")
                }
                agent_core::delegation::ipc::CallOutcome::Ok(v) => {
                    panic!("answered a revoked caller: {v}")
                }
            }
        });
    }
    /// `command` picks the fixture profile's driver; `hook_proven` is agy's approval hook.
    fn harness_with(
        command: &str,
        hook_proven: bool,
    ) -> (tempfile::TempDir, Rc<Supervisor>, WorkerId) {
        let dir = tempfile::tempdir().expect("private directory");
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
        }
        let store = Rc::new(Store::open_in_memory().expect("store"));
        let profile = Profile {
            id: "fixture".into(),
            name: "Fixture".into(),
            command: command.into(),
            available_for_delegation: true,
            ..Default::default()
        };
        let profiles = vec![profile.clone()];
        let cwd = dir.path().to_owned();
        let switch = cwd.clone();
        // An `opted-out` file turns delegation off for the fixture profile until it is removed.
        let profiles = move || {
            let mut profiles = profiles.clone();
            if switch.join("opted-out").exists() {
                for p in &mut profiles {
                    p.available_for_delegation = false;
                }
            }
            profiles
        };
        let services = AppServices {
            handle: store.clone().into(),
            store_path: None,
            app_pid: std::process::id(),
            bridge_command: PathBuf::from("/usr/bin/agent-terminal"),
            profiles: Rc::new(profiles),
            resolve: Rc::new(move |_| {
                if cwd.join("unavailable").exists() {
                    return None;
                }
                if cwd.join("fail-resolve-after-first").exists() {
                    std::fs::write(cwd.join("unavailable"), "").unwrap();
                }
                Some(super::super::seam::ResolvedChild {
                    program: "/usr/bin/python3".into(),
                    extra_args: vec![
                        "-I".into(),
                        format!(
                            "{}/tests/support/scripted_claude.py",
                            env!("CARGO_MANIFEST_DIR")
                        ),
                        cwd.to_string_lossy().into_owned(),
                    ],
                    env: crate::agent_proc::AgentEnv {
                        unset: std::env::vars_os()
                            .map(|(k, _)| k.to_string_lossy().into_owned())
                            .collect(),
                        env: vec![
                            ("HOME".into(), cwd.to_string_lossy().into_owned()),
                            ("PATH".into(), "/usr/bin:/bin".into()),
                        ],
                    },
                    default_model: Some("synthetic-model".into()),
                    default_effort: None,
                })
            }),
            make_adapter: Rc::new(|driver, program| -> Box<dyn agent_core::adapter::Adapter> {
                match driver {
                    Driver::Agy => Box::new(agent_core::agy::AgyAdapter::new(program)),
                    _ => Box::new(agent_core::claude::ClaudeAdapter::new()),
                }
            }),
            hook_check: Rc::new(move |_| {
                Box::pin(async move {
                    if hook_proven {
                        Ok(())
                    } else {
                        Err("not agy".into())
                    }
                })
            }),
            tab_open_in: Rc::new(|_| false),
            report: Rc::new(|e| eprintln!("delegation test: {e}")),
            catalog: TEST_CATALOG
                .with(|c| c.borrow_mut().take())
                .unwrap_or_default(),
        };
        let runtime_dir = RuntimeDir::from_verified(dir.path().to_owned());
        let sup = Rc::new_cyclic(|weak| Supervisor {
            services,
            limits: Limits::default(),
            routes: Rc::new(Verified),
            state: RefCell::new(Runtime::default()),
            ipc: RefCell::new(None),
            rundir: runtime_dir,
            instance: InstanceNonce::from_bytes([7; 16]),
            salt: [8; 16],
            weak: weak.clone(),
        });
        let (_, root) = insert_root(&sup, dir.path(), "root", "root", Some(Mode::Ask));
        (dir, sup, root)
    }
    /// Another thread with its own delegation root, beside the harness's: (thread, root).
    fn add_root(sup: &Rc<Supervisor>, dir: &std::path::Path) -> (String, WorkerId) {
        insert_root(sup, dir, "second root", "second root", Some(Mode::Ask))
    }
    /// A new thread titled `title` with a delegation root `name` in `dir`, registered with the
    /// supervisor: (thread, root). `mode` is the root's runtime mode, if one is set.
    fn insert_root(
        sup: &Rc<Supervisor>,
        dir: &std::path::Path,
        title: &str,
        name: &str,
        mode: Option<Mode>,
    ) -> (String, WorkerId) {
        let store = sup.services.handle.reader();
        let thread = store
            .create_thread(&dir.to_string_lossy(), Some(title))
            .expect("thread");
        let root = store
            .create_delegation_root(&NewRoot {
                thread: thread.clone(),
                driver: Driver::Claude,
                profile_id: ProfileId::parse("fixture").unwrap(),
                name: name.into(),
                policy_json: "{}".into(),
            })
            .expect("root");
        let row = store.delegation_root(&root.root).unwrap().unwrap();
        let worker = store
            .delegation_worker(root.worker.as_str())
            .unwrap()
            .unwrap();
        let mut rt = sup.worker_rt(worker, root.worker.clone(), dir.to_owned());
        if let Some(mode) = mode {
            rt.mode = mode;
        }
        let mut state = sup.state.borrow_mut();
        state.roots.insert(root.worker.clone(), RootRt::new(row));
        state.threads.insert(thread.clone(), root.worker.clone());
        state.workers.insert(root.worker.clone(), rt);
        (thread, root.worker)
    }
    fn spawn_request(key: &str) -> SpawnRequest {
        SpawnRequest {
            profile_id: ProfileId::parse("fixture").unwrap(),
            name: "worker".into(),
            task: "do the task".into(),
            task_mode: TaskMode::Plan,
            model: Some("synthetic-model".into()),
            effort: None,
            workspace: WorkspaceRequest::SharedReadOnly,
            expected_authority_epoch: None,
            idempotency_key: IdempotencyKey::parse(key).unwrap(),
        }
    }
    fn in_context(f: impl std::future::Future<Output = ()>) {
        let context = glib::MainContext::new();
        context
            .with_thread_default(|| context.block_on(f))
            .expect("main context");
    }
    /// Waits for `check`, failing with the caller's line if it never holds. The budget is twice
    /// the agent stop path's own worst case: many checks wait for a stop, and one that took the
    /// SIGTERM-then-SIGKILL fallback (seen on loaded CI runners) is slow, not hung.
    #[track_caller]
    fn pump_until(mut check: impl FnMut() -> bool) -> impl std::future::Future<Output = ()> {
        let at = std::panic::Location::caller();
        let budget = crate::agent_proc::STOP_WORST_CASE * 2;
        async move {
            let start = Instant::now();
            while !check() {
                assert!(
                    start.elapsed() < budget,
                    "condition at {}:{} did not hold within {budget:?}",
                    at.file(),
                    at.line()
                );
                glib::timeout_future(Duration::from_millis(10)).await;
            }
        }
    }
    async fn stop(sup: &Rc<Supervisor>) {
        let stops = sup.shutdown();
        for stop in stops {
            super::super::capacity::wait_exit(&stop.watch).await;
        }
        glib::timeout_future(Duration::from_millis(20)).await;
    }
    async fn connect(sup: &Rc<Supervisor>, worker: &WorkerId, conn: ConnId) {
        let cap = CapabilityToken::from_random([conn.0 as u8; 32]);
        let epoch = sup.epoch(worker).unwrap();
        {
            let mut state = sup.state.borrow_mut();
            let w = state.workers.get_mut(worker).unwrap();
            let generation = w.generation;
            w.token = Some(TokenSlot::new(
                epoch,
                generation,
                cap.clone(),
                Some(std::process::id()),
            ));
        }
        sup.authenticate(
            conn,
            Hello {
                api: ApiRange { min: 1, max: 1 },
                instance: sup.instance.clone(),
                cap,
                bridge: BridgeInfo {
                    version: "test".into(),
                    pid: std::process::id(),
                },
            },
            PeerFacts::default(),
        )
        .await
        .unwrap();
    }
    async fn tool(sup: &Rc<Supervisor>, conn: ConnId, id: u64, request: ToolRequest) -> Value {
        match sup.call(conn, call(id, request)).await {
            agent_core::delegation::ipc::CallOutcome::Ok(v) => v,
            agent_core::delegation::ipc::CallOutcome::Err(e) => panic!("tool failed: {e:?}"),
        }
    }
    /// The error a call that must fail returns.
    async fn call_err(
        sup: &Rc<Supervisor>,
        conn: ConnId,
        id: u64,
        request: ToolRequest,
    ) -> ToolError {
        match sup.call(conn, call(id, request)).await {
            agent_core::delegation::ipc::CallOutcome::Ok(v) => panic!("tool succeeded: {v}"),
            agent_core::delegation::ipc::CallOutcome::Err(e) => e,
        }
    }
    /// The worker a spawn receipt names.
    fn spawned(receipt: &Value) -> WorkerId {
        WorkerId::parse(receipt["worker_id"].as_str().unwrap()).unwrap()
    }
    /// Registers the agent-terminal bridge with agy under `home`, as Settings does.
    fn configure_agy(sup: &Rc<Supervisor>, home: &std::path::Path) {
        let state = home.join(".config/agent-terminal/delegation");
        let preview = agent_kit::delegation_config::preview(
            home,
            &state,
            &sup.services.bridge_command,
            agent_kit::delegation_config::Action::Configure,
        )
        .unwrap();
        agent_kit::delegation_config::apply(preview).unwrap();
    }
    /// A run's stored state, `None` before its row exists.
    fn run_state(sup: &Rc<Supervisor>, run: &str) -> Option<String> {
        sup.services
            .handle
            .reader()
            .delegation_run(run)
            .unwrap()
            .map(|r| r.state)
    }
    /// A message to `target` with no run or epoch expectation.
    fn send_request(
        target: Target,
        text: &str,
        kind: agent_core::delegation::mailbox::MessageKind,
        key: &str,
    ) -> SendRequest {
        SendRequest {
            target,
            text: text.into(),
            kind,
            reply_to: None,
            delivery: Delivery::NextTurn,
            expected_run_id: None,
            expected_authority_epoch: None,
            idempotency_key: IdempotencyKey::parse(key).unwrap(),
        }
    }
    /// An instruction to `worker` for its next turn.
    fn instruction(worker: &WorkerId, text: &str, key: &str) -> SendRequest {
        send_request(
            Target::Worker(worker.clone()),
            text,
            agent_core::delegation::mailbox::MessageKind::Instruction,
            key,
        )
    }
    /// A child's report to its parent.
    fn report_to_parent(text: &str, key: &str) -> SendRequest {
        send_request(
            Target::Parent,
            text,
            agent_core::delegation::mailbox::MessageKind::Report,
            key,
        )
    }
    /// A cancel of `worker` with no run or epoch expectation.
    fn cancel_request(worker: &WorkerId, key: &str) -> CancelRequest {
        CancelRequest {
            worker_id: worker.clone(),
            expected_run_id: None,
            expected_authority_epoch: None,
            idempotency_key: IdempotencyKey::parse(key).unwrap(),
        }
    }
    #[test]
    fn authenticated_tools_cover_catalogue_paging_wait_mailbox_and_close() {
        in_context(async {
            let (_dir, sup, root) = harness();
            connect(&sup, &root, ConnId(11)).await;
            let caps = tool(
                &sup,
                ConnId(11),
                1,
                ToolRequest::Capabilities(Default::default()),
            )
            .await;
            assert_eq!(caps["profiles"][0]["available"], true);
            let receipt = tool(
                &sup,
                ConnId(11),
                2,
                ToolRequest::Spawn(spawn_request("tools-spawn")),
            )
            .await;
            let worker = spawned(&receipt);
            pump_until(|| sup.state.borrow().workers[&worker].run.is_none()).await;
            let listed = tool(&sup, ConnId(11), 3, ToolRequest::List(Default::default())).await;
            assert!(listed.to_string().contains(worker.as_str()));
            let read = tool(
                &sup,
                ConnId(11),
                4,
                ToolRequest::Read(ReadRequest {
                    target: Some(worker.clone()),
                    ..Default::default()
                }),
            )
            .await;
            assert!(!read["events"].as_array().unwrap().is_empty());
            let waited = tool(
                &sup,
                ConnId(11),
                5,
                ToolRequest::Wait(WaitRequest {
                    workers: Some(vec![worker.clone()]),
                    timeout_ms: Some(1),
                    ..Default::default()
                }),
            )
            .await;
            assert!(!waited["events"].as_array().unwrap().is_empty());
            connect(&sup, &worker, ConnId(12)).await;
            let report = tool(
                &sup,
                ConnId(12),
                6,
                ToolRequest::Send(report_to_parent("status report", "child-report")),
            )
            .await;
            let inbox = tool(&sup, ConnId(11), 7, ToolRequest::Read(Default::default())).await;
            assert!(inbox.to_string().contains("status report"));
            let message =
                agent_core::delegation::MessageId::parse(report["message_id"].as_str().unwrap())
                    .unwrap();
            let ack = ReadRequest {
                acknowledge: Some(vec![message]),
                idempotency_key: Some(IdempotencyKey::parse("ack-report").unwrap()),
                ..Default::default()
            };
            tool(&sup, ConnId(11), 8, ToolRequest::Read(ack)).await;
            let cancel = tool(
                &sup,
                ConnId(11),
                9,
                ToolRequest::Cancel(cancel_request(&worker, "idle-cancel")),
            )
            .await;
            assert_eq!(cancel["state"], "idle");
            tool(
                &sup,
                ConnId(11),
                10,
                ToolRequest::Close(CloseRequest {
                    worker_id: worker.clone(),
                    expected_authority_epoch: None,
                    idempotency_key: IdempotencyKey::parse("tools-close").unwrap(),
                }),
            )
            .await;
            pump_until(|| sup.state.borrow().workers[&worker].generations.is_empty()).await;
            assert_eq!(sup.state.borrow().workers[&worker].row.state, "closed");
            stop(&sup).await;
        });
    }

    /// A capability is current only under its root's present epoch, and never once the root is
    /// gone or the token is.
    #[test]
    fn a_token_is_current_only_under_its_live_roots_epoch() {
        in_context(async {
            let (_dir, sup, root) = harness();
            connect(&sup, &root, ConnId(13)).await;
            let current = |sup: &Rc<Supervisor>| {
                let state = sup.state.borrow();
                state.token_current(&state.workers[&root])
            };
            assert!(current(&sup));
            let row = sup.state.borrow_mut().roots.remove(&root).unwrap();
            assert!(!current(&sup), "a missing root is stale, not a panic");
            sup.state.borrow_mut().roots.insert(root.clone(), row);
            sup.state
                .borrow_mut()
                .roots
                .get_mut(&root)
                .unwrap()
                .row
                .authority_epoch += 1;
            assert!(!current(&sup), "an older epoch is stale");
            sup.state.borrow_mut().workers.get_mut(&root).unwrap().token = None;
            assert!(!current(&sup));
        });
    }
    #[test]
    fn spawn_fake_child_runs_task_records_run_and_result() {
        in_context(async {
            let (dir, sup, root) = harness();
            let receipt = sup
                .spawn_worker(&root, spawn_request("first"))
                .await
                .expect("spawn");
            let worker = spawned(&receipt);
            pump_until(|| {
                let state = sup.state.borrow();
                let w = &state.workers[&worker];
                assert_ne!(
                    w.row.state, "failed",
                    "worker failed: {:?}",
                    w.row.delegation_reason
                );
                w.row.state == "completed"
            })
            .await;
            let frame_text = std::fs::read_to_string(dir.path().join("frames.ndjson")).unwrap();
            let input: Vec<Value> = frame_text
                .lines()
                .filter_map(|s| serde_json::from_str::<Value>(s).ok())
                .filter(|v| v["dir"] == "in" && v["frame"]["type"] == "user")
                .collect();
            assert_eq!(input.len(), 2);
            assert!(input[0]["frame"]["message"]["content"]
                .as_str()
                .unwrap()
                .contains("startup check"));
            assert!(input[1]["frame"]["message"]["content"]
                .as_str()
                .unwrap()
                .contains("do the task"));
            let store = sup.services.handle.reader();
            let run = store
                .delegation_run(receipt["run_id"].as_str().unwrap())
                .unwrap()
                .unwrap();
            assert_eq!(run.state, "completed");
            assert!(
                !sup.state.borrow().workers[&worker].generations.is_empty(),
                "idle child keeps process capacity"
            );
            let thread = sup.state.borrow().roots[&root].row.root_thread_id.clone();
            assert!(
                !sup.root_busy(&thread),
                "idle completed child must not keep root busy"
            );
            let replay = sup
                .spawn_worker(&root, spawn_request("first"))
                .await
                .unwrap();
            assert_eq!(receipt["worker_id"], replay["worker_id"]);
            assert_eq!(replay["replayed"], true);
            assert_eq!(sup.state.borrow().workers.len(), 2);
            stop(&sup).await;
        });
    }
    #[test]
    fn cancel_during_async_binding_preparation_prevents_later_process_start() {
        in_context(async {
            let (dir, sup, root) = harness();
            let mut spawn = Box::pin(sup.spawn_worker(&root, spawn_request("cancel-preparation")));
            std::future::poll_fn(|cx| {
                assert!(std::future::Future::poll(spawn.as_mut(), cx).is_pending());
                std::task::Poll::Ready(())
            })
            .await;
            let worker = sup
                .state
                .borrow()
                .workers
                .keys()
                .find(|id| *id != &root)
                .unwrap()
                .clone();
            assert!(sup.state.borrow().workers[&worker].session.is_none());
            sup.cancel_worker_call(&root, cancel_request(&worker, "cancel-reserved"))
                .await
                .unwrap();
            let receipt = spawn.await.unwrap();
            assert!(sup.state.borrow().workers[&worker].session.is_none());
            assert!(!dir.path().join("frames.ndjson").exists());
            pump_until(|| {
                run_state(&sup, receipt["run_id"].as_str().unwrap())
                    .is_some_and(|s| matches!(s.as_str(), "failed" | "interrupted"))
            })
            .await;
            stop(&sup).await;
        });
    }
    #[test]
    fn delayed_old_terminal_receipt_cannot_complete_replacement_run() {
        in_context(async {
            let (_dir, sup, root) = harness();
            let receipt = sup
                .spawn_worker(&root, spawn_request("old-terminal"))
                .await
                .unwrap();
            let worker = spawned(&receipt);
            pump_until(|| sup.state.borrow().workers[&worker].run.is_none()).await;
            let old = sup.scope(&worker).unwrap();
            let replacement = RunId::from_suffix(&"b".repeat(32)).unwrap();
            {
                let mut state = sup.state.borrow_mut();
                let w = state.workers.get_mut(&worker).unwrap();
                w.run = Some(replacement.clone());
                w.row.state = "running".into();
            }
            sup.on_child_envelope(
                &worker,
                old,
                agent_core::event::Envelope::new(agent_core::event::Event::TurnCompleted {
                    state: agent_core::event::TurnState::Completed,
                    usage: None,
                    error: None,
                    cost_usd: None,
                }),
                (999, None),
                false,
                None,
            );
            assert_eq!(
                sup.state.borrow().workers[&worker].run.as_ref(),
                Some(&replacement)
            );
            assert_eq!(sup.state.borrow().workers[&worker].row.state, "running");
            sup.state.borrow_mut().workers.get_mut(&worker).unwrap().run = None;
            stop(&sup).await;
        });
    }
    #[test]
    fn failed_fresh_environment_resolution_never_launches_ambient_process() {
        in_context(async {
            let (dir, sup, root) = harness();
            std::fs::write(dir.path().join("fail-resolve-after-first"), "").unwrap();
            let receipt = sup
                .spawn_worker(&root, spawn_request("resolve-failure"))
                .await
                .unwrap();
            let worker = spawned(&receipt);
            assert_eq!(sup.state.borrow().workers[&worker].row.state, "failed");
            assert!(sup.state.borrow().workers[&worker].generations.is_empty());
            assert!(!dir.path().join("frames.ndjson").exists());
            pump_until(|| {
                run_state(&sup, receipt["run_id"].as_str().unwrap()).is_some_and(|s| s == "failed")
            })
            .await;
            stop(&sup).await;
        });
    }
    #[test]
    fn provider_reported_model_is_sanitised_before_registry_and_storage() {
        in_context(async {
            let (dir, sup, root) = harness();
            let canary = format!("sk-{}", "z".repeat(48));
            std::fs::write(dir.path().join("reported-model"), &canary).unwrap();
            let receipt = sup
                .spawn_worker(&root, spawn_request("reported-model-canary"))
                .await
                .unwrap();
            let worker = spawned(&receipt);
            pump_until(|| sup.state.borrow().workers[&worker].run.is_none()).await;
            let row = sup
                .services
                .handle
                .reader()
                .delegation_worker(worker.as_str())
                .unwrap()
                .unwrap();
            assert!(!row.identity_json.unwrap().contains(&canary));
            {
                let runtime = sup.state.borrow();
                let summary = sup.summary(&worker, &runtime.workers[&worker]).unwrap();
                assert!(!serde_json::to_string(&summary).unwrap().contains(&canary));
            }
            stop(&sup).await;
        });
    }
    #[test]
    fn secret_model_is_rejected_before_acceptance_or_provider_launch() {
        in_context(async {
            let (dir, sup, root) = harness();
            let mut request = spawn_request("secret-model");
            request.model = Some(format!("sk-{}", "a".repeat(48)));
            assert_eq!(
                sup.spawn_worker(&root, request).await.unwrap_err().code,
                ErrorCode::InvalidArgument
            );
            assert_eq!(sup.state.borrow().workers.len(), 1);
            assert!(!dir.path().join("frames.ndjson").exists());
            stop(&sup).await;
        });
    }
    #[test]
    fn cancel_escalates_ignored_interrupt_and_waits_for_real_exit() {
        in_context(async {
            let (dir, sup, root) = harness();
            std::fs::write(dir.path().join("ignore-interrupt"), "").unwrap();
            let mut request = spawn_request("ignore-stop");
            request.task = "hold".into();
            let receipt = sup.spawn_worker(&root, request).await.unwrap();
            let worker = spawned(&receipt);
            pump_until(|| {
                sup.state.borrow().workers[&worker].pending.is_none()
                    && sup.state.borrow().workers[&worker].row.state == "running"
            })
            .await;
            let watch = sup.state.borrow().workers[&worker]
                .generations
                .values()
                .next()
                .unwrap()
                .watch
                .clone();
            sup.cancel_worker_call(&root, cancel_request(&worker, "cancel-ignore"))
                .await
                .unwrap();
            assert_ne!(watch.phase(), crate::agent_proc::StopPhase::Exited);
            pump_until(|| watch.phase() == crate::agent_proc::StopPhase::Exited).await;
            // The exit is observed in the same notification that reports it.
            assert!(sup.state.borrow().workers[&worker].generations.is_empty());
            // The run ends only once its interrupted turn has committed, a store round trip
            // that can land after the exit.
            pump_until(|| sup.state.borrow().workers[&worker].run.is_none()).await;
            stop(&sup).await;
        });
    }
    #[test]
    fn claude_bootstrap_tool_violation_latches_and_task_is_never_sent() {
        in_context(async {
            let (dir, sup, root) = harness();
            std::fs::write(dir.path().join("bootstrap-tool"), "").unwrap();
            let receipt = sup
                .spawn_worker(&root, spawn_request("violated"))
                .await
                .unwrap();
            let worker = spawned(&receipt);
            pump_until(|| sup.state.borrow().workers[&worker].row.state == "failed").await;
            let text = std::fs::read_to_string(dir.path().join("frames.ndjson")).unwrap();
            assert!(!text
                .lines()
                .filter_map(|s| serde_json::from_str::<Value>(s).ok())
                .any(|v| v["dir"] == "in"
                    && v["frame"]["type"] == "user"
                    && v["frame"]["message"]["content"]
                        .as_str()
                        .is_some_and(|s| s.contains("do the task"))));
            stop(&sup).await;
        });
    }
    #[test]
    fn agy_opted_out_unbound_root_can_launch_but_child_cannot() {
        in_context(async {
            let (dir, sup, root) = harness();
            let receipt = sup
                .spawn_worker(&root, spawn_request("optout-boundary"))
                .await
                .unwrap();
            let child = spawned(&receipt);
            pump_until(|| sup.state.borrow().workers[&child].run.is_none()).await;
            let facts = super::super::seam::DelegationLaunchFacts {
                generation: ProcessGeneration(1),
                argv: vec!["agy".into()],
                physical_cwd: dir.path().to_owned(),
                effective_home: None,
                config_redirect_vars: vec![],
                hook_proven: false,
            };
            sup.state
                .borrow_mut()
                .workers
                .get_mut(&root)
                .unwrap()
                .driver = Driver::Agy;
            let hooks = super::super::attest::Hooks {
                supervisor: Rc::downgrade(&sup),
                worker: root.clone(),
            };
            assert!(hooks.before_spawn(facts.clone()).await.is_err());
            sup.state
                .borrow_mut()
                .workers
                .get_mut(&root)
                .unwrap()
                .row
                .profile_id = "not-opted-in".into();
            assert!(hooks.before_spawn(facts.clone()).await.is_ok());
            {
                let mut state = sup.state.borrow_mut();
                let w = state.workers.get_mut(&child).unwrap();
                w.driver = Driver::Agy;
                w.row.profile_id = "not-opted-in".into();
                w.token = None;
            }
            let hooks = super::super::attest::Hooks {
                supervisor: Rc::downgrade(&sup),
                worker: child,
            };
            assert!(hooks.before_spawn(facts).await.is_err());
            stop(&sup).await;
        });
    }
    /// A launch that never reached a process releases what its binding issued: the bearer-token
    /// overlay file is removed and the never-presented token ends, and only its own token.
    #[test]
    fn an_abandoned_launch_removes_its_overlay_and_token() {
        in_context(async {
            let (dir, sup, root) = harness();
            let hooks = super::super::attest::Hooks {
                supervisor: Rc::downgrade(&sup),
                worker: root.clone(),
            };
            let plant = |name: &str| {
                let path = dir.path().join(name);
                std::fs::write(&path, "{}").unwrap();
                let epoch = sup.epoch(&root).unwrap();
                let mut state = sup.state.borrow_mut();
                let w = state.workers.get_mut(&root).unwrap();
                w.pending_overlay = Some(path.clone());
                w.token = Some(TokenSlot::new(
                    epoch,
                    ProcessGeneration(9),
                    CapabilityToken::from_random([9; 32]),
                    None,
                ));
                path
            };
            // Never spawned: the overlay goes and the token ends.
            let overlay = plant("abandoned.json");
            hooks.launch_abandoned(ProcessGeneration(9));
            pump_until(|| !overlay.exists()).await;
            assert!(sup.state.borrow().workers[&root].token.is_none());
            assert!(sup.state.borrow().workers[&root].pending_overlay.is_none());

            // A token issued for another generation is not this launch's to end.
            let overlay = plant("other.json");
            hooks.launch_abandoned(ProcessGeneration(8));
            pump_until(|| !overlay.exists()).await;
            assert!(
                sup.state.borrow().workers[&root].token.is_some(),
                "generation 9's token outlives generation 8's abandoned launch"
            );
            stop(&sup).await;
        });
    }

    #[test]
    fn an_unusable_agy_registration_starts_the_root_unbound_and_unavailable() {
        in_context(async {
            let (dir, sup, root) = harness();
            let profile = (sup.services.profiles)().remove(0);
            assert!(opted_in(&profile));
            // Never configured: an opted-in agy root cannot be bound, and says why.
            let problem = sup.agy_root_problem(&profile, Driver::Agy).await.unwrap();
            assert!(problem.contains("registration"), "{problem}");
            assert_eq!(sup.agy_root_problem(&profile, Driver::Claude).await, None);
            {
                let mut state = sup.state.borrow_mut();
                let w = state.workers.get_mut(&root).unwrap();
                w.driver = Driver::Agy;
                w.token = None;
                w.row.profile_id = profile.id.clone();
            }
            let facts = super::super::seam::DelegationLaunchFacts {
                generation: ProcessGeneration(1),
                argv: vec!["agy".into()],
                physical_cwd: dir.path().to_owned(),
                effective_home: None,
                config_redirect_vars: vec![],
                hook_proven: false,
            };
            let hooks = super::super::attest::Hooks {
                supervisor: Rc::downgrade(&sup),
                worker: root.clone(),
            };
            assert!(sup.profile_available(&root));
            assert!(hooks.before_spawn(facts.clone()).await.is_err());
            sup.state
                .borrow_mut()
                .workers
                .get_mut(&root)
                .unwrap()
                .registration_off = true;
            // Unbound: it launches, but is not offered for delegation and is never rebound.
            assert!(!sup.profile_available(&root));
            assert!(hooks.before_spawn(facts).await.is_ok());
            assert!(hooks.before_root_prompt());
            configure_agy(&sup, dir.path());
            assert_eq!(sup.agy_root_problem(&profile, Driver::Agy).await, None);
            stop(&sup).await;
        });
    }
    #[test]
    fn agy_live_checks_use_successful_actual_launch_facts_after_profile_resolution_changes() {
        in_context(async {
            let (dir, sup, root) = harness();
            configure_agy(&sup, dir.path());
            let thread = sup.state.borrow().roots[&root].row.root_thread_id.clone();
            let profile = (sup.services.profiles)().remove(0);
            let resolved = (sup.services.resolve)(&profile).unwrap();
            let hooks = sup
                .prepare_root(&thread, &profile, Driver::Agy)
                .await
                .unwrap();
            let provider = sup
                .services
                .handle
                .reader()
                .add_provider_thread(&thread, "agy", "default")
                .unwrap();
            let open = agent_core::adapter::OpenSession {
                program: resolved.program.clone(),
                extra_args: resolved.extra_args,
                cwd: dir.path().to_string_lossy().into_owned(),
                model: None,
                effort: None,
                mode: Mode::Plan,
                resume: None,
                new_session_id: None,
                approval_hook: false,
                delegation: None,
            };
            let session = ChatSession::with_options(
                Box::new(agent_core::agy::AgyAdapter::new(&resolved.program)),
                open,
                crate::chat::session::SessionStore {
                    handle: sup.services.handle.clone(),
                    provider: Some(Some(provider)),
                },
                thread.clone(),
                Rc::new(|_| {}),
                Err(None),
                resolved.env,
                super::super::seam::SessionOptions {
                    generation_base: sup.root_generation_base(&thread),
                    hooks: Some(hooks),
                    child: None,
                },
            );
            sup.attach_root_session(&thread, Rc::downgrade(&session));
            pump_until(|| session.delegation_launch_facts().is_some()).await;
            assert_eq!(
                session
                    .delegation_launch_facts()
                    .unwrap()
                    .effective_home
                    .as_deref(),
                Some(dir.path())
            );
            std::fs::write(dir.path().join("unavailable"), "").unwrap();
            assert!((sup.services.resolve)(&profile).is_none());
            sup.recheck_agy(&root).await.unwrap();
            std::fs::create_dir(dir.path().join(".agents")).unwrap();
            std::fs::write(dir.path().join(".agents/mcp_config.json"), "{}").unwrap();
            assert_eq!(
                sup.recheck_agy(&root).await.unwrap_err().code,
                ErrorCode::WorkspaceUntrusted
            );
            pump_until(|| sup.state.borrow().workers[&root].generations.is_empty()).await;
            assert!(sup.state.borrow().workers[&root].token.is_none());
            stop(&sup).await;
        });
    }
    #[test]
    fn agy_live_generation_without_successful_launch_facts_is_refused() {
        in_context(async {
            let (dir, sup, root) = harness();
            configure_agy(&sup, dir.path());
            let receipt = sup
                .spawn_worker(&root, spawn_request("agy-poststart-policy"))
                .await
                .unwrap();
            let worker = spawned(&receipt);
            pump_until(|| sup.state.borrow().workers[&worker].run.is_none()).await;
            assert_eq!(
                sup.recheck_agy(&worker).await.unwrap_err().code,
                ErrorCode::WorkspaceUntrusted
            );
            assert!(sup.state.borrow().workers[&worker].token.is_none());
            pump_until(|| sup.state.borrow().workers[&worker].generations.is_empty()).await;
            assert_eq!(
                sup.state.borrow().workers[&worker].row.delegation_status,
                "workspace_untrusted"
            );
            stop(&sup).await;
        });
    }
    #[test]
    fn agy_hello_expiry_ignores_consumed_and_rotated_credentials() {
        in_context(async {
            let (_dir, sup, root) = harness();
            let receipt = sup
                .spawn_worker(&root, spawn_request("hello-expiry"))
                .await
                .unwrap();
            let worker = spawned(&receipt);
            pump_until(|| sup.state.borrow().workers[&worker].run.is_none()).await;
            let generation = sup.state.borrow().workers[&worker].generation;
            let old = sup.state.borrow().workers[&worker]
                .token
                .as_ref()
                .unwrap()
                .token
                .clone();
            sup.arm_agy_hello_window(&worker, generation, old, Duration::from_millis(5));
            {
                let mut state = sup.state.borrow_mut();
                let token = state
                    .workers
                    .get_mut(&worker)
                    .unwrap()
                    .token
                    .as_mut()
                    .unwrap();
                token.token = CapabilityToken::from_random([99; 32]);
                token.consumed_by = Some(1);
            }
            let cap = sup.state.borrow().workers[&worker]
                .token
                .as_ref()
                .unwrap()
                .token
                .clone();
            sup.arm_agy_hello_window(&worker, generation, cap.clone(), Duration::from_millis(5));
            glib::timeout_future(Duration::from_millis(20)).await;
            assert!(sup.state.borrow().workers[&worker].token.is_some());
            sup.state
                .borrow_mut()
                .workers
                .get_mut(&worker)
                .unwrap()
                .token
                .as_mut()
                .unwrap()
                .consumed_by = None;
            sup.arm_agy_hello_window(&worker, generation, cap, Duration::from_millis(5));
            pump_until(|| sup.state.borrow().workers[&worker].token.is_none()).await;
            pump_until(|| sup.state.borrow().workers[&worker].generations.is_empty()).await;
            assert_eq!(
                sup.state.borrow().workers[&worker].row.delegation_status,
                "rebind_required"
            );
            stop(&sup).await;
        });
    }
    #[test]
    fn agy_managed_registration_requires_exact_entry_and_allow_rules() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
        let home = dir.path();
        let command = std::path::Path::new("/usr/bin/agent-terminal");
        let state = home.join(".config/agent-terminal/delegation");
        assert_eq!(
            super::super::ops::managed_registration_ready(home, command)
                .unwrap_err()
                .code,
            ErrorCode::WorkspaceUntrusted
        );
        let preview = agent_kit::delegation_config::preview(
            home,
            &state,
            command,
            agent_kit::delegation_config::Action::Configure,
        )
        .unwrap();
        agent_kit::delegation_config::apply(preview).unwrap();
        super::super::ops::managed_registration_ready(home, command).unwrap();
        let path = home.join(".gemini/config/mcp_config.json");
        let mut configured: Value = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        configured["mcpServers"]["agent_terminal"]["command"] =
            serde_json::json!("/tmp/other-bridge");
        std::fs::write(&path, serde_json::to_vec(&configured).unwrap()).unwrap();
        assert_eq!(
            super::super::ops::managed_registration_ready(home, command)
                .unwrap_err()
                .code,
            ErrorCode::WorkspaceUntrusted
        );
    }
    #[test]
    fn unregistered_agy_is_unavailable_and_a_refused_child_tells_its_parent_why() {
        in_context(async {
            let (dir, sup, root) = harness_with("agy", true);
            connect(&sup, &root, ConnId(21)).await;
            let caps = tool(
                &sup,
                ConnId(21),
                1,
                ToolRequest::Capabilities(Default::default()),
            )
            .await;
            assert_eq!(caps["profiles"][0]["available"], false);
            assert!(caps["profiles"][0]["reason"]
                .as_str()
                .unwrap()
                .contains("registration"));
            let receipt = sup
                .spawn_worker(&root, spawn_request("agy-unregistered"))
                .await
                .unwrap();
            let worker = spawned(&receipt);
            pump_until(|| sup.state.borrow().workers[&worker].row.state == "failed").await;
            assert_eq!(
                sup.state.borrow().workers[&worker].row.delegation_status,
                "workspace_untrusted"
            );
            // A failed child revokes its root's authority; read once that has settled.
            pump_until(|| !sup.state.borrow().roots[&root].revoking).await;
            connect(&sup, &root, ConnId(22)).await;
            // The parent's unfiltered page names the child, not the parent, and says why.
            let start = Instant::now();
            let mut id = 2;
            let failure = loop {
                let page = tool(&sup, ConnId(22), id, ToolRequest::Read(Default::default())).await;
                id += 1;
                if let Some(e) = page["events"].as_array().unwrap().iter().find(|e| {
                    e["summary"]
                        .as_str()
                        .is_some_and(|s| s.starts_with("worker failed:"))
                }) {
                    break e.clone();
                }
                assert!(start.elapsed() < Duration::from_secs(5), "no failure event");
                glib::timeout_future(Duration::from_millis(10)).await;
            };
            assert_eq!(failure["worker_id"], worker.as_str());
            assert!(failure["summary"]
                .as_str()
                .unwrap()
                .contains("registration"));
            configure_agy(&sup, dir.path());
            let caps = tool(
                &sup,
                ConnId(22),
                id,
                ToolRequest::Capabilities(Default::default()),
            )
            .await;
            assert_eq!(caps["profiles"][0]["available"], true);
            stop(&sup).await;
        });
    }
    #[test]
    fn authentication_binds_one_connection_and_rejects_stale_capabilities() {
        in_context(async {
            let (_dir, sup, root) = harness();
            let cap = CapabilityToken::from_random([4; 32]);
            sup.state.borrow_mut().workers.get_mut(&root).unwrap().token = Some(TokenSlot::new(
                AuthorityEpoch(0),
                ProcessGeneration(1),
                cap.clone(),
                Some(std::process::id()),
            ));
            let hello = Hello {
                api: ApiRange { min: 1, max: 1 },
                instance: sup.instance.clone(),
                cap,
                bridge: BridgeInfo {
                    version: "test".into(),
                    pid: std::process::id(),
                },
            };
            let welcome = sup
                .authenticate(ConnId(1), hello.clone(), PeerFacts::default())
                .await
                .unwrap();
            assert_eq!(welcome.principal.worker_id, root);
            assert_eq!(
                sup.authenticate(ConnId(2), hello.clone(), PeerFacts::default())
                    .await
                    .unwrap_err()
                    .code,
                ErrorCode::AuthenticationFailed
            );
            sup.closed(ConnId(1));
            assert!(sup
                .authenticate(ConnId(3), hello.clone(), PeerFacts::default())
                .await
                .is_ok());
            sup.state.borrow_mut().workers.get_mut(&root).unwrap().token = None;
            assert_eq!(
                sup.authenticate(ConnId(4), hello, PeerFacts::default())
                    .await
                    .unwrap_err()
                    .code,
                ErrorCode::AuthenticationFailed
            );
        });
    }
    #[test]
    fn dead_ipc_rebind_rotates_credentials_without_revoking_accepted_followup() {
        in_context(async {
            let (_dir, sup, root) = harness();
            let receipt = sup
                .spawn_worker(&root, spawn_request("rebind-initial"))
                .await
                .unwrap();
            let worker = spawned(&receipt);
            pump_until(|| sup.state.borrow().workers[&worker].run.is_none()).await;
            let epoch = sup.epoch(&root).unwrap();
            let generation = sup.state.borrow().workers[&worker].generation;
            let original = sup.state.borrow().workers[&worker]
                .token
                .as_ref()
                .unwrap()
                .token
                .clone();
            sup.set_status(&worker, "rebind_required", "fixture lost IPC");
            let request = SendRequest {
                expected_authority_epoch: Some(epoch.0),
                ..instruction(&worker, "after rebind", "rebind-followup")
            };
            let send = sup.send_worker(&root, request).await.unwrap();
            pump_until(|| {
                sup.services
                    .handle
                    .reader()
                    .delegation_message(send["message_id"].as_str().unwrap())
                    .unwrap()
                    .is_some_and(|m| m.delivery_state == "provider_confirmed")
            })
            .await;
            pump_until(|| sup.state.borrow().workers[&worker].run.is_none()).await;
            assert_eq!(sup.epoch(&root).unwrap(), epoch);
            assert!(sup.state.borrow().workers[&worker].generation > generation);
            assert!(!sup.state.borrow().workers[&worker]
                .token
                .as_ref()
                .unwrap()
                .token
                .ct_eq(original.expose_secret()));
            stop(&sup).await;
        });
    }
    #[test]
    fn queued_followup_is_reserved_once_and_idempotent_retry_starts_no_second_run() {
        in_context(async {
            let (_dir, sup, root) = harness();
            let receipt = sup
                .spawn_worker(&root, spawn_request("start-followup"))
                .await
                .unwrap();
            let worker = spawned(&receipt);
            pump_until(|| sup.state.borrow().workers[&worker].row.state == "completed").await;
            let request = instruction(&worker, "follow-up", "followup");
            let first = sup.send_worker(&root, request.clone()).await.unwrap();
            pump_until(|| {
                sup.services
                    .handle
                    .reader()
                    .delegation_message(first["message_id"].as_str().unwrap())
                    .unwrap()
                    .is_some_and(|m| m.delivery_state == "provider_confirmed")
            })
            .await;
            pump_until(|| sup.state.borrow().workers[&worker].row.state == "completed").await;
            let run = sup.state.borrow().workers[&worker]
                .last_run
                .clone()
                .unwrap();
            assert_ne!(Some(run.as_str()), receipt["run_id"].as_str());
            let replay = sup.send_worker(&root, request).await.unwrap();
            assert_eq!(first["message_id"], replay["message_id"]);
            assert_eq!(
                sup.state.borrow().workers[&worker].last_run.as_ref(),
                Some(&run)
            );
            stop(&sup).await;
        });
    }
    #[test]
    fn root_stop_revokes_queued_delivery_and_keeps_slots_until_observed_exit() {
        in_context(async {
            let (dir, sup, root) = harness();
            let mut req = spawn_request("hold-start");
            req.task = "hold".into();
            let receipt = sup.spawn_worker(&root, req).await.unwrap();
            let worker = spawned(&receipt);
            pump_until(|| sup.state.borrow().workers[&worker].row.state == "running").await;
            let request = instruction(&worker, "MUST_NOT_RUN", "queued-before-stop");
            let send = sup.send_worker(&root, request).await.unwrap();
            let thread = sup.state.borrow().roots[&root].row.root_thread_id.clone();
            sup.root_stop(&thread);
            assert!(
                sup.usage(&root).live_processes >= 1,
                "synthetic stop released a process slot"
            );
            assert!(sup.state.borrow().workers[&worker].token.is_none());
            pump_until(|| sup.usage(&root).live_processes == 0).await;
            let text = std::fs::read_to_string(dir.path().join("frames.ndjson")).unwrap();
            assert!(!text.contains("MUST_NOT_RUN"));
            let message = sup
                .services
                .handle
                .reader()
                .delegation_message(send["message_id"].as_str().unwrap())
                .unwrap()
                .unwrap();
            assert_eq!(message.delivery_state, "cancelled");
        });
    }
    struct Reentrant {
        sup: Weak<Supervisor>,
        thread: String,
        count: std::cell::Cell<usize>,
    }
    impl WorkerSubscriber for Reentrant {
        fn committed(&self, env: &agent_core::delegation::scope::ScopedEnvelope, seq: i64) {
            let sup = self.sup.upgrade().unwrap();
            let thread = sup.state.borrow().workers[&env.scope.worker]
                .row
                .backing_thread_id
                .clone()
                .unwrap();
            let stored = sup
                .services
                .handle
                .reader()
                .events(&thread, Some(seq - 1), 1)
                .unwrap();
            assert_eq!(stored[0].0, seq);
            assert_eq!(stored[0].1, env.envelope);
            self.scoped(env);
        }
        fn scoped(&self, env: &agent_core::delegation::scope::ScopedEnvelope) {
            let sup = self.sup.upgrade().unwrap();
            let _ = sup.scope_is_current(&env.scope);
            let _ = sup.child_workspace(&env.scope.worker);
            let _ = sup.root_busy(&self.thread);
            self.count.set(self.count.get() + 1);
        }
        fn registry(&self, thread: &str, workers: &[WorkerSummary]) {
            let sup = self.sup.upgrade().unwrap();
            let _ = sup.root_busy(thread);
            for worker in workers {
                let _ = sup.child_workspace(&worker.worker_id);
            }
        }
    }
    #[test]
    fn subscribers_can_reenter_runtime_during_registry_and_scoped_publication() {
        in_context(async {
            let (_dir, sup, root) = harness();
            let thread = sup.state.borrow().roots[&root].row.root_thread_id.clone();
            let subscriber = Rc::new(Reentrant {
                sup: Rc::downgrade(&sup),
                thread: thread.clone(),
                count: std::cell::Cell::new(0),
            });
            let erased: Rc<dyn WorkerSubscriber> = subscriber.clone();
            sup.subscribe(&thread, Rc::downgrade(&erased));
            let receipt = sup
                .spawn_worker(&root, spawn_request("reentrant"))
                .await
                .unwrap();
            let worker = spawned(&receipt);
            pump_until(|| sup.state.borrow().workers[&worker].row.state == "completed").await;
            assert!(subscriber.count.get() > 2);
            stop(&sup).await;
        });
    }
    /// Records what a subscriber was told, in order: `None` for a replayed event, `Some`
    /// for a registry with the workers it listed.
    #[derive(Default)]
    struct Recorder {
        log: RefCell<Vec<Option<Vec<WorkerId>>>>,
    }
    impl WorkerSubscriber for Recorder {
        fn scoped(&self, _env: &agent_core::delegation::scope::ScopedEnvelope) {}
        fn replayed(&self, _env: &agent_core::delegation::scope::ScopedEnvelope) {
            self.log.borrow_mut().push(None);
        }
        fn registry(&self, _thread: &str, workers: &[WorkerSummary]) {
            self.log
                .borrow_mut()
                .push(Some(workers.iter().map(|w| w.worker_id.clone()).collect()));
        }
    }
    #[test]
    fn replay_for_an_unprepared_root_lists_its_stored_workers_first() {
        in_context(async {
            let (_dir, sup, root) = harness();
            let thread = sup.state.borrow().roots[&root].row.root_thread_id.clone();
            let live: Rc<dyn WorkerSubscriber> = Rc::new(Recorder::default());
            sup.subscribe(&thread, Rc::downgrade(&live));
            let receipt = sup
                .spawn_worker(&root, spawn_request("stored"))
                .await
                .unwrap();
            let worker = spawned(&receipt);
            pump_until(|| sup.state.borrow().workers[&worker].row.state == "completed").await;
            // As after a restart, before the thread's agent is prepared again: the in-memory
            // registry knows no workers for this thread.
            let mapped = sup.state.borrow_mut().threads.remove(&thread).unwrap();
            assert!(sup.summaries(&thread).is_empty());
            let recorder = Rc::new(Recorder::default());
            let erased: Rc<dyn WorkerSubscriber> = recorder.clone();
            sup.subscribe(&thread, Rc::downgrade(&erased));
            pump_until(|| recorder.log.borrow().iter().any(Option::is_none)).await;
            let log = recorder.log.take();
            let first_replay = log.iter().position(Option::is_none).unwrap();
            assert!(
                log[..first_replay]
                    .iter()
                    .flatten()
                    .any(|workers| workers.contains(&worker)),
                "the stored worker was listed before its history: {log:?}"
            );
            sup.state.borrow_mut().threads.insert(thread, mapped);
            stop(&sup).await;
        });
    }
    /// The app's `shutdown` handler runs outside any future, and the store writer closes right
    /// after it: the shutdown revocation must be committed before then, not left to GLib's flush.
    #[test]
    fn shutdown_revocations_commit_before_the_store_writer_closes() {
        let context = glib::MainContext::new();
        context
            .with_thread_default(|| {
                let (_dir, sup, root) = context.block_on(async {
                    let (dir, sup, root) = harness();
                    let receipt = sup
                        .spawn_worker(&root, spawn_request("shutdown"))
                        .await
                        .unwrap();
                    let worker = spawned(&receipt);
                    pump_until(|| sup.state.borrow().workers[&worker].run.is_none()).await;
                    (dir, sup, root)
                });
                let row = sup.state.borrow().roots[&root].row.clone();
                let stops = sup.shutdown();
                assert!(sup.state.borrow().revokes_in_flight > 0, "committed later");
                sup.settle_revokes(Duration::from_secs(10));
                assert_eq!(sup.state.borrow().revokes_in_flight, 0);
                let stored = sup
                    .services
                    .handle
                    .reader()
                    .delegation_root(&row.id)
                    .unwrap()
                    .unwrap();
                assert!(
                    stored.authority_epoch > row.authority_epoch,
                    "the AppShutdown revocation reached the store"
                );
                context.block_on(async {
                    for stop in stops {
                        super::super::capacity::wait_exit(&stop.watch).await;
                    }
                });
            })
            .expect("main context");
    }
    #[test]
    fn production_start_reconciles_crash_without_replaying_or_signalling_old_process() {
        const MARKER: &str = "AGENT_TERMINAL_RESTART_FIXTURE";
        if std::env::var_os(MARKER).is_none() {
            use std::os::unix::fs::PermissionsExt;
            let runtime = tempfile::tempdir().unwrap();
            std::fs::set_permissions(runtime.path(), std::fs::Permissions::from_mode(0o700))
                .unwrap();
            let result=std::process::Command::new(std::env::current_exe().unwrap()).args(["--exact","delegation::supervisor::tests::production_start_reconciles_crash_without_replaying_or_signalling_old_process","--nocapture"]).env(MARKER,"1").env("XDG_RUNTIME_DIR",runtime.path()).output().unwrap();
            assert!(
                result.status.success(),
                "isolated restart fixture failed: {}",
                String::from_utf8_lossy(&result.stderr)
            );
            return;
        }
        in_context(async {
            let (dir, old, root) = harness();
            let mut request = spawn_request("crash-persisted");
            request.task = "hold".into();
            let receipt = old.spawn_worker(&root, request.clone()).await.unwrap();
            let worker = spawned(&receipt);
            pump_until(|| old.state.borrow().workers[&worker].pending.is_none()).await;
            // The restart reconciles an operation still being dispatched to `unknown`, so
            // the crash is simulated only once the spawn is durably confirmed.
            let operation = receipt["operation_id"].as_str().unwrap().to_owned();
            pump_until(|| {
                old.services
                    .handle
                    .reader()
                    .delegation_operation(&operation)
                    .ok()
                    .flatten()
                    .is_some_and(|row| row.dispatch_state == "confirmed")
            })
            .await;
            let watch = old.state.borrow().workers[&worker]
                .generations
                .values()
                .next()
                .unwrap()
                .watch
                .clone();
            let before = std::fs::read(dir.path().join("frames.ndjson")).unwrap();
            let services = old.services.clone();
            let recovered = Supervisor::start(services, Limits::default(), Rc::new(Verified))
                .await
                .unwrap();
            assert_eq!(
                old.services
                    .handle
                    .reader()
                    .delegation_run(receipt["run_id"].as_str().unwrap())
                    .unwrap()
                    .unwrap()
                    .state,
                "unknown"
            );
            assert_ne!(
                watch.phase(),
                crate::agent_proc::StopPhase::Exited,
                "startup signalled the previously recorded process"
            );
            let thread = old.state.borrow().roots[&root].row.root_thread_id.clone();
            let profile = (old.services.profiles)().remove(0);
            assert!(recovered
                .prepare_root(&thread, &profile, Driver::Claude)
                .await
                .is_some());
            assert!(recovered.state.borrow().workers[&worker].session.is_none());
            assert!(recovered.state.borrow().workers[&worker].pending.is_none());
            assert!(recovered.state.borrow().child_specs.contains_key(&worker));
            assert_eq!(
                recovered.state.borrow().child_specs[&worker]
                    .resume
                    .as_deref(),
                Some("synthetic-session")
            );
            let replay = recovered.spawn_worker(&root, request).await.unwrap();
            assert_eq!(replay["replayed"], true);
            assert_eq!(replay["worker_id"], receipt["worker_id"]);
            glib::timeout_future(Duration::from_millis(30)).await;
            assert_eq!(
                std::fs::read(dir.path().join("frames.ndjson")).unwrap(),
                before
            );
            assert_ne!(watch.phase(), crate::agent_proc::StopPhase::Exited);
            stop(&recovered).await;
            stop(&old).await;
        });
    }
    #[test]
    fn stop_during_root_binding_preparation_cannot_install_late_credentials() {
        in_context(async {
            let (_dir, sup, root) = harness();
            let thread = sup.state.borrow().roots[&root].row.root_thread_id.clone();
            let profile = (sup.services.profiles)().remove(0);
            let mut preparing = Box::pin(sup.prepare_root(&thread, &profile, Driver::Claude));
            std::future::poll_fn(|cx| {
                assert!(std::future::Future::poll(preparing.as_mut(), cx).is_pending());
                std::task::Poll::Ready(())
            })
            .await;
            sup.root_stop(&thread);
            assert!(preparing.await.is_none());
            assert!(sup.state.borrow().workers[&root].staged.is_none());
            assert!(sup.state.borrow().workers[&root].token.is_none());
            stop(&sup).await;
        });
    }
    #[test]
    fn unavailable_child_profile_stops_its_subtree_and_rejects_later_delivery() {
        in_context(async {
            let (_dir, sup, root) = harness();
            let mut request = spawn_request("profile-stop");
            request.task = "hold".into();
            let receipt = sup.spawn_worker(&root, request).await.unwrap();
            let worker = spawned(&receipt);
            pump_until(|| sup.state.borrow().workers[&worker].pending.is_none()).await;
            sup.state
                .borrow_mut()
                .workers
                .get_mut(&worker)
                .unwrap()
                .row
                .profile_id = "removed-profile".into();
            sup.config_changed("removed-profile");
            assert!(sup.state.borrow().workers[&worker].token.is_none());
            pump_until(|| sup.state.borrow().workers[&worker].generations.is_empty()).await;
            pump_until(|| !sup.state.borrow().roots[&root].revoking).await;
            let request = instruction(&worker, "must not execute", "after-profile-removal");
            assert!(sup.send_worker(&root, request).await.is_err());
            assert_eq!(sup.state.borrow().workers[&worker].row.state, "closed");
            stop(&sup).await;
        });
    }
    #[test]
    fn publication_failure_revokes_and_stops_descendants_and_persists_failure() {
        in_context(async {
            let (_dir, sup, root) = harness();
            let parent_receipt = sup
                .spawn_worker(&root, spawn_request("failure-parent"))
                .await
                .unwrap();
            let parent = spawned(&parent_receipt);
            pump_until(|| sup.state.borrow().workers[&parent].run.is_none()).await;
            let mut request = spawn_request("failure-grandchild");
            request.task = "hold".into();
            let receipt = sup.spawn_worker(&parent, request).await.unwrap();
            let child = spawned(&receipt);
            pump_until(|| sup.state.borrow().workers[&child].pending.is_none()).await;
            sup.fail_worker(
                &parent,
                error(
                    ErrorCode::PersistenceFailed,
                    "synthetic committed-feed failure",
                ),
            );
            assert!(sup.state.borrow().workers[&child].token.is_none());
            pump_until(|| sup.state.borrow().workers[&child].generations.is_empty()).await;
            pump_until(|| {
                sup.services
                    .handle
                    .reader()
                    .delegation_worker(parent.as_str())
                    .unwrap()
                    .is_some_and(|w| w.state == "failed")
            })
            .await;
            pump_until(|| {
                run_state(&sup, receipt["run_id"].as_str().unwrap())
                    .is_some_and(|s| matches!(s.as_str(), "failed" | "interrupted" | "unknown"))
            })
            .await;
            stop(&sup).await;
        });
    }
    /// A descendant stopped after its task left `pending` but before its turn opened has no
    /// turn to end as interrupted; its run must still reach a final state, not stay open.
    #[test]
    fn a_descendant_stopped_before_its_turn_opens_still_ends_its_run() {
        in_context(async {
            let (dir, sup, root) = harness();
            let parent_receipt = sup
                .spawn_worker(&root, spawn_request("silent-parent"))
                .await
                .unwrap();
            let parent = spawned(&parent_receipt);
            pump_until(|| sup.state.borrow().workers[&parent].run.is_none()).await;
            std::fs::write(dir.path().join("silent-task"), "").unwrap();
            let receipt = sup
                .spawn_worker(&parent, spawn_request("silent-grandchild"))
                .await
                .unwrap();
            let child = spawned(&receipt);
            let run = receipt["run_id"].as_str().unwrap().to_owned();
            pump_until(|| sup.state.borrow().workers[&child].pending.is_none()).await;
            // Let the write reach the child, whose turn then never opens.
            pump_until(|| {
                std::fs::read_to_string(dir.path().join("frames.ndjson"))
                    .is_ok_and(|log| log.contains("do the task"))
            })
            .await;
            sup.fail_worker(
                &parent,
                error(ErrorCode::PersistenceFailed, "synthetic parent failure"),
            );
            pump_until(|| sup.state.borrow().workers[&child].generations.is_empty()).await;
            pump_until(|| {
                run_state(&sup, &run)
                    .is_some_and(|s| matches!(s.as_str(), "interrupted" | "failed"))
            })
            .await;
            assert!(sup.state.borrow().workers[&child].run.is_none());
            stop(&sup).await;
        });
    }
    /// Threads are fully isolated: two threads delegate at once, each to its own sub-agent, and
    /// stopping one (what a confirmed agent switch does) ends only its own sub-agent. App close
    /// then ends the rest.
    #[test]
    fn two_threads_delegate_at_once_and_stopping_one_never_touches_the_other() {
        in_context(async {
            let (dir, sup, root_a) = harness();
            let thread_a = sup.state.borrow().roots[&root_a].row.root_thread_id.clone();
            let (thread_b, root_b) = add_root(&sup, dir.path());
            let hold = |key: &str| {
                let mut request = spawn_request(key);
                request.task = "hold".into();
                request
            };
            let receipt_a = sup.spawn_worker(&root_a, hold("isolated-a")).await.unwrap();
            let receipt_b = sup.spawn_worker(&root_b, hold("isolated-b")).await.unwrap();
            let (child_a, child_b) = (spawned(&receipt_a), spawned(&receipt_b));
            pump_until(|| {
                let state = sup.state.borrow();
                [&child_a, &child_b].iter().all(|c| {
                    state.workers[*c].pending.is_none() && !state.workers[*c].generations.is_empty()
                })
            })
            .await;
            assert_eq!(sup.state.borrow().workers[&child_a].root, root_a);
            assert_eq!(sup.state.borrow().workers[&child_b].root, root_b);
            assert!(sup.root_has_live_children(&thread_a));
            assert!(sup.root_has_live_children(&thread_b));

            sup.root_stop(&thread_a);
            pump_until(|| sup.state.borrow().workers[&child_a].generations.is_empty()).await;
            pump_until(|| !sup.root_has_live_children(&thread_a)).await;
            {
                let state = sup.state.borrow();
                let b = &state.workers[&child_b];
                assert!(
                    !b.generations.is_empty(),
                    "the other thread's sub-agent still runs"
                );
                assert!(b.run.is_some(), "and is still on its task");
            }
            assert!(sup.root_has_live_children(&thread_b));
            let run_b = receipt_b["run_id"].as_str().unwrap();
            assert!(sup
                .services
                .handle
                .reader()
                .delegation_run(run_b)
                .unwrap()
                .is_some_and(|r| !matches!(r.state.as_str(), "interrupted" | "failed")));

            // App close: every thread's sub-agents end.
            let stops = sup.shutdown();
            for stop in stops {
                super::super::capacity::wait_exit(&stop.watch).await;
            }
            pump_until(|| sup.state.borrow().workers[&child_b].generations.is_empty()).await;
        });
    }
    /// A sub-agent that finished its task but is still up is not busy, yet an agent switch would
    /// end it: it counts as live, so the switch warns.
    #[test]
    fn an_idle_sub_agent_is_live_though_not_busy() {
        in_context(async {
            let (_dir, sup, root) = harness();
            let thread = sup.state.borrow().roots[&root].row.root_thread_id.clone();
            assert!(!sup.root_has_live_children(&thread), "no sub-agent yet");
            let receipt = sup
                .spawn_worker(&root, spawn_request("idle-child"))
                .await
                .unwrap();
            let child = spawned(&receipt);
            pump_until(|| sup.state.borrow().workers[&child].run.is_none()).await;
            assert!(!sup.state.borrow().workers[&child].generations.is_empty());
            assert!(!sup.root_busy(&thread));
            assert!(sup.root_has_live_children(&thread));
            stop(&sup).await;
        });
    }
    /// The same gap through revocation: Stop on the root ends a child whose turn never opened.
    #[test]
    fn root_stop_ends_the_run_of_a_child_whose_turn_never_opened() {
        in_context(async {
            let (dir, sup, root) = harness();
            std::fs::write(dir.path().join("silent-task"), "").unwrap();
            let receipt = sup
                .spawn_worker(&root, spawn_request("silent-child"))
                .await
                .unwrap();
            let child = spawned(&receipt);
            let run = receipt["run_id"].as_str().unwrap().to_owned();
            pump_until(|| {
                std::fs::read_to_string(dir.path().join("frames.ndjson"))
                    .is_ok_and(|log| log.contains("do the task"))
            })
            .await;
            let thread = sup.state.borrow().roots[&root].row.root_thread_id.clone();
            sup.root_stop(&thread);
            pump_until(|| sup.state.borrow().workers[&child].generations.is_empty()).await;
            pump_until(|| run_state(&sup, &run).is_some_and(|s| s == "interrupted")).await;
            stop(&sup).await;
        });
    }
    #[test]
    fn root_stop_rolls_back_tentative_provider_identity_without_touching_newer_ticket() {
        in_context(async {
            let (_dir, sup, root) = harness();
            let thread = sup.state.borrow().roots[&root].row.root_thread_id.clone();
            {
                let mut state = sup.state.borrow_mut();
                let w = state.workers.get_mut(&root).unwrap();
                w.switch_revision = 8;
                w.switch_original = Some((8, Driver::Claude, "fixture".into()));
                w.switch_registration_off = true;
                w.driver = Driver::Codex;
                w.row.driver = "codex".into();
                w.row.profile_id = "new-profile".into();
            }
            sup.rollback_root_switch(&root, Some(7));
            assert_eq!(sup.state.borrow().workers[&root].driver, Driver::Codex);
            assert!(!sup.state.borrow().workers[&root].registration_off);
            sup.root_stop(&thread);
            assert_eq!(sup.state.borrow().workers[&root].driver, Driver::Claude);
            assert!(
                sup.state.borrow().workers[&root].registration_off,
                "a rolled-back switch restores the unbound flag with the profile"
            );
            assert_eq!(sup.state.borrow().workers[&root].row.profile_id, "fixture");
            assert!(sup.state.borrow().workers[&root].switch_original.is_none());
            let stored = sup
                .services
                .handle
                .reader()
                .delegation_worker(root.as_str())
                .unwrap()
                .unwrap();
            assert_eq!(stored.driver, "claude");
            assert_eq!(stored.profile_id, "fixture");
            stop(&sup).await;
        });
    }
    #[test]
    fn unrelated_root_cannot_read_send_cancel_or_close_a_child() {
        in_context(async {
            let (dir, sup, root) = harness();
            let (_, other) = insert_root(&sup, dir.path(), "other root", "other", None);
            let receipt = sup
                .spawn_worker(&root, spawn_request("isolated"))
                .await
                .unwrap();
            let worker = spawned(&receipt);
            assert!(!sup.visible(&other, &worker, true));
            let close = CloseRequest {
                worker_id: worker,
                expected_authority_epoch: None,
                idempotency_key: IdempotencyKey::parse("foreign-close").unwrap(),
            };
            assert!(sup.close_worker_call(&other, close).await.is_err());
            stop(&sup).await;
        });
    }
    /// Worker status, run agreement, startup deadlines and paging errors (PR-E).
    mod status_tests;
    /// A root is told its sub-agents' results without polling for them.
    mod wake_tests;
}
pub(crate) struct P1Routes;
impl RouteTable for P1Routes {
    fn admit(&self, q: &RouteQuery) -> RouteSpec {
        admit_route(q)
    }
}
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum RootTurnGate {
    Proceed,
    Deferred,
}
pub(super) struct RootRt {
    pub row: RootRow,
    pub revoking: bool,
    pub revision: u64,
    pub closed: bool,
    pub transferring: bool,
}
impl RootRt {
    /// A root just registered: not revoking, open, not moving to another agent.
    pub fn new(row: RootRow) -> Self {
        Self {
            row,
            revoking: false,
            revision: 0,
            closed: false,
            transferring: false,
        }
    }
}
pub(super) struct GenRt {
    pub watch: ExitWatch,
    pub overlay: Option<PathBuf>,
}
pub(super) struct TokenSlot {
    pub epoch: AuthorityEpoch,
    pub generation: ProcessGeneration,
    pub token: CapabilityToken,
    pub consumed_by: Option<u32>,
    pub live_conn: Option<ConnId>,
    pub pid: Option<u32>,
    pub issued: Instant,
}
impl TokenSlot {
    /// A capability issued now, not yet presented by any connection.
    pub fn new(
        epoch: AuthorityEpoch,
        generation: ProcessGeneration,
        token: CapabilityToken,
        pid: Option<u32>,
    ) -> Self {
        Self {
            epoch,
            generation,
            token,
            consumed_by: None,
            live_conn: None,
            pid,
            issued: Instant::now(),
        }
    }
}
pub(super) struct WorkerRt {
    pub row: WorkerRow,
    pub root: WorkerId,
    pub driver: Driver,
    pub mode: Mode,
    pub generation: ProcessGeneration,
    pub rotating_generation: Option<ProcessGeneration>,
    pub preparing: bool,
    /// An opted-in agy root started unbound because its managed registration was unusable:
    /// treated as opted out (no staging, no rebind each turn) until it is next prepared.
    pub registration_off: bool,
    pub switch_revision: u64,
    pub switch_original: Option<(u64, Driver, String)>,
    /// `registration_off` as it was before the pending switch, restored with `switch_original`.
    pub switch_registration_off: bool,
    pub session: Option<Rc<ChatSession>>,
    pub root_session: Weak<ChatSession>,
    pub staged: Option<PreparedBinding>,
    pub pending_overlay: Option<PathBuf>,
    pub token: Option<TokenSlot>,
    pub generations: BTreeMap<ProcessGeneration, GenRt>,
    pub observed: ObservedCalls,
    pub seen_calls: std::collections::VecDeque<agent_core::delegation::correlation::InvocationKey>,
    pub run: Option<RunId>,
    pub last_run: Option<RunId>,
    pub pending: Option<super::seam::DelegatedInput>,
    pub dispatching: bool,
    pub readback: super::readback::Readback,
    /// The last run whose turn the agent was seen to open: the task-start watchdog's evidence,
    /// per run (a worker's state is inherited from its previous run until the new turn opens).
    pub opened_run: Option<RunId>,
    pub cwd: PathBuf,
}
impl WorkerRt {
    pub fn session(&self) -> Option<Rc<ChatSession>> {
        self.session.clone().or_else(|| self.root_session.upgrade())
    }
}
#[derive(Default)]
pub(super) struct Runtime {
    pub roots: HashMap<WorkerId, RootRt>,
    pub threads: HashMap<String, WorkerId>,
    pub workers: HashMap<WorkerId, WorkerRt>,
    pub connections: HashMap<ConnId, WorkerId>,
    pub dispatching: HashSet<OperationId>,
    pub waits: HashMap<(ConnId, u64), WorkerId>,
    pub subscribers: Vec<(String, Weak<dyn WorkerSubscriber>)>,
    pub shutting_down: bool,
    /// `commit_revoke` futures not yet finished. Quitting waits for them before the store
    /// writer closes, so the shutdown revocation is saved rather than refused.
    pub revokes_in_flight: usize,
    pub deferred: HashMap<WorkerId, std::collections::VecDeque<super::deferred::Delivery>>,
    pub controls: HashMap<String, super::deferred::Delivery>,
    pub child_specs: HashMap<WorkerId, super::child::ChildSpec>,
    pub preparations: HashMap<String, u64>,
    /// How long a task handed to a child may wait for its turn to open before the worker is
    /// failed as stalled; `None` is [`super::deferred::TASK_START_DEADLINE`] (tests shorten it).
    pub task_start_deadline: Option<std::time::Duration>,
    /// Per root, its children's news not yet given to it, and what it was already shown
    /// (see `wake.rs`).
    pub wakes: HashMap<WorkerId, agent_core::delegation::wake::Queue>,
    /// Roots with a wake attempt already scheduled.
    pub wake_armed: HashSet<WorkerId>,
}
impl Runtime {
    /// Whether `w` holds a capability issued under its root's current authority epoch. No
    /// token, or a root no longer registered, is stale.
    pub fn token_current(&self, w: &WorkerRt) -> bool {
        w.token.as_ref().is_some_and(|t| {
            self.roots
                .get(&w.root)
                .is_some_and(|r| t.epoch == r.row.epoch())
        })
    }
}
pub(crate) struct Supervisor {
    pub(super) services: AppServices,
    pub(super) limits: Limits,
    pub(super) routes: Rc<dyn RouteTable>,
    pub(super) state: RefCell<Runtime>,
    pub(super) ipc: RefCell<Option<Rc<IpcServer>>>,
    pub(super) rundir: RuntimeDir,
    pub(super) instance: InstanceNonce,
    pub(super) salt: [u8; 16],
    pub(super) weak: Weak<Supervisor>,
}
pub(super) fn error(code: ErrorCode, text: &str) -> ToolError {
    ToolError::new(code, text)
}
pub(super) fn now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| i64::try_from(d.as_millis()).unwrap_or(i64::MAX))
}
/// `v`'s serde name as the store spells it, `unknown` if it has none.
pub(super) fn enum_value<T: serde::Serialize>(v: &T) -> String {
    agent_core::serde_name::name_of(v).unwrap_or_else(|| "unknown".into())
}
pub(super) fn opted_in(p: &Profile) -> bool {
    p.available_for_delegation && !p.disabled
}

impl Supervisor {
    pub(crate) fn root_transfer_stop(self: &Rc<Self>, thread: &str) -> Vec<ChildStop> {
        let children = self.root_closing(thread);
        let data = {
            let mut state = self.state.borrow_mut();
            state.threads.get(thread).cloned().and_then(|root| {
                state.roots.get_mut(&root)?.transferring = true;
                let session = state.workers.get(&root)?.session()?;
                Some((root, session))
            })
        };
        if let Some((root, session)) = data {
            self.rotate_before_stop(&root, session.process_generation());
            session.rebind_stop();
        }
        children
    }
    pub(crate) async fn root_transfer_resume(
        self: &Rc<Self>,
        thread: &str,
        profile: &Profile,
        driver: Driver,
        bind: bool,
    ) -> bool {
        let root = self.state.borrow().threads.get(thread).cloned();
        let Some(root) = root else {
            return true;
        };
        {
            let mut state = self.state.borrow_mut();
            let Some(r) = state
                .roots
                .get_mut(&root)
                .filter(|r| r.closed && r.transferring)
            else {
                return false;
            };
            r.transferring = false;
            r.closed = false;
        }
        self.prepare_root_switch(thread, profile, driver, bind)
            .await
    }
    /// Moves a thread's root to `profile`. With `bind` false (see [`Self::agy_root_problem`])
    /// an opted-in profile is treated as opted out for this root: it starts unbound.
    pub(crate) async fn prepare_root_switch(
        self: &Rc<Self>,
        thread: &str,
        profile: &Profile,
        driver: Driver,
        bind: bool,
    ) -> bool {
        let data = {
            let state = self.state.borrow();
            state.threads.get(thread).and_then(|id| {
                state
                    .workers
                    .get(id)
                    .and_then(|w| w.session())
                    .map(|s| (id.clone(), s))
            })
        };
        let Some((worker, session)) = data else {
            return true;
        };
        self.rollback_root_switch(&worker, None);
        let ticket = {
            let mut state = self.state.borrow_mut();
            let Some(w) = state.workers.get_mut(&worker) else {
                return false;
            };
            w.switch_revision = w.switch_revision.wrapping_add(1);
            w.switch_original = Some((w.switch_revision, w.driver, w.row.profile_id.clone()));
            w.switch_registration_off = w.registration_off;
            w.switch_revision
        };
        let same_policy = {
            let state = self.state.borrow();
            let w = &state.workers[&worker];
            w.driver == driver && w.row.profile_id == profile.id
        };
        if same_policy {
            self.rotate_before_stop(&worker, session.process_generation());
        }
        if let Some(watch) = session.rebind_stop() {
            super::capacity::wait_exit(&watch).await;
        }
        let start = Instant::now();
        while self
            .state
            .borrow()
            .roots
            .get(&worker)
            .is_some_and(|r| r.revoking)
        {
            if start.elapsed() > std::time::Duration::from_secs(5) {
                return false;
            }
            glib::timeout_future(std::time::Duration::from_millis(10)).await;
        }
        if !self.root_session_current(&worker, &session)
            || self.state.borrow().workers[&worker].switch_revision != ticket
        {
            return false;
        }
        let id = worker.to_string();
        let profile_id = profile.id.clone();
        if self
            .services
            .durable("change root delegation profile", move |s| {
                s.set_delegation_worker_profile(&id, driver, &profile_id)
            })
            .await
            .is_err()
        {
            return false;
        }
        if self
            .state
            .borrow()
            .workers
            .get(&worker)
            .is_none_or(|w| w.switch_revision != ticket)
        {
            return false;
        }
        if let Some(w) = self.state.borrow_mut().workers.get_mut(&worker) {
            w.driver = driver;
            w.row.driver = enum_value(&driver);
            w.row.profile_id = profile.id.clone();
            w.registration_off = opted_in(profile) && !bind;
        }
        if !opted_in(profile) || !bind {
            if let Some(w) = self.state.borrow_mut().workers.get_mut(&worker) {
                w.switch_original = None;
            }
            return true;
        }
        let prepared = self
            .stage(&worker, session.process_generation().next(), profile)
            .await
            .is_ok()
            && self.root_session_current(&worker, &session)
            && self
                .state
                .borrow()
                .workers
                .get(&worker)
                .is_some_and(|w| w.switch_revision == ticket);
        if prepared {
            if let Some(w) = self.state.borrow_mut().workers.get_mut(&worker) {
                w.switch_original = None;
            }
        } else {
            self.rollback_root_switch(&worker, Some(ticket));
        }
        prepared
    }
    fn rollback_root_switch(&self, worker: &WorkerId, ticket: Option<u64>) {
        let restored = {
            let mut state = self.state.borrow_mut();
            let Some(w) = state.workers.get_mut(worker) else {
                return;
            };
            let Some((_, driver, profile)) = w
                .switch_original
                .clone()
                .filter(|(pending, _, _)| ticket.is_none_or(|expected| expected == *pending))
            else {
                return;
            };
            w.switch_original = None;
            w.driver = driver;
            w.row.driver = enum_value(&driver);
            w.row.profile_id = profile.clone();
            w.registration_off = w.switch_registration_off;
            w.row.identity_json = None;
            let overlay = w.staged.take().and_then(|b| b.overlay);
            (driver, profile, overlay)
        };
        let (driver, profile, overlay) = restored;
        let id = worker.to_string();
        self.services
            .write("restore cancelled root switch", None, true, move |s| {
                s.set_delegation_worker_profile(&id, driver, &profile)
            });
        if let Some(path) = overlay {
            gio::spawn_blocking(move || super::launch::unlink_overlay(&path));
        }
    }
    fn root_session_current(&self, worker: &WorkerId, session: &Rc<ChatSession>) -> bool {
        let state = self.state.borrow();
        !state.shutting_down
            && state.roots.get(worker).is_some_and(|r| !r.closed)
            && state
                .workers
                .get(worker)
                .and_then(WorkerRt::session)
                .is_some_and(|s| Rc::ptr_eq(&s, session))
    }
    pub(crate) fn worker_scope(
        &self,
        thread: &str,
        worker: &WorkerId,
    ) -> Option<agent_core::delegation::scope::Scope> {
        let scope = self.scope(worker)?;
        self.scope_belongs_to_thread(&scope, thread)
            .then_some(scope)
    }
    pub(crate) fn config_changed(self: &Rc<Self>, profile: &str) {
        let enabled = (self.services.profiles)()
            .iter()
            .any(|p| p.id == profile && opted_in(p));
        if enabled {
            return;
        }
        let affected = self
            .state
            .borrow()
            .workers
            .iter()
            .filter(|(_, w)| w.row.profile_id == profile)
            .map(|(id, w)| (id.clone(), w.root.clone(), w.row.parent_id.is_none()))
            .collect::<Vec<_>>();
        for (worker, root, is_root) in affected {
            if is_root {
                let conn = self
                    .state
                    .borrow_mut()
                    .workers
                    .get_mut(&worker)
                    .and_then(|w| w.token.take())
                    .and_then(|t| t.live_conn);
                if let (Some(conn), Some(ipc)) = (conn, self.ipc.borrow().as_ref()) {
                    ipc.revoke(
                        conn,
                        error(
                            ErrorCode::ProviderUnavailable,
                            "profile disabled for delegation",
                        ),
                    );
                }
                self.begin_revoke(&root, RevocationEvent::RootStop);
            } else {
                self.begin_revoke(&root, RevocationEvent::Close(worker));
            }
        }
    }
    pub(super) fn profile_available(&self, worker: &WorkerId) -> bool {
        let profile = self
            .state
            .borrow()
            .workers
            .get(worker)
            .filter(|w| !w.registration_off)
            .map(|w| w.row.profile_id.clone());
        profile.is_some_and(|id| {
            (self.services.profiles)()
                .iter()
                .any(|p| p.id == id && opted_in(p))
        })
    }
    pub(crate) fn root_generation_base(&self, thread: &str) -> ProcessGeneration {
        let state = self.state.borrow();
        state
            .threads
            .get(thread)
            .and_then(|id| state.workers.get(id))
            .map_or(ProcessGeneration(0), |w| w.generation)
    }
    pub(crate) async fn start(
        services: AppServices,
        limits: Limits,
        routes: Rc<dyn RouteTable>,
    ) -> Result<Rc<Self>, String> {
        super::reconcile::startup(&services)
            .await
            .map_err(|e| e.to_string())?;
        let (rundir, random) = gio::spawn_blocking(|| {
            use std::io::Read;
            let dir = super::rundir::verified()?;
            let mut random = [0; 32];
            std::fs::File::open("/dev/urandom")
                .and_then(|mut f| f.read_exact(&mut random))
                .map_err(|e| e.to_string())?;
            super::launch::sweep_overlays(&dir, &|pid| {
                std::fs::read_link(format!("/proc/{pid}/exe"))
                    .ok()
                    .is_some_and(|p| p.file_name().is_some_and(|n| n == "agent-terminal"))
            });
            Ok::<_, String>((dir, random))
        })
        .await
        .map_err(|_| "delegation startup worker failed".to_owned())??;
        let mut nonce = [0; 16];
        nonce.copy_from_slice(&random[..16]);
        let mut salt = [0; 16];
        salt.copy_from_slice(&random[16..]);
        let sup = Rc::new_cyclic(|weak| Self {
            services,
            limits,
            routes,
            state: RefCell::new(Runtime::default()),
            ipc: RefCell::new(None),
            rundir,
            instance: InstanceNonce::from_bytes(nonce),
            salt,
            weak: weak.clone(),
        });
        *sup.ipc.borrow_mut() = Some(IpcServer::bind(
            &sup.rundir,
            sup.services.app_pid,
            limits,
            sup.clone(),
        )?);
        Ok(sup)
    }
    pub(crate) async fn prepare_root(
        self: &Rc<Self>,
        thread: &str,
        profile: &Profile,
        driver: Driver,
    ) -> Option<Rc<dyn DelegationHooks>> {
        if !opted_in(profile) || self.state.borrow().shutting_down {
            return None;
        }
        let preparation = {
            let mut state = self.state.borrow_mut();
            let revision = state.preparations.entry(thread.into()).or_default();
            *revision = revision.wrapping_add(1);
            *revision
        };
        loop {
            let revoking = {
                let state = self.state.borrow();
                state
                    .threads
                    .get(thread)
                    .and_then(|id| state.roots.get(id))
                    .is_some_and(|root| root.revoking)
            };
            if !revoking {
                break;
            }
            glib::timeout_future(std::time::Duration::from_millis(10)).await;
            if !self.preparation_current(thread, preparation) {
                return None;
            }
        }
        let req = NewRoot {
            thread: thread.to_owned(),
            driver,
            profile_id: agent_core::delegation::ProfileId::parse(&profile.id).ok()?,
            name: profile.name.clone(),
            policy_json: "{}".into(),
        };
        let (row, worker, cwd, children) = self
            .services
            .durable("prepare delegation root", move |s| {
                let row = match s.delegation_root_of_thread(&req.thread)? {
                    Some(r) => {
                        let w = WorkerId::parse(&r.root_worker_id)
                            .map_err(|e| agent_kit::store::StoreError::Failed(e.to_string()))?;
                        s.resume_delegation_root(&w)?;
                        s.set_delegation_worker_profile(
                            w.as_str(),
                            req.driver,
                            req.profile_id.as_str(),
                        )?;
                        s.delegation_root(&r.id)?
                            .ok_or(agent_kit::store::StoreError::WriterGone)?
                    }
                    None => {
                        let h = s.create_delegation_root(&req)?;
                        s.delegation_root(&h.root)?
                            .ok_or(agent_kit::store::StoreError::WriterGone)?
                    }
                };
                let worker = s
                    .delegation_worker(&row.root_worker_id)?
                    .ok_or(agent_kit::store::StoreError::WriterGone)?;
                let cwd = s
                    .thread_summary(&req.thread)?
                    .map(|t| t.cwd)
                    .unwrap_or_default();
                let children = s.delegation_workers_of_root(&row.id)?;
                Ok((row, worker, cwd, children))
            })
            .await
            .ok()?;
        if !self.preparation_current(thread, preparation) {
            return None;
        }
        let id = WorkerId::parse(&worker.id).ok()?;
        let old = self
            .state
            .borrow()
            .workers
            .get(&id)
            .map(|w| {
                w.generations
                    .values()
                    .map(|g| g.watch.clone())
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();
        for watch in old {
            super::capacity::wait_exit(&watch).await;
        }
        if !self.preparation_current(thread, preparation) {
            return None;
        }
        let generation = self
            .state
            .borrow()
            .workers
            .get(&id)
            .map_or(ProcessGeneration(1), |w| w.generation.next());
        {
            let mut state = self.state.borrow_mut();
            state.threads.insert(thread.into(), id.clone());
            state.roots.insert(id.clone(), RootRt::new(row));
            for child in children.into_iter().filter(|c| c.parent_id.is_some()) {
                if let Ok(cid) = WorkerId::parse(&child.id) {
                    state
                        .workers
                        .entry(cid)
                        .or_insert_with(|| self.worker_rt(child, id.clone(), PathBuf::new()));
                }
            }
            if let Some(w) = state.workers.get_mut(&id) {
                w.row = worker;
                w.driver = driver;
                w.cwd = PathBuf::from(cwd);
                w.registration_off = false;
            } else {
                state.workers.insert(
                    id.clone(),
                    self.worker_rt(worker, id.clone(), PathBuf::from(cwd)),
                );
            }
        }
        self.restore_children(&id).await;
        if !self.preparation_current(thread, preparation) {
            return None;
        }
        if let Err(e) = self.stage(&id, generation, profile).await {
            (self.services.report)(&e.to_string());
            return None;
        }
        if !self.preparation_current(thread, preparation) {
            return None;
        }
        Some(Rc::new(super::attest::Hooks {
            supervisor: self.weak.clone(),
            worker: id,
        }))
    }
    fn preparation_current(&self, thread: &str, revision: u64) -> bool {
        let state = self.state.borrow();
        !state.shutting_down && state.preparations.get(thread) == Some(&revision)
    }
    pub(super) fn worker_rt(&self, row: WorkerRow, root: WorkerId, cwd: PathBuf) -> WorkerRt {
        let driver = row.driver_kind().unwrap_or(Driver::Claude);
        let mode = row
            .task_mode_kind()
            .map(TaskMode::as_mode)
            .unwrap_or(Mode::Ask);
        WorkerRt {
            row,
            root,
            driver,
            mode,
            generation: ProcessGeneration(0),
            rotating_generation: None,
            preparing: false,
            registration_off: false,
            switch_revision: 0,
            switch_original: None,
            switch_registration_off: false,
            session: None,
            root_session: Weak::new(),
            staged: None,
            pending_overlay: None,
            token: None,
            generations: BTreeMap::new(),
            observed: ObservedCalls::new(self.salt, self.limits.observed_calls as usize),
            seen_calls: std::collections::VecDeque::new(),
            run: None,
            last_run: None,
            pending: None,
            dispatching: false,
            readback: super::readback::Readback::default(),
            opened_run: None,
            cwd,
        }
    }
    pub(super) async fn stage(
        self: &Rc<Self>,
        worker: &WorkerId,
        generation: ProcessGeneration,
        profile: &Profile,
    ) -> Result<(), ToolError> {
        let epoch = self.epoch(worker)?;
        let (driver, role, task_mode) = {
            let state = self.state.borrow();
            let w = state
                .workers
                .get(worker)
                .ok_or_else(|| error(ErrorCode::NotFound, "unknown worker"))?;
            (
                w.driver,
                if w.row.parent_id.is_none() {
                    Role::Root
                } else {
                    Role::Child
                },
                w.row.task_mode_kind(),
            )
        };
        let req = BindingRequest {
            driver,
            role,
            worker: worker.clone(),
            generation,
            task_mode,
            app_pid: self.services.app_pid,
            bridge_command: self.services.bridge_command.clone(),
            endpoint: super::ipc::socket_path(&self.rundir, self.services.app_pid),
            instance: self.instance.clone(),
            rundir: self.rundir.clone(),
        };
        if !opted_in(profile) {
            return Err(error(
                ErrorCode::ProviderUnavailable,
                "profile unavailable for delegation",
            ));
        }
        if driver == Driver::Agy {
            self.recheck_agy(worker).await?;
        }
        let old = loop {
            let acquired = {
                let mut state = self.state.borrow_mut();
                let w = state.workers.get_mut(worker).ok_or_else(|| {
                    error(ErrorCode::NotFound, "worker closed during preparation")
                })?;
                if w.preparing {
                    None
                } else {
                    w.preparing = true;
                    Some(w.staged.take())
                }
            };
            if let Some(old) = acquired {
                break old;
            }
            glib::timeout_future(std::time::Duration::from_millis(10)).await;
            self.epoch(worker)?;
        };
        if let Some(path) = old.and_then(|b| b.overlay) {
            let _ = gio::spawn_blocking(move || super::launch::unlink_overlay(&path)).await;
        }
        let prepared = gio::spawn_blocking(move || super::launch::prepare_binding(&req)).await;
        if let Some(w) = self.state.borrow_mut().workers.get_mut(worker) {
            w.preparing = false;
        }
        let prep = prepared
            .map_err(|_| error(ErrorCode::InternalError, "launch preparation worker failed"))?
            .map_err(|_| {
                error(
                    ErrorCode::RuntimeUnavailable,
                    "cannot prepare delegation binding",
                )
            })?;
        let valid = self.epoch(worker) == Ok(epoch) && self.profile_available(worker) && {
            let state = self.state.borrow();
            state.workers.get(worker).is_some_and(|w| {
                w.row.state != "closed"
                    && (role != Role::Root || state.roots.get(&w.root).is_some_and(|r| !r.closed))
            })
        };
        if !valid {
            if let Some(path) = prep.overlay {
                gio::spawn_blocking(move || super::launch::unlink_overlay(&path));
            }
            return Err(error(
                ErrorCode::StaleEpoch,
                "authority changed during binding preparation",
            ));
        }
        let mut state = self.state.borrow_mut();
        let w = state
            .workers
            .get_mut(worker)
            .ok_or_else(|| error(ErrorCode::NotFound, "worker closed during preparation"))?;
        w.staged = Some(prep);
        Ok(())
    }
    pub(crate) fn attach_root_session(self: &Rc<Self>, thread: &str, session: Weak<ChatSession>) {
        let id = {
            let mut state = self.state.borrow_mut();
            let Some(id) = state.threads.get(thread).cloned() else {
                return;
            };
            if let Some(w) = state.workers.get_mut(&id) {
                w.root_session = session;
                if let Some(s) = w.root_session.upgrade() {
                    w.mode = s.effective_mode().session_safe();
                }
            }
            id
        };
        // News that waited for the thread to open.
        self.schedule_wake(&id, super::wake::WAKE_COALESCE);
    }
    pub(crate) fn before_root_turn(self: &Rc<Self>, thread: &str) -> RootTurnGate {
        let needed = {
            let state = self.state.borrow();
            let Some(id) = state.threads.get(thread) else {
                return RootTurnGate::Proceed;
            };
            let Some(w) = state.workers.get(id) else {
                return RootTurnGate::Proceed;
            };
            if state.shutting_down || state.roots.get(id).is_some_and(|r| r.revoking) {
                return RootTurnGate::Deferred;
            }
            if (w.token.is_some() && w.row.delegation_status != "rebind_required")
                || w.staged.is_some()
                || w.registration_off
            {
                return RootTurnGate::Proceed;
            }
            (id.clone(), w.row.profile_id.clone(), w.session())
        };
        let Some(session) = needed.2 else {
            return RootTurnGate::Proceed;
        };
        let Some(profile) = (self.services.profiles)()
            .into_iter()
            .find(|p| p.id == needed.1 && opted_in(p))
        else {
            return RootTurnGate::Proceed;
        };
        let sup = self.clone();
        glib::spawn_future_local(async move {
            let next = session.process_generation().next();
            if let Err(e) = sup.stage(&needed.0, next, &profile).await {
                (sup.services.report)(&e.to_string());
                return;
            }
            sup.rotate_before_stop(&needed.0, session.process_generation());
            if let Some(watch) = session.rebind_stop() {
                super::capacity::wait_exit(&watch).await;
            }
            if sup.root_session_current(&needed.0, &session) && sup.epoch(&needed.0).is_ok() {
                // A thread resuming from idle suspension had no cards or Codex grants to lose
                // (suspension waits for them), so there is nothing to say.
                if !session.resumes_from_suspension() {
                    session.note("Delegation was rebound; previous permission cards and session grants have expired.");
                }
                session.rebind_start();
            }
        });
        RootTurnGate::Deferred
    }
    pub(crate) fn on_root_turn_end(self: &Rc<Self>, thread: &str) {
        let id = self.state.borrow().threads.get(thread).cloned();
        if let Some(id) = id {
            self.release_waits(&id);
            // What the turn did not wait for is told now, once the session has settled.
            self.schedule_wake(&id, super::wake::WAKE_COALESCE);
        }
    }
    pub(crate) fn root_stop(self: &Rc<Self>, thread: &str) {
        {
            let mut state = self.state.borrow_mut();
            let revision = state.preparations.entry(thread.into()).or_default();
            *revision = revision.wrapping_add(1);
        }
        let root = self.state.borrow().threads.get(thread).cloned();
        if let Some(root) = root {
            let session = self
                .state
                .borrow()
                .workers
                .get(&root)
                .and_then(WorkerRt::session);
            // A stopped root is not woken by its sub-agents' news until the user speaks again.
            self.forget_wakes(&root);
            if let Some(session) = session {
                session.hold_wakes();
                session.interrupt_run();
            }
            self.begin_revoke(&root, RevocationEvent::RootStop);
        }
    }
    pub(crate) fn root_closing(self: &Rc<Self>, thread: &str) -> Vec<ChildStop> {
        {
            let mut state = self.state.borrow_mut();
            if let Some(root) = state.threads.get(thread).cloned() {
                if let Some(r) = state.roots.get_mut(&root) {
                    r.closed = true;
                    r.transferring = false;
                }
            }
        }
        self.root_stop(thread);
        let state = self.state.borrow();
        let Some(root) = state.threads.get(thread) else {
            return Vec::new();
        };
        state
            .workers
            .values()
            .filter(|w| &w.root == root && w.row.parent_id.is_some())
            .flat_map(|w| {
                w.generations.values().map(|g| ChildStop {
                    agent: enum_value(&w.driver),
                    title: w.row.name.clone(),
                    watch: g.watch.clone(),
                })
            })
            .collect()
    }
    pub(crate) fn root_busy(&self, thread: &str) -> bool {
        let state = self.state.borrow();
        let Some(root) = state.threads.get(thread) else {
            return false;
        };
        state.workers.values().any(|w| {
            &w.root == root
                && w.row.parent_id.is_some()
                && (w.run.is_some() || w.pending.is_some() || w.preparing || w.dispatching)
        })
    }
    /// Whether the thread has a sub-agent still alive: working (as [`Self::root_busy`]) or
    /// idle between turns with its process up. Changing the thread's agent ends every one, so
    /// the switch warns first.
    pub(crate) fn root_has_live_children(&self, thread: &str) -> bool {
        let state = self.state.borrow();
        let Some(root) = state.threads.get(thread) else {
            return false;
        };
        state.workers.values().any(|w| {
            &w.root == root
                && w.row.parent_id.is_some()
                && !matches!(w.row.state.as_str(), "closed" | "failed")
                && (w.run.is_some()
                    || w.pending.is_some()
                    || w.preparing
                    || w.dispatching
                    || !w.generations.is_empty())
        })
    }
    pub(crate) fn subscribe(&self, thread: &str, sub: Weak<dyn WorkerSubscriber>) {
        let high_water = self.services.handle.writer().map(|w| {
            w.durable_call("capture inspector replay boundary", |s| {
                s.delegation_event_high_water()
            })
        });
        let inline_water = if high_water.is_none() {
            self.services
                .handle
                .reader()
                .delegation_event_high_water()
                .ok()
        } else {
            None
        };
        let gate = Rc::new(ReplayGate {
            subscriber: sub.clone(),
            buffered: RefCell::new(Vec::new()),
            bytes: std::cell::Cell::new(0),
            overflow: std::cell::Cell::new(false),
        });
        let erased: Rc<dyn WorkerSubscriber> = gate.clone();
        let gate_weak = Rc::downgrade(&erased);
        self.state
            .borrow_mut()
            .subscribers
            .push((thread.into(), gate_weak.clone()));
        self.notify_registry();
        let Some(sup) = self.weak.upgrade() else {
            return;
        };
        let thread = thread.to_owned();
        let registered_thread = thread.clone();
        glib::spawn_future_local(async move {
            let high_water = if let Some(receiver) = high_water {
                gio::spawn_blocking(move || {
                    receiver
                        .recv_timeout(std::time::Duration::from_secs(5))
                        .ok()
                        .and_then(Result::ok)
                })
                .await
                .ok()
                .flatten()
            } else {
                inline_water
            };
            let Some(high_water) = high_water else {
                (sup.services.report)(
                    "Cannot load worker history: replay boundary was not committed.",
                );
                let mut state = sup.state.borrow_mut();
                state
                    .subscribers
                    .retain(|(_, weak)| !weak.ptr_eq(&gate_weak));
                state.subscribers.push((registered_thread, sub));
                return;
            };
            let history = sup
                .services
                .read(move |s| {
                    let Some(root) = s.delegation_root_of_thread(&thread)? else {
                        return Ok::<_, agent_kit::store::StoreError>((None, Vec::new()));
                    };
                    let root_id = WorkerId::parse(&root.root_worker_id)
                        .map_err(|e| agent_kit::store::StoreError::Failed(e.to_string()))?;
                    let children = s
                        .delegation_workers_of_root(&root.id)?
                        .into_iter()
                        .filter(|w| w.parent_id.is_some())
                        .collect::<Vec<_>>();
                    let mut out = Vec::new();
                    let mut bytes = 0usize;
                    'workers: for w in &children {
                        let Some(thread) = w.backing_thread_id.as_deref() else {
                            continue;
                        };
                        let Ok(worker) = WorkerId::parse(&w.id) else {
                            continue;
                        };
                        let mut after = None;
                        let mut run = None;
                        let mut generation = ProcessGeneration(0);
                        'pages: loop {
                            let page = s.events(thread, after, 256)?;
                            if page.is_empty() {
                                break;
                            }
                            for (seq, envelope) in page {
                                if seq>high_water {break 'pages;}
                                after = Some(seq);
                                for link in s
                                    .delegation_event_links(seq)?
                                    .into_iter()
                                    .filter(|l| l.worker_id == w.id)
                                {
                                    generation=ProcessGeneration::from_stored(link.process_generation);
                                    run=link.run_id.and_then(|r|RunId::parse(&r).ok());
                                }
                                bytes += serde_json::to_vec(&envelope)?.len();
                                if bytes > HISTORY_BYTES || out.len() >= HISTORY_EVENTS {
                                    out.push(agent_core::delegation::scope::ScopedEnvelope{scope:agent_core::delegation::scope::Scope{root:root_id.clone(),worker:worker.clone(),generation,run:run.clone()},envelope:agent_core::event::Envelope::new(agent_core::event::Event::Notice{text:format!("Inspector replay truncated before event {seq}; older history remains available through paginated worker read.")})});
                                    break 'workers;
                                }
                                out.push(agent_core::delegation::scope::ScopedEnvelope {
                                    scope: agent_core::delegation::scope::Scope {
                                        root: root_id.clone(),
                                        worker: worker.clone(),
                                        generation,
                                        run: run.clone(),
                                    },
                                    envelope,
                                });
                            }
                        }
                    }
                    Ok((Some((root_id, children)), out))
                })
                .await;
            if let (Some(sub), Some(Ok((stored, history)))) = (sub.upgrade(), history) {
                // A root not yet prepared in this process has no workers in memory, so its
                // replayed history would reach a view that cannot place it: list them first.
                if let Some((root, children)) = stored {
                    let mut workers = sup.summaries(&registered_thread);
                    let missing = children
                        .into_iter()
                        .filter_map(|row| {
                            let id = WorkerId::parse(&row.id).ok()?;
                            if workers.iter().any(|w| w.worker_id == id) {
                                return None;
                            }
                            sup.summary(&id, &sup.worker_rt(row, root.clone(), PathBuf::new()))
                        })
                        .collect::<Vec<_>>();
                    if !missing.is_empty() {
                        workers.extend(missing);
                        sub.registry(&registered_thread, &workers);
                    }
                }
                for env in history {
                    sub.replayed(&env);
                }
                loop {
                    let buffered = std::mem::take(&mut *gate.buffered.borrow_mut());
                    gate.bytes.set(0);
                    if buffered.is_empty() {
                        break;
                    }
                    for (seq, env) in buffered {
                        if seq > high_water {
                            sub.committed(&env, seq);
                        }
                    }
                }
                if gate.overflow.get() {
                    (sup.services.report)("Worker inspector live catch-up exceeded its buffer; reopen it or use paginated worker read to resynchronise.");
                }
                let mut state = sup.state.borrow_mut();
                state
                    .subscribers
                    .retain(|(_, weak)| !weak.ptr_eq(&gate_weak));
                state
                    .subscribers
                    .push((registered_thread, Rc::downgrade(&sub)));
            } else {
                (sup.services.report)("Cannot load worker history; live activity remains available. Reopen the inspector to retry history.");
                let mut state = sup.state.borrow_mut();
                state
                    .subscribers
                    .retain(|(_, weak)| !weak.ptr_eq(&gate_weak));
                state.subscribers.push((registered_thread, sub));
            }
        });
    }
    pub(crate) fn child_session(&self, worker: &WorkerId) -> Option<Rc<ChatSession>> {
        self.state.borrow().workers.get(worker)?.session.clone()
    }
    pub(crate) fn submit_human(self: &Rc<Self>, worker: &WorkerId, text: &str) {
        self.queue_human(worker, text);
    }
    pub(super) fn begin_revoke(self: &Rc<Self>, root: &WorkerId, event: RevocationEvent) {
        self.latch_revoke(root, &event);
        let revision = self
            .state
            .borrow()
            .roots
            .get(root)
            .map_or(0, |r| r.revision);
        let sup = self.clone();
        let root = root.clone();
        self.state.borrow_mut().revokes_in_flight += 1;
        glib::spawn_future_local(async move {
            let committed = sup.commit_revoke(&root, event, revision).await;
            sup.state.borrow_mut().revokes_in_flight -= 1;
            if let Err(e) = committed {
                (sup.services.report)(&e.to_string());
            }
        });
    }
    /// Iterates this thread's main context until no revocation is being committed, at most
    /// `wait`. Called outside any future on that context (the app's `shutdown` handler).
    pub(crate) fn settle_revokes(&self, wait: std::time::Duration) {
        let context = glib::MainContext::ref_thread_default();
        let deadline = Instant::now() + wait;
        while self.state.borrow().revokes_in_flight > 0 {
            let Some(left) = deadline.checked_duration_since(Instant::now()) else {
                tracing::warn!(
                    left = self.state.borrow().revokes_in_flight,
                    "shutdown: delegation revocations did not commit in time"
                );
                return;
            };
            // A wake-up so a blocking iteration never outlives the deadline.
            let tick = glib::timeout_source_new(
                left.min(std::time::Duration::from_millis(50)),
                None,
                glib::Priority::DEFAULT,
                || glib::ControlFlow::Break,
            );
            tick.attach(Some(&context));
            context.iteration(true);
            tick.destroy();
        }
    }
    pub(super) fn latch_revoke(&self, root: &WorkerId, event: &RevocationEvent) {
        if event.is_root_wide() {
            self.rollback_root_switch(root, None);
        }
        let (sessions, conns, overlays) = {
            let mut state = self.state.borrow_mut();
            let tree = state
                .workers
                .iter()
                .filter(|(_, w)| &w.root == root)
                .map(|(id, w)| (id.clone(), w.row.parent(), w.row.task_mode_kind()))
                .collect::<Vec<_>>();
            let effect = revocation(event, &tree);
            if event.is_root_wide() {
                if let Some(thread) = state.roots.get(root).map(|r| r.row.root_thread_id.clone()) {
                    let revision = state.preparations.entry(thread).or_default();
                    *revision = revision.wrapping_add(1);
                }
                if let Some(w) = state.workers.get_mut(root) {
                    w.switch_revision = w.switch_revision.wrapping_add(1);
                }
            }
            if let Some(r) = state.roots.get_mut(root) {
                r.revoking |= effect.bump;
                if matches!(
                    event,
                    RevocationEvent::RootStop | RevocationEvent::AppShutdown
                ) {
                    r.transferring = false;
                }
                if effect.bump {
                    r.revision = r.revision.wrapping_add(1);
                }
            }
            let mut sessions = Vec::new();
            let mut conns = Vec::new();
            let mut overlays = Vec::new();
            if effect.bump {
                for w in state.workers.values_mut().filter(|w| &w.root == root) {
                    if let Some(path) = w.staged.take().and_then(|b| b.overlay) {
                        overlays.push(path);
                    }
                    // Bound to a launch whose process never started: no exit will remove it.
                    if let Some(path) = w.pending_overlay.take() {
                        overlays.push(path);
                    }
                }
            }
            for id in &effect.kill_tokens {
                if let Some(w) = state.workers.get_mut(id) {
                    if let Some(t) = w.token.take() {
                        if let Some(c) = t.live_conn {
                            conns.push(c);
                        }
                    }
                    w.pending = None;
                }
            }
            for id in &effect.stop_processes {
                if let Some(w) = state.workers.get_mut(id) {
                    if let Some(s) = w.session() {
                        sessions.push((id.clone(), s));
                    }
                }
            }
            state.waits.retain(|_, w| !effect.kill_tokens.contains(w));
            state
                .deferred
                .retain(|worker, _| !effect.kill_tokens.contains(worker));
            state
                .controls
                .retain(|_, entry| !effect.kill_tokens.contains(&entry.worker));
            (sessions, conns, overlays)
        };
        for path in overlays {
            gio::spawn_blocking(move || super::launch::unlink_overlay(&path));
        }
        for c in conns {
            if let Some(ipc) = self.ipc.borrow().as_ref() {
                ipc.revoke(
                    c,
                    error(ErrorCode::StaleEpoch, "delegation authority revoked"),
                );
            }
        }
        let mut stopped = Vec::with_capacity(sessions.len());
        for (id, s) in sessions {
            s.expire_approvals("Delegation authority revoked");
            s.interrupt_run();
            s.shutdown(true);
            stopped.push(id);
        }
        if let Some(sup) = self.weak.upgrade() {
            sup.interrupt_open_runs(&stopped);
        }
    }
    pub(crate) async fn revoke(
        self: &Rc<Self>,
        root: &WorkerId,
        event: RevocationEvent,
    ) -> Result<(), ToolError> {
        self.latch_revoke(root, &event);
        let revision = self
            .state
            .borrow()
            .roots
            .get(root)
            .map_or(0, |r| r.revision);
        self.commit_revoke(root, event, revision).await
    }
    async fn commit_revoke(
        self: &Rc<Self>,
        root: &WorkerId,
        event: RevocationEvent,
        revision: u64,
    ) -> Result<(), ToolError> {
        let root_id = root.clone();
        let revoked = self
            .services
            .durable("revoke delegation", move |s| {
                s.revoke_delegation(&root_id, &event)
            })
            .await?;
        let mut state = self.state.borrow_mut();
        if let Some(r) = state.roots.get_mut(root) {
            r.row.authority_epoch = revoked.authority_epoch as i64;
            if r.revision == revision {
                r.revoking = false;
            }
        }
        for id in revoked.effect.mark_restricted {
            if let Some(w) = state.workers.get_mut(&id) {
                w.row.delegation_status = "restricted".into();
            }
        }
        for id in revoked.closed {
            if let Some(w) = state.workers.get_mut(&id) {
                w.row.state = "closed".into();
            }
        }
        drop(state);
        self.notify_registry();
        Ok(())
    }
    pub(super) fn epoch(&self, worker: &WorkerId) -> Result<AuthorityEpoch, ToolError> {
        let state = self.state.borrow();
        if state.shutting_down {
            return Err(error(
                ErrorCode::RuntimeUnavailable,
                "delegation is shutting down",
            ));
        }
        let w = state
            .workers
            .get(worker)
            .ok_or_else(|| error(ErrorCode::NotFound, "unknown worker"))?;
        let r = state
            .roots
            .get(&w.root)
            .ok_or_else(|| error(ErrorCode::RuntimeUnavailable, "root unavailable"))?;
        if r.revoking {
            return Err(error(
                ErrorCode::StaleEpoch,
                "delegation authority is changing",
            ));
        }
        Ok(r.row.epoch())
    }
    pub(super) fn principal_mode(&self, worker: &WorkerId) -> Mode {
        self.state
            .borrow()
            .workers
            .get(worker)
            .map(|w| {
                w.session()
                    .map_or(w.mode, |s| s.effective_mode())
                    .session_safe()
            })
            .unwrap_or(Mode::Plan)
    }
    pub(super) fn notify_registry(&self) {
        let work = {
            let mut state = self.state.borrow_mut();
            state.subscribers.retain(|(_, s)| s.strong_count() > 0);
            state
                .subscribers
                .iter()
                .filter_map(|(t, s)| Some((t.clone(), s.upgrade()?)))
                .collect::<Vec<_>>()
        };
        for (thread, sub) in work {
            sub.registry(&thread, &self.summaries(&thread));
        }
    }
    pub(super) fn summaries(&self, thread: &str) -> Vec<WorkerSummary> {
        let state = self.state.borrow();
        let Some(root) = state.threads.get(thread) else {
            return Vec::new();
        };
        state
            .workers
            .iter()
            .filter(|(_, w)| &w.root == root && w.row.parent_id.is_some())
            .filter_map(|(id, w)| self.summary(id, w))
            .collect()
    }
    pub(super) fn summary(&self, id: &WorkerId, w: &WorkerRt) -> Option<WorkerSummary> {
        use agent_core::delegation::wire::*;
        let mut summary = WorkerSummary {
            worker_id: id.clone(),
            parent_id: w.row.parent(),
            name: w.row.name.clone(),
            origin: parse_lenient(&w.row.origin)?,
            state: parse_lenient(&w.row.state).unwrap_or(WorkerState::Unknown),
            run_id: w.run.clone(),
            run_state: w
                .run
                .as_ref()
                .map(|_| agent_core::delegation::lifecycle::RunState::Running),
            identity: w
                .row
                .identity_json
                .as_deref()
                .and_then(|v| serde_json::from_str(v).ok()),
            depth: w.row.depth as u8,
            can_spawn: w.row.depth < self.limits.max_depth as i64,
            controls: Controls {
                send: w.row.state != "closed",
                this_run: agent_core::delegation::route::DelegationCaps::of(w.driver).this_run,
                cancel: w.run.is_some(),
                close: w.row.state != "closed",
            },
            task_mode: w.row.task_mode_kind(),
            workspace: serde_json::from_str(&w.row.workspace_json).unwrap_or(WorkspaceInfo {
                kind: WorkspaceKind::SharedReadOnly,
                path: None,
                base_commit: None,
            }),
            delegation: DelegationStatus {
                code: parse_lenient(&w.row.delegation_status)
                    .unwrap_or(DelegationStatusCode::Unavailable),
                reason: w.row.delegation_reason.clone(),
                paths: Vec::new(),
            },
            last_activity: w.row.last_activity.clone(),
            updated_at: agent_core::quota::epoch_to_rfc3339(w.row.updated_at / 1000),
        };
        fit_summary(&mut summary);
        Some(summary)
    }
    pub(crate) fn shutdown(self: &Rc<Self>) -> Vec<ChildStop> {
        self.state.borrow_mut().shutting_down = true;
        let roots = self
            .state
            .borrow()
            .roots
            .keys()
            .cloned()
            .collect::<Vec<_>>();
        for root in roots {
            self.begin_revoke(&root, RevocationEvent::AppShutdown);
        }
        if let Some(ipc) = self.ipc.borrow().as_ref() {
            ipc.close_all(error(
                ErrorCode::RuntimeUnavailable,
                "application shutting down",
            ));
        }
        self.state
            .borrow()
            .workers
            .values()
            .filter(|w| w.row.parent_id.is_some())
            .flat_map(|w| {
                w.generations.values().map(|g| ChildStop {
                    agent: enum_value(&w.driver),
                    title: w.row.name.clone(),
                    watch: g.watch.clone(),
                })
            })
            .collect()
    }
}
