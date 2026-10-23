//! The session seam with fake delegation hooks, a fake child
//! gate and publisher, and real adapters over deterministic fake processes. Nothing reaches a
//! provider or the network.

use super::*;
use crate::agent_proc::{ExitWatch, StopPhase};
use crate::delegation::seam::{
    ApprovalGate, ChildPolicy, ChildPublish, DelegatedInput, DelegatedOutcome, DelegationHooks,
    GateVerdict, InputSource, LaunchBinding, SessionOptions, TurnBoundary,
};
use agent_core::codex::CodexAdapter;
use agent_core::delegation::mailbox::{DeliveryPlan, MessageKind};
use agent_core::delegation::{AuthorityEpoch, MessageId, OperationId, RunId, WorkerId};
use serde_json::{json, Value};

// ---- fakes ----

type Judge = dyn Fn(&str, &Value) -> GateVerdict;

/// A child gate whose verdicts the test sets; it logs every call, in order.
struct TestGate {
    verdict: Box<Judge>,
    rules: Cell<bool>,
    log: RefCell<Vec<String>>,
}

impl TestGate {
    fn new(verdict: impl Fn(&str, &Value) -> GateVerdict + 'static, rules: bool) -> Rc<Self> {
        Rc::new(Self {
            verdict: Box::new(verdict),
            rules: Cell::new(rules),
            log: RefCell::default(),
        })
    }

    fn defer(rules: bool) -> Rc<Self> {
        Self::new(|_, _| GateVerdict::Defer, rules)
    }
}

impl ApprovalGate for TestGate {
    fn judge(&self, tool: &str, input: &Value, _cwd: Option<&str>) -> GateVerdict {
        self.log.borrow_mut().push(format!("judge {tool}"));
        (self.verdict)(tool, input)
    }

    fn rule_may_apply(&self, _action: &crate::always_allow::Action, _cwd: Option<&str>) -> bool {
        self.log.borrow_mut().push("rule".to_owned());
        self.rules.get()
    }
}

/// For a Plan child: only reads pass to the existing policy; every edit, command
/// and unknown tool is denied. The classification here stands in for `authority::effect`.
fn plan_gate() -> Rc<TestGate> {
    const READS: &[&str] = &["Read", "Glob", "Grep", "LS", "view_file", "list_dir"];
    TestGate::new(
        |tool, _| {
            if READS.contains(&tool) {
                GateVerdict::Defer
            } else {
                GateVerdict::Deny {
                    reason: "a Plan agent cannot do this".to_owned(),
                }
            }
        },
        false,
    )
}

/// A publication boundary that passes everything through and records what it was given.
#[derive(Default)]
struct TestPublish {
    published: RefCell<Vec<Envelope>>,
    persisted: RefCell<Vec<Envelope>>,
    cancelled: RefCell<Vec<u64>>,
}

impl ChildPublish for TestPublish {
    fn publish(&self, _generation: ProcessGeneration, env: Envelope) -> Option<Envelope> {
        self.published.borrow_mut().push(env.clone());
        Some(env)
    }

    fn persist(&self, _thread: &str, _provider: Option<&str>, env: &Envelope) {
        self.persisted.borrow_mut().push(env.clone());
    }

    fn cancel_generation(&self, generation: ProcessGeneration) {
        self.cancelled.borrow_mut().push(generation.0);
    }
}

/// Delegation hooks that stage at most one binding and record every call.
#[derive(Default)]
struct TestHooks {
    hold_spawn: Rc<Cell<bool>>,
    completed_launch_checks: Rc<Cell<usize>>,
    refuse_spawn: Cell<bool>,
    launch_checks: RefCell<Vec<DelegationLaunchFacts>>,
    hold_prompt: Cell<bool>,
    hold_after_check: Cell<Option<usize>>,
    prompt_checks: Cell<usize>,
    staged: RefCell<Option<(u64, LaunchBinding)>>,
    asked: RefCell<Vec<(Driver, u64)>>,
    started: RefCell<Vec<(u64, Option<i32>, ExitWatch)>>,
    /// Each stopping generation, with its own watch's phase at that moment.
    stopping: RefCell<Vec<(u64, Option<StopPhase>)>>,
    lines: RefCell<Vec<String>>,
    observed: Cell<usize>,
    turns: RefCell<Vec<String>>,
    modes: RefCell<Vec<Mode>>,
    /// What `suspending` answers, and each generation it was asked about.
    allow_suspend: Cell<bool>,
    suspend_asks: RefCell<Vec<u64>>,
    /// Each generation whose binding was released without a process.
    abandoned: RefCell<Vec<u64>>,
}

impl DelegationHooks for TestHooks {
    fn before_spawn(
        &self,
        facts: DelegationLaunchFacts,
    ) -> crate::delegation::seam::LocalFuture<Result<(), String>> {
        self.launch_checks.borrow_mut().push(facts);
        let hold = self.hold_spawn.clone();
        let completed = self.completed_launch_checks.clone();
        let refuse = self.refuse_spawn.get();
        Box::pin(async move {
            while hold.get() {
                glib::timeout_future(Duration::from_millis(1)).await;
            }
            completed.set(completed.get() + 1);
            if refuse {
                Err("synthetic final launch refusal".into())
            } else {
                Ok(())
            }
        })
    }
    fn before_root_prompt(&self) -> bool {
        self.prompt_checks.set(self.prompt_checks.get() + 1);
        !self.hold_prompt.get()
            && self
                .hold_after_check
                .get()
                .is_none_or(|n| self.prompt_checks.get() <= n)
    }
    fn binding(&self, driver: Driver, generation: ProcessGeneration) -> Option<LaunchBinding> {
        self.asked.borrow_mut().push((driver, generation.0));
        let staged = self.staged.borrow_mut().take();
        match staged {
            Some((g, binding)) if g == generation.0 => Some(binding),
            other => {
                *self.staged.borrow_mut() = other;
                None
            }
        }
    }

    fn process_started(&self, generation: ProcessGeneration, pid: Option<i32>, watch: ExitWatch) {
        self.started.borrow_mut().push((generation.0, pid, watch));
    }

    fn launch_abandoned(&self, generation: ProcessGeneration) {
        self.abandoned.borrow_mut().push(generation.0);
    }

    fn process_stopping(&self, generation: ProcessGeneration) {
        let phase = self
            .started
            .borrow()
            .iter()
            .find(|(g, _, _)| *g == generation.0)
            .map(|(_, _, w)| w.phase());
        self.stopping.borrow_mut().push((generation.0, phase));
    }

    fn suspending(&self, generation: ProcessGeneration) -> bool {
        self.suspend_asks.borrow_mut().push(generation.0);
        self.allow_suspend.get()
    }

    fn line(&self, _generation: ProcessGeneration, line: &str, _stderr: bool) {
        self.lines.borrow_mut().push(line.to_owned());
    }

    fn observed(&self, _generation: ProcessGeneration, _env: &Envelope) {
        self.observed.set(self.observed.get() + 1);
    }

    fn turn(&self, generation: ProcessGeneration, boundary: TurnBoundary<'_>) {
        let text = match boundary {
            TurnBoundary::Started { native_turn } => {
                format!("{} started {}", generation.0, native_turn.unwrap_or("-"))
            }
            TurnBoundary::Ended { state } => format!("{} ended {state:?}", generation.0),
        };
        self.turns.borrow_mut().push(text);
    }

    fn mode_changed(&self, effective: Mode) {
        self.modes.borrow_mut().push(effective);
    }
}

// ---- helpers ----

fn worker(suffix: &str) -> WorkerId {
    WorkerId::from_suffix(suffix).expect("worker id")
}

fn input(source: InputSource, plan: DeliveryPlan, text: &str) -> DelegatedInput {
    DelegatedInput {
        operation: OperationId::from_suffix("operation1").expect("operation id"),
        epoch: AuthorityEpoch(1),
        source,
        from: worker("parent001"),
        from_label: "Claude (root)".into(),
        text: text.into(),
        plan,
        tag: "op-tag".into(),
    }
}

fn task(text: &str) -> DelegatedInput {
    input(InputSource::Task, DeliveryPlan::StartTurn, text)
}

fn instruction(plan: DeliveryPlan, text: &str) -> DelegatedInput {
    input(
        InputSource::Message {
            id: MessageId::from_suffix("message01").expect("message id"),
            kind: MessageKind::Instruction,
        },
        plan,
        text,
    )
}

fn run() -> RunId {
    RunId::from_suffix("run000001").expect("run id")
}

/// A private environment for a fake process: no inherited variable reaches it.
fn bare_env(dir: &Path) -> LaunchEnv {
    LaunchEnv {
        unset: std::env::vars_os()
            .map(|(key, _)| key.to_string_lossy().into_owned())
            .collect(),
        env: vec![
            ("HOME".into(), dir.to_string_lossy().into_owned()),
            ("PATH".into(), "/usr/bin:/bin".into()),
            ("LC_ALL".into(), "C".into()),
        ],
    }
}

fn codex_open(dir: &Path, mode: Mode) -> OpenSession {
    OpenSession {
        program: "/usr/bin/python3".into(),
        extra_args: vec![
            "-I".into(),
            format!(
                "{}/tests/support/scripted_codex.py",
                env!("CARGO_MANIFEST_DIR")
            ),
            dir.to_string_lossy().into_owned(),
        ],
        cwd: dir.to_string_lossy().into_owned(),
        model: Some("synthetic-model".into()),
        effort: None,
        mode,
        resume: None,
        new_session_id: None,
        approval_hook: false,
        delegation: None,
    }
}

struct Child {
    dir: tempfile::TempDir,
    store: Rc<Store>,
    thread: ThreadId,
    session: Rc<ChatSession>,
    seen: Seen,
    publish: Rc<TestPublish>,
    hooks: Rc<TestHooks>,
}

impl Child {
    fn frames(&self) -> Vec<Value> {
        std::fs::read_to_string(self.dir.path().join("frames.ndjson"))
            .unwrap_or_default()
            .lines()
            .filter_map(|l| serde_json::from_str::<Value>(l).ok())
            .collect()
    }

    fn sent(&self, method: &str) -> Vec<Value> {
        self.frames()
            .into_iter()
            .filter(|f| f["dir"] == "in" && f["frame"]["method"] == method)
            .map(|f| f["frame"].clone())
            .collect()
    }

    fn result_for(&self, tag: &str) -> Option<Envelope> {
        self.seen
            .borrow()
            .iter()
            .find(|e| {
                e.request.as_deref() == Some(tag) && matches!(e.event, Event::ControlResult { .. })
            })
            .cloned()
    }
}

/// A supervised Codex child over the scripted app-server, with `gate` and `mode`.
fn codex_child(mode: Mode, gate: Rc<TestGate>, prepare: impl FnOnce(&Path)) -> Child {
    let dir = tempfile::tempdir().expect("dir");
    prepare(dir.path());
    let store = Rc::new(Store::open(&dir.path().join("threads.sqlite")).expect("store"));
    let thread = fresh(&store, &dir.path().to_string_lossy());
    let (sink, seen) = make_sink();
    let publish = Rc::new(TestPublish::default());
    let hooks = Rc::new(TestHooks::default());
    let session = ChatSession::with_options(
        Box::new(CodexAdapter::new()),
        codex_open(dir.path(), mode),
        SessionStore::from(store.clone()),
        thread.clone(),
        sink,
        Err(None),
        bare_env(dir.path()),
        SessionOptions {
            generation_base: ProcessGeneration(0),
            hooks: Some(hooks.clone()),
            child: Some(ChildPolicy {
                gate,
                publish: publish.clone(),
                mode,
            }),
        },
    );
    session.set_delegation_worker(worker("child0001"));
    Child {
        dir,
        store,
        thread,
        session,
        seen,
        publish,
        hooks,
    }
}

/// A supervised Claude child whose fake process records its stdin in `stdin.log`, with its
/// Always rules in its directory.
fn claude_child(
    ctx: &glib::MainContext,
    dir: &Path,
    mode: Mode,
    gate: Rc<TestGate>,
) -> (Rc<ChatSession>, Seen) {
    let store = Rc::new(Store::open_in_memory().expect("store"));
    let thread = fresh(&store, &dir.to_string_lossy());
    let (sink, seen) = make_sink();
    let mut open = scripted(dir, "cat > \"$D/stdin.log\"");
    open.mode = mode;
    let session = ChatSession::with_options(
        Box::new(agent_core::claude::ClaudeAdapter::new()),
        open,
        SessionStore::from(store),
        thread,
        sink,
        Err(None),
        LaunchEnv::default(),
        SessionOptions {
            generation_base: ProcessGeneration(0),
            hooks: None,
            child: Some(ChildPolicy {
                gate,
                publish: Rc::new(TestPublish::default()),
                mode,
            }),
        },
    );
    *session.inner.saved_rules.borrow_mut() = Some(crate::always_allow::StoreCache::new(
        dir.join(crate::always_allow::FILE_NAME),
    ));
    assert!(pump_until(ctx, 10, || session.inner.state.borrow().alive));
    (session, seen)
}

fn claude_tool(id: &str, tool: &str, input: Value) -> String {
    json!({"type": "control_request", "request_id": id, "request": {
        "subtype": "can_use_tool", "tool_name": tool, "tool_use_id": format!("tu-{id}"),
        "input": input}})
    .to_string()
}

fn stdin_has(ctx: &glib::MainContext, dir: &Path, needle: &str) -> bool {
    pump_until(ctx, 10, || stdin_log(dir).contains(needle))
}

// ---- delivery ----

/// A task starts a turn as a stored `DelegatedMessage` (never a `UserMessage`), labelled
/// for the agent as coming from another agent, through the child's publication boundary
/// (never `store.append_event`), and never through the session's own queue.
#[test]
fn a_delegated_task_starts_a_labelled_turn_and_is_never_a_user_message_or_queued() {
    in_loop(|ctx| {
        let child = codex_child(Mode::Ask, TestGate::defer(false), |_| {});
        let outcome = child
            .session
            .deliver_delegated(task("Review the parser\nsuccess"));
        assert_eq!(outcome, DelegatedOutcome::TurnStarted);
        assert!(pump_until(ctx, 10, || child
            .publish
            .persisted
            .borrow()
            .iter()
            .any(|e| matches!(e.event, Event::TurnCompleted { .. }))));
        let starts = child.sent("turn/start");
        assert_eq!(starts.len(), 1, "{starts:?}");
        let sent = starts[0]["params"]["input"][0]["text"]
            .as_str()
            .expect("text");
        assert!(
            sent.starts_with("[Task delegated to you by Claude (root) (op_operation1)."),
            "{sent}"
        );
        assert!(sent.ends_with("\nReview the parser\nsuccess"), "{sent}");
        // Stored through the publisher, as delegated data with no label in it.
        let persisted = child.publish.persisted.borrow();
        let message = persisted
            .iter()
            .find_map(|e| match &e.event {
                Event::DelegatedMessage { text, to, from, .. } => Some((text, to, from)),
                _ => None,
            })
            .expect("a delegated message");
        assert_eq!(message.0, "Review the parser\nsuccess");
        assert_eq!(message.1.as_str(), "w_child0001");
        assert_eq!(message.2.as_str(), "w_parent001");
        assert!(!persisted.iter().any(|e| matches!(
            e.event,
            Event::ItemStarted {
                kind: ItemKind::UserMessage,
                ..
            }
        )));
        drop(persisted);
        // The store itself was never written for the child's events.
        let stored = child
            .store
            .events(&child.thread, None, 1_000)
            .expect("events");
        assert!(stored.is_empty(), "{stored:?}");
        assert!(child.session.inner.queued_prompts.borrow().is_empty());
        child.session.shutdown(false);
    });
}

/// Delegated input is refused, never queued in the session, when it cannot be written now:
/// a turn already running, a plan the session does not own, a delivery the agent cannot take,
/// or a session with no worker named.
#[test]
fn delegated_input_that_cannot_be_written_now_is_refused_and_never_queued() {
    in_loop(|ctx| {
        let child = codex_child(Mode::Ask, TestGate::defer(false), |_| {});
        assert_eq!(
            child
                .session
                .deliver_delegated(instruction(DeliveryPlan::QueueNextTurn, "later")),
            DelegatedOutcome::Refused(agent_core::delegation::ErrorCode::InternalError)
        );
        assert_eq!(
            child.session.deliver_delegated(instruction(
                DeliveryPlan::WriteLive { target: None },
                "live"
            )),
            DelegatedOutcome::Refused(agent_core::delegation::ErrorCode::UnsupportedDelivery),
            "a live user line is Claude's"
        );
        assert_eq!(
            child
                .session
                .deliver_delegated(instruction(DeliveryPlan::SteerNow(run()), "no turn")),
            DelegatedOutcome::Refused(agent_core::delegation::ErrorCode::StaleRun)
        );
        assert_eq!(
            child.session.deliver_delegated(task("first\nhold")),
            DelegatedOutcome::TurnStarted
        );
        assert!(pump_until(ctx, 10, || child
            .session
            .inner
            .native_turn
            .borrow()
            .is_some()));
        assert_eq!(
            child.session.deliver_delegated(task("second")),
            DelegatedOutcome::Refused(agent_core::delegation::ErrorCode::StaleRun)
        );
        assert!(child.session.inner.queued_prompts.borrow().is_empty());
        assert!(child.session.interrupt_run());
        assert!(pump_until(ctx, 10, || !child.session.status().running_turn));
        assert!(!child.session.interrupt_run(), "nothing left to interrupt");

        // Unnamed, a session records no message with no recipient: it refuses.
        *child.session.inner.worker.borrow_mut() = None;
        assert_eq!(
            child.session.deliver_delegated(task("x")),
            DelegatedOutcome::Refused(agent_core::delegation::ErrorCode::InternalError)
        );
        child.session.shutdown(false);
    });
}

/// A Codex strict steer is pinned to the running turn. Accepted, its tag gets an `ok`;
/// refused, its tag gets an `error`, and NO new turn starts (the user steer's fallback is not
/// taken).
#[test]
fn a_refused_strict_steer_starts_no_new_turn() {
    in_loop(|ctx| {
        let child = codex_child(Mode::Ask, TestGate::defer(false), |_| {});
        assert_eq!(
            child.session.deliver_delegated(task("work\nhold")),
            DelegatedOutcome::TurnStarted
        );
        assert!(pump_until(ctx, 10, || child
            .session
            .inner
            .native_turn
            .borrow()
            .is_some()));
        let mut accepted = instruction(DeliveryPlan::SteerNow(run()), "also the tests");
        accepted.tag = "steer-ok".into();
        assert_eq!(
            child.session.deliver_delegated(accepted),
            DelegatedOutcome::SteerSent {
                tag: "steer-ok".into()
            }
        );
        assert!(pump_until(ctx, 10, || child
            .result_for("steer-ok")
            .is_some()));
        assert!(matches!(
            child.result_for("steer-ok").expect("result").event,
            Event::ControlResult {
                ok: Some(_),
                error: None
            }
        ));
        let steered = std::fs::read_to_string(child.dir.path().join("steered")).expect("steered");
        assert!(steered.contains("[Delegated instruction from Claude (root) (m_message01)."));

        std::fs::write(child.dir.path().join("refuse-steer"), "").expect("refuse");
        let mut refused = instruction(DeliveryPlan::SteerNow(run()), "too late");
        refused.tag = "steer-refused".into();
        assert_eq!(
            child.session.deliver_delegated(refused),
            DelegatedOutcome::SteerSent {
                tag: "steer-refused".into()
            }
        );
        assert!(pump_until(ctx, 10, || child
            .result_for("steer-refused")
            .is_some()));
        assert!(matches!(
            child.result_for("steer-refused").expect("result").event,
            Event::ControlResult {
                ok: None,
                error: Some(_)
            }
        ));
        // Give a fallback every chance to show itself: none comes.
        assert!(!pump_until(ctx, 1, || child.sent("turn/start").len() > 1));
        assert_eq!(child.sent("turn/start").len(), 1, "no new turn");
        assert_eq!(child.sent("turn/steer").len(), 2);
        assert!(!has(&child.seen, |e| matches!(e,
            Event::Notice { text } if text.contains("new message"))));
        assert!(child.session.status().running_turn, "the turn runs on");
        assert!(child.session.inner.queued_prompts.borrow().is_empty());
        child.session.shutdown(false);
    });
}

// ---- inherited authority before every auto-allow ----

/// The gate judges first. A denial is answered at once even when an Always rule
/// matches the call; with the gate deferring but forbidding rules, the rule neither applies
/// nor is offered; only when the gate allows rules does the Always rule answer the call.
#[test]
fn the_gate_runs_before_an_always_rule() {
    in_loop(|ctx| {
        let tmp = tempfile::tempdir().expect("tmp");
        let rules = tmp.path().join(crate::always_allow::FILE_NAME);
        let rule = crate::always_allow::Rule::new("*", "Bash", "touch b.tmp").expect("rule");
        crate::always_allow::AlwaysRules::remember(&rules, rule).expect("saved");

        let deny = TestGate::new(
            |_, _| GateVerdict::Deny {
                reason: "revoked".into(),
            },
            true,
        );
        let (session, seen) = claude_child(ctx, tmp.path(), Mode::Ask, deny.clone());
        feed(&session, &can_use_tool("r1", "touch b.tmp"));
        assert_eq!(resolution(&seen, "r1"), Some(Decision::Deny));
        assert_eq!(
            deny.log.borrow().first().map(String::as_str),
            Some("judge Bash")
        );
        assert!(
            !deny.log.borrow().iter().any(|c| c == "rule"),
            "no rule is consulted after a denial: {:?}",
            deny.log.borrow()
        );
        assert!(stdin_has(ctx, tmp.path(), "\"r1\""));
        assert!(stdin_log(tmp.path()).contains("deny"));
        session.shutdown(false);

        let dir2 = tempfile::tempdir().expect("tmp");
        std::fs::copy(&rules, dir2.path().join(crate::always_allow::FILE_NAME)).expect("copy");
        let no_rules = TestGate::defer(false);
        let (session, seen) = claude_child(ctx, dir2.path(), Mode::Ask, no_rules.clone());
        feed(&session, &can_use_tool("r2", "touch b.tmp"));
        assert!(!pump_until(ctx, 1, || resolution(&seen, "r2").is_some()));
        let options = offered(&seen, "r2");
        assert!(
            !options.contains(&Decision::AllowForSession)
                && !options.contains(&Decision::AllowAlways),
            "{options:?}"
        );
        assert_eq!(no_rules.log.borrow()[0], "judge Bash");
        session.shutdown(false);

        let dir3 = tempfile::tempdir().expect("tmp");
        std::fs::copy(&rules, dir3.path().join(crate::always_allow::FILE_NAME)).expect("copy");
        let (session, seen) = claude_child(ctx, dir3.path(), Mode::Ask, TestGate::defer(true));
        feed(&session, &can_use_tool("r3", "touch b.tmp"));
        assert!(pump_until(ctx, 10, || resolution(&seen, "r3").is_some()));
        assert_eq!(resolution(&seen, "r3"), Some(Decision::Allow));
        session.shutdown(false);
    });
}

/// An `AskOnly` verdict forces a card with no grant to offer, never auto-allowed, even in
/// Accept edits with a matching session rule.
#[test]
fn an_ask_only_card_offers_no_grant_and_is_never_auto_allowed() {
    in_loop(|ctx| {
        let tmp = tempfile::tempdir().expect("tmp");
        let gate = TestGate::new(
            |_, _| GateVerdict::AskOnly {
                reason: "outside this agent's worktree".into(),
            },
            true,
        );
        let (session, seen) = claude_child(ctx, tmp.path(), Mode::AcceptEdits, gate);
        let rule = crate::always_allow::Rule::new("", "Bash", "touch b.tmp").expect("rule");
        session.inner.session_rules.borrow_mut().add(rule);
        feed(&session, &can_use_tool("r1", "touch b.tmp"));
        assert!(!pump_until(ctx, 1, || resolution(&seen, "r1").is_some()));
        assert_eq!(
            offered(&seen, "r1"),
            vec![Decision::Allow, Decision::Deny, Decision::Cancel]
        );
        // Asked to keep a rule anyway, it allows once and keeps nothing.
        session.respond_approval("r1", Decision::AllowForSession);
        assert_eq!(resolution(&seen, "r1"), Some(Decision::Allow));
        assert_eq!(session.inner.session_rules.borrow().rules.len(), 1);
        session.shutdown(false);
    });
}

/// A rule the card offered is checked again when the answer comes; if the gate
/// no longer allows it, the call is allowed once and nothing is kept, with a notice.
#[test]
fn a_rule_is_rechecked_when_it_would_be_kept() {
    in_loop(|ctx| {
        let tmp = tempfile::tempdir().expect("tmp");
        let gate = TestGate::defer(true);
        let (session, seen) = claude_child(ctx, tmp.path(), Mode::Ask, gate.clone());
        feed(&session, &can_use_tool("r1", "touch b.tmp"));
        assert!(offered(&seen, "r1").contains(&Decision::AllowForSession));
        gate.rules.set(false); // the parent was restricted meanwhile
        session.respond_approval("r1", Decision::AllowForSession);
        assert_eq!(resolution(&seen, "r1"), Some(Decision::Allow));
        assert!(session.inner.session_rules.borrow().rules.is_empty());
        assert!(has(&seen, |e| matches!(e,
            Event::Notice { text } if text.contains("no rule was kept"))));
        session.shutdown(false);
    });
}

/// A plain Allow is also a grant: authority revoked while the card waits must win.
#[test]
fn a_stale_plain_allow_is_denied_for_claude_and_codex() {
    in_loop(|ctx| {
        let revoked = Rc::new(Cell::new(false));
        let live = revoked.clone();
        let gate = TestGate::new(
            move |_, _| {
                if live.get() {
                    GateVerdict::Deny {
                        reason: "revoked while waiting".into(),
                    }
                } else {
                    GateVerdict::Defer
                }
            },
            false,
        );
        let tmp = tempfile::tempdir().expect("tmp");
        let (session, seen) = claude_child(ctx, tmp.path(), Mode::Ask, gate.clone());
        feed(&session, &can_use_tool("stale", "touch a.tmp"));
        revoked.set(true);
        session.respond_approval("stale", Decision::Allow);
        assert_eq!(resolution(&seen, "stale"), Some(Decision::Deny));
        assert!(stdin_has(ctx, tmp.path(), "deny"));
        session.shutdown(false);

        revoked.set(false);
        let child = codex_child(Mode::Ask, gate, |_| {});
        child.session.send_prompt("plain-command");
        assert!(pump_until(ctx, 10, || !child
            .session
            .inner
            .open_approvals
            .borrow()
            .is_empty()));
        let request = child.session.inner.open_approvals.borrow()[0].clone();
        revoked.set(true);
        child.session.respond_approval(&request, Decision::Allow);
        assert_eq!(resolution(&child.seen, &request), Some(Decision::Deny));
        assert!(pump_until(ctx, 10, || !child.session.status().running_turn));
        child.session.shutdown(false);
    });
}

/// PR44 roots may fall back hookless, but a supervised agy child must never do so: Plan
/// without a hook can still edit files. The fresh per-start proof is mandatory in every mode.
#[test]
fn agy_final_launch_guard_holds_writes_and_records_only_actual_generation_facts() {
    for outcome in [0, 1, 2] {
        in_loop(|ctx| {
            let tmp = tempfile::tempdir().expect("tmp");
            let home = tmp.path().join("private-home");
            std::fs::create_dir(&home).expect("home");
            let store = Rc::new(Store::open_in_memory().expect("store"));
            let thread = fresh(&store, &tmp.path().to_string_lossy());
            let (sink, _) = make_sink();
            let hooks = Rc::new(TestHooks::default());
            hooks.hold_spawn.set(true);
            let session = ChatSession::with_options(
                Box::new(AgyAdapter::new("agy")),
                scripted(tmp.path(), "cat > \"$D/stdin.log\""),
                SessionStore::from(store),
                thread,
                sink,
                Err(None),
                LaunchEnv {
                    env: vec![
                        ("HOME".into(), home.to_string_lossy().into_owned()),
                        (
                            "UNRELATED_SECRET".into(),
                            "synthetic-not-in-launch-facts".into(),
                        ),
                    ],
                    unset: vec!["HOME".into()],
                },
                SessionOptions {
                    generation_base: ProcessGeneration(7),
                    hooks: Some(hooks.clone()),
                    child: None,
                },
            );
            assert!(pump_until(ctx, 10, || !hooks
                .launch_checks
                .borrow()
                .is_empty()));
            let facts = hooks.launch_checks.borrow()[0].clone();
            assert_eq!(facts.generation, ProcessGeneration(8));
            assert_eq!(
                facts.physical_cwd,
                tmp.path().canonicalize().expect("physical")
            );
            assert_eq!(facts.effective_home.as_ref(), Some(&home));
            assert!(!facts.hook_proven);
            assert!(!facts.argv.is_empty());
            assert!(session.delegation_launch_facts().is_none());
            session.send_prompt("held during final launch check");
            assert!(!tmp.path().join("args.log").exists());
            if outcome == 1 {
                session.shutdown(false);
            } else if outcome == 2 {
                session.rebind_stop();
                session.rebind_start();
                assert!(pump_until(ctx, 10, || hooks.launch_checks.borrow().len() == 2));
                session.send_prompt("replacement prompt");
            }
            hooks.hold_spawn.set(false);
            assert!(pump_until(ctx, 10, || hooks.completed_launch_checks.get() > 0));
            if outcome == 1 {
                assert!(!tmp.path().join("args.log").exists());
                assert!(session.delegation_launch_facts().is_none());
                assert_eq!(session.process_generation(), ProcessGeneration(7));
            } else {
                let expected = if outcome == 2 {
                    "replacement prompt"
                } else {
                    "held during final launch check"
                };
                assert!(stdin_has(ctx, tmp.path(), expected));
                if outcome == 2 {
                    // The rebound start's held writes never reach the replacement; the user's
                    // message behind them does, once, encoded afresh, ahead of the newer one.
                    let stdin =
                        std::fs::read_to_string(tmp.path().join("stdin.log")).expect("input");
                    let held = "held during final launch check";
                    assert_eq!(stdin.matches(held).count(), 1, "{stdin:?}");
                    assert!(
                        stdin.find(held) < stdin.find("replacement prompt"),
                        "{stdin:?}"
                    );
                }
                let actual = session.delegation_launch_facts().expect("successful facts");
                assert_eq!(actual.generation, ProcessGeneration(8));
                assert_eq!(actual.effective_home, Some(home));
                assert_eq!(actual.physical_cwd, facts.physical_cwd);
                assert_eq!(
                    hooks.asked.borrow().len(),
                    if outcome == 2 { 2 } else { 1 },
                    "getter must not consume another binding"
                );
                session.shutdown(false);
            }
        });
    }
}

#[test]
fn agy_final_launch_refusal_exposes_no_process_or_successful_facts() {
    in_loop(|ctx| {
        let tmp = tempfile::tempdir().expect("tmp");
        let store = Rc::new(Store::open_in_memory().expect("store"));
        let thread = fresh(&store, &tmp.path().to_string_lossy());
        let (sink, _) = make_sink();
        let hooks = Rc::new(TestHooks::default());
        hooks.refuse_spawn.set(true);
        let session = ChatSession::with_options(
            Box::new(AgyAdapter::new("agy")),
            scripted(tmp.path(), "cat > \"$D/stdin.log\""),
            SessionStore::from(store),
            thread,
            sink,
            Err(None),
            LaunchEnv::default(),
            SessionOptions {
                generation_base: ProcessGeneration(0),
                hooks: Some(hooks.clone()),
                child: None,
            },
        );
        session.send_prompt("must not escape");
        assert!(pump_until(ctx, 10, || !session.status().alive));
        assert!(!tmp.path().join("args.log").exists());
        assert!(session.delegation_launch_facts().is_none());
        assert_eq!(session.process_generation(), ProcessGeneration(0));
    });
}

/// A message sent into a start whose launch is then refused is kept, said so, and goes first
/// when the agent next starts: never dropped with the refused start's writes.
#[test]
fn a_refused_launch_keeps_its_message_and_sends_it_first_next_time() {
    in_loop(|ctx| {
        let tmp = tempfile::tempdir().expect("tmp");
        let store = Rc::new(Store::open_in_memory().expect("store"));
        let thread = fresh(&store, &tmp.path().to_string_lossy());
        let (sink, seen) = make_sink();
        let hooks = Rc::new(TestHooks::default());
        hooks.refuse_spawn.set(true);
        let session = ChatSession::with_options(
            Box::new(AgyAdapter::new("agy")),
            scripted(tmp.path(), "cat > \"$D/stdin.log\""),
            SessionStore::from(store),
            thread,
            sink,
            Err(None),
            LaunchEnv::default(),
            SessionOptions {
                generation_base: ProcessGeneration(0),
                hooks: Some(hooks.clone()),
                child: None,
            },
        );
        session.send_prompt("the kept message");
        assert!(pump_until(ctx, 10, || !session.status().alive));
        assert!(!tmp.path().join("args.log").exists(), "nothing spawned");
        let kept_notice = || {
            has(
                &seen,
                |e| matches!(e, Event::Notice { text } if text.contains("did not reach the agent")),
            )
        };
        assert!(kept_notice(), "the user is told the message was kept");

        // The cause is fixed: the next message starts the agent, the kept one goes first, and
        // the new one waits behind it (this agent never ends its turn).
        hooks.refuse_spawn.set(false);
        session.send_prompt("the next message");
        let stdin = tmp.path().join("stdin.log");
        let written = || std::fs::read_to_string(&stdin).unwrap_or_default();
        assert!(
            pump_until(ctx, 10, || written().contains("the kept message")),
            "the kept message reached the agent: {:?}",
            written()
        );
        assert!(
            !written().contains("the next message"),
            "the new message waits behind the kept one: {:?}",
            written()
        );
    });
}

/// An agy session with delegation hooks, for the tests below; `program` replaces the shell
/// that runs the scripted agent (a missing one fails every spawn, the first included).
fn held_agy(
    tmp: &std::path::Path,
    hooks: &Rc<TestHooks>,
    program: Option<&str>,
) -> (Rc<ChatSession>, Seen) {
    let store = Rc::new(Store::open_in_memory().expect("store"));
    let thread = fresh(&store, &tmp.to_string_lossy());
    let (sink, seen) = make_sink();
    let mut open = scripted(tmp, "cat > \"$D/stdin.log\"");
    if let Some(program) = program {
        open.program = program.into();
    }
    let session = ChatSession::with_options(
        Box::new(AgyAdapter::new("agy")),
        open,
        SessionStore::from(store),
        thread,
        sink,
        Err(None),
        LaunchEnv::default(),
        SessionOptions {
            generation_base: ProcessGeneration(0),
            hooks: Some(hooks.clone()),
            child: None,
        },
    );
    (session, seen)
}

/// A start torn down while its check runs (the supervisor failing the worker shuts the session
/// down; a switch stops it) queues its message again; the user's own stop drops it.
#[test]
fn a_torn_down_start_keeps_its_message_and_a_user_stop_drops_it() {
    in_loop(|ctx| {
        let tmp = tempfile::tempdir().expect("tmp");
        let hooks = Rc::new(TestHooks::default());
        hooks.hold_spawn.set(true);
        let (session, _seen) = held_agy(tmp.path(), &hooks, None);
        session.send_prompt("kept through the teardown");
        assert!(pump_until(ctx, 10, || !hooks
            .launch_checks
            .borrow()
            .is_empty()));
        session.inner.stop_current();
        assert!(pump_until(ctx, 10, || !session.status().alive));
        hooks.hold_spawn.set(false);
        session.send_prompt("sent after it");
        let stdin = tmp.path().join("stdin.log");
        let written = || std::fs::read_to_string(&stdin).unwrap_or_default();
        assert!(
            pump_until(ctx, 10, || written().contains("kept through the teardown")),
            "{:?}",
            written()
        );
        assert!(!written().contains("sent after it"), "{:?}", written());

        // The user's stop of a start still waiting drops what it held.
        let tmp = tempfile::tempdir().expect("tmp");
        let hooks = Rc::new(TestHooks::default());
        hooks.hold_spawn.set(true);
        let (session, _seen) = held_agy(tmp.path(), &hooks, None);
        session.send_prompt("stopped by the user");
        assert!(pump_until(ctx, 10, || !hooks
            .launch_checks
            .borrow()
            .is_empty()));
        session.interrupt();
        assert!(pump_until(ctx, 10, || !session.status().alive));
        hooks.hold_spawn.set(false);
        session.send_prompt("after the stop");
        let stdin = tmp.path().join("stdin.log");
        let written = || std::fs::read_to_string(&stdin).unwrap_or_default();
        assert!(pump_until(ctx, 10, || written().contains("after the stop")));
        assert!(
            !written().contains("stopped by the user"),
            "{:?}",
            written()
        );
    });
}

/// A start that fails to spawn opens no turn: its message is queued, and a newer one waits
/// behind it rather than starting the next process ahead of it.
#[test]
fn a_failed_spawn_keeps_its_message_ahead_of_newer_ones() {
    in_loop(|ctx| {
        let tmp = tempfile::tempdir().expect("tmp");
        let hooks = Rc::new(TestHooks::default());
        let (session, seen) = held_agy(
            tmp.path(),
            &hooks,
            Some("/nonexistent/agent-terminal-test-agy"),
        );
        session.send_prompt("first, through a failed spawn");
        assert!(pump_until(ctx, 10, || {
            has(
                &seen,
                |e| matches!(e, Event::Notice { text } if text.contains("did not reach the agent")),
            )
        }));
        assert!(!session.status().alive);
        assert!(!session.inner.state.borrow().running_turn, "no turn opened");
        session.inner.open.borrow_mut().program = "/bin/sh".into();
        session.send_prompt("second");
        let stdin = tmp.path().join("stdin.log");
        let written = || std::fs::read_to_string(&stdin).unwrap_or_default();
        assert!(
            pump_until(ctx, 10, || written()
                .contains("first, through a failed spawn")),
            "{:?}",
            written()
        );
        assert!(!written().contains("second"), "{:?}", written());
    });
}

#[test]
fn shutdown_cancels_a_pending_proof_even_if_the_process_is_already_dead() {
    in_loop(|ctx| {
        let tmp = tempfile::tempdir().expect("tmp");
        let (session, _) = claude_child(ctx, tmp.path(), Mode::Ask, TestGate::defer(false));
        session.shutdown(false);
        let generation = session.inner.generation.get();
        *session.inner.starting.borrow_mut() = Some(PendingStart {
            generation,
            writes: vec!["must not reach a replacement process".to_owned()],
            prompts: Vec::new(),
        });
        let process_generation = session.process_generation();
        session.shutdown(false);
        assert!(session.inner.starting.borrow().is_none());
        assert!(session.inner.pre_init_prompts.borrow().is_empty());
        session.inner.finish_proven_start(generation, Ok(()));
        assert_eq!(session.process_generation(), process_generation);
        assert!(session.inner.proc.borrow().is_none());
        assert!(!session.status().alive);
    });
}

#[test]
fn root_prompt_gate_holds_direct_and_automatic_queued_input_without_losing_it() {
    for late in [false, true] {
        in_loop(|ctx| {
            let tmp = tempfile::tempdir().expect("tmp");
            let store = Rc::new(Store::open_in_memory().expect("store"));
            let thread = fresh(&store, &tmp.path().to_string_lossy());
            let (sink, _) = make_sink();
            let hooks = Rc::new(TestHooks::default());
            hooks.hold_prompt.set(!late);
            hooks.hold_after_check.set(late.then_some(1));
            let session = ChatSession::with_options(
                Box::new(agent_core::claude::ClaudeAdapter::new()),
                scripted(tmp.path(), "cat > \"$D/stdin.log\""),
                SessionStore::from(store),
                thread,
                sink,
                Err(None),
                LaunchEnv::default(),
                SessionOptions {
                    generation_base: ProcessGeneration(0),
                    hooks: Some(hooks.clone()),
                    child: None,
                },
            );
            session.send_prompt("held direct prompt");
            session.queue_prompt("held queued prompt");
            session.inner.dispatch_queued_prompt();
            assert_eq!(session.inner.queued_prompts.borrow().len(), 2);
            assert_eq!(session.inner.queued_prompts.borrow()[0].stored, late);
            assert!(!session.inner.prompt_in_flight.get());
            assert!(hooks.prompt_checks.get() >= 2);
            hooks.hold_prompt.set(false);
            hooks.hold_after_check.set(None);
            session.inner.dispatch_queued_prompt();
            assert!(stdin_has(ctx, tmp.path(), "held direct prompt"));
            assert_eq!(session.inner.queued_prompts.borrow().len(), 1);
            assert_eq!(
                session.inner.queued_prompts.borrow()[0].text,
                "held queued prompt"
            );
            session.shutdown(false);
        });
    }
}

#[test]
fn an_agy_child_never_falls_back_to_a_hookless_process() {
    for mode in [Mode::Plan, Mode::Ask, Mode::AcceptEdits] {
        in_loop(|ctx| {
            let tmp = tempfile::tempdir().expect("tmp");
            let store = Rc::new(Store::open_in_memory().expect("store"));
            let thread = fresh(&store, &tmp.path().to_string_lossy());
            let (sink, seen) = make_sink();
            set_test_hook_check(std::sync::Arc::new(|_| {
                Err("workspace hook appeared".into())
            }));
            let handle = ApprovalHandle::bind(
                &tmp.path().join("rt"),
                "child",
                tmp.path(),
                mode,
                Duration::from_secs(5),
            )
            .expect("socket");
            let mut open = fake_agy(tmp.path());
            open.mode = mode;
            let session = ChatSession::with_options(
                Box::new(AgyAdapter::new("agy")),
                open,
                SessionStore::from(store),
                thread,
                sink,
                Ok(handle),
                LaunchEnv::default(),
                SessionOptions {
                    generation_base: ProcessGeneration(0),
                    hooks: Some(Rc::new(TestHooks::default())),
                    child: Some(ChildPolicy {
                        gate: TestGate::defer(false),
                        publish: Rc::new(TestPublish::default()),
                        mode,
                    }),
                },
            );
            assert!(pump_until(ctx, 10, || !session.status().alive));
            assert_eq!(session.process_generation(), ProcessGeneration(0));
            assert!(!tmp.path().join("args.log").exists());
            assert!(has(
                &seen,
                |event| matches!(event, Event::Error {message} if message.contains("not started"))
            ));
            session.shutdown(false);
        });
    }
}

/// Claude: a Plan child's edit, command and unknown tool are denied before any
/// card or rule; a read passes to the existing policy.
#[test]
fn plan_denies_edit_exec_and_unknown_for_claude() {
    in_loop(|ctx| {
        let tmp = tempfile::tempdir().expect("tmp");
        let (session, seen) = claude_child(ctx, tmp.path(), Mode::Plan, plan_gate());
        let file = tmp.path().join("a.txt").to_string_lossy().into_owned();
        feed(
            &session,
            &claude_tool("edit", "Edit", json!({"file_path": file})),
        );
        feed(
            &session,
            &claude_tool("exec", "Bash", json!({"command": "true"})),
        );
        feed(&session, &claude_tool("unknown", "Frobnicate", json!({})));
        feed(
            &session,
            &claude_tool("read", "Read", json!({"file_path": file})),
        );
        for id in ["edit", "exec", "unknown"] {
            assert_eq!(resolution(&seen, id), Some(Decision::Deny), "{id}");
            assert!(stdin_has(ctx, tmp.path(), &format!("\"{id}\"")), "{id}");
        }
        assert_eq!(
            resolution(&seen, "read"),
            None,
            "a read is the policy's to ask"
        );
        session.shutdown(false);
    });
}

/// Codex: a Plan child's patch, command, permission request and unresolved MCP
/// call (unknown) are all declined at once, before any card can be answered.
#[test]
fn plan_denies_edit_exec_and_unknown_for_codex() {
    let unknown = format!(
        "form:{}",
        json!({"mode": "form", "message": "Allow the synthetic MCP server to run tool \"x\"?",
               "requestedSchema": {"type": "object", "properties": {}},
               "_meta": {"codex_approval_kind": "mcp_tool_call"}})
    );
    let permissions = format!(
        "permissions:{}",
        json!({"fileSystem": {"write": ["synthetic"]}})
    );
    for scenario in [
        "file-change",
        "plain-command",
        permissions.as_str(),
        unknown.as_str(),
    ] {
        in_loop(|ctx| {
            let gate = plan_gate();
            let child = codex_child(Mode::Plan, gate.clone(), |_| {});
            child.session.send_prompt(scenario);
            assert!(
                pump_until(ctx, 10, || has(&child.seen, |e| matches!(
                    e,
                    Event::ApprovalResolved {
                        decision: Decision::Deny
                    }
                ))),
                "{scenario}: {:?}",
                child.seen.borrow()
            );
            assert!(
                gate.log.borrow().iter().any(|c| c.starts_with("judge ")),
                "{scenario}"
            );
            // The scripted server saw the decline and finished the turn.
            assert!(
                pump_until(ctx, 10, || !child.session.status().running_turn),
                "{scenario}: {:?}",
                child.frames()
            );
            child.session.shutdown(false);
        });
    }
}

// ---- modes ----

#[test]
fn a_child_never_leaves_its_task_mode_nor_enters_bypass() {
    in_loop(|ctx| {
        let tmp = tempfile::tempdir().expect("tmp");
        let (session, seen) = claude_child(ctx, tmp.path(), Mode::Ask, TestGate::defer(false));
        session.set_mode(Mode::AcceptEdits);
        session.set_mode(Mode::Bypass);
        assert_eq!(session.effective_mode(), Mode::Ask);
        assert!(has(&seen, |e| matches!(e,
            Event::Notice { text } if text.contains("cannot leave its task's mode"))));
        session.set_mode(Mode::Plan);
        assert!(stdin_has(ctx, tmp.path(), "set_permission_mode"));
        let request: Value = stdin_log(tmp.path())
            .lines()
            .filter_map(|line| serde_json::from_str::<Value>(line).ok())
            .find(|frame| frame["request"]["subtype"] == "set_permission_mode")
            .expect("mode request");
        feed(
            &session,
            &json!({"type":"control_response", "response": {
                "subtype":"success", "request_id":request["request_id"], "response":{}
            }})
            .to_string(),
        );
        assert!(pump_until(ctx, 5, || session.effective_mode() == Mode::Plan));
        session.shutdown(false);
    });
}

#[test]
fn a_codex_ask_child_requests_workspace_write_and_reads_it_back() {
    in_loop(|ctx| {
        let child = codex_child(Mode::Ask, TestGate::defer(false), |_| {});
        assert!(pump_until(ctx, 10, || !child
            .sent("thread/start")
            .is_empty()));
        let start = &child.sent("thread/start")[0];
        assert_eq!(start["params"]["approvalPolicy"], "untrusted");
        assert_eq!(start["params"]["sandbox"], "workspace-write");
        let result = child
            .frames()
            .into_iter()
            .find(|f| f["dir"] == "out" && f["frame"]["id"] == start["id"])
            .expect("thread/start result");
        assert_eq!(
            result["frame"]["result"]["sandbox"]["type"],
            "workspaceWrite"
        );
        child.session.send_prompt("success");
        assert!(pump_until(ctx, 10, || !child.sent("turn/start").is_empty()));
        let turn = &child.sent("turn/start")[0];
        assert_eq!(turn["params"]["sandboxPolicy"]["type"], "workspaceWrite");
        assert_eq!(turn["params"]["approvalPolicy"], "untrusted");
        child.session.shutdown(false);
    });
}

// ---- root isolation ----

/// A child's turn, model change and usage reach its own sink and publisher only; the
/// root session's state, sink and store are untouched.
#[test]
fn a_child_turn_model_change_and_usage_never_touch_the_root() {
    in_loop(|ctx| {
        let root_dir = tempfile::tempdir().expect("root");
        let root_store = Rc::new(Store::open_in_memory().expect("store"));
        let root_thread = fresh(&root_store, &root_dir.path().to_string_lossy());
        let (root_sink, root_seen) = make_sink();
        let root = ChatSession::with_env(
            Box::new(CodexAdapter::new()),
            codex_open(root_dir.path(), Mode::Ask),
            root_store.clone(),
            root_thread.clone(),
            root_sink,
            Err(None),
            bare_env(root_dir.path()),
        );
        assert!(pump_until(ctx, 10, || has(&root_seen, |e| matches!(
            e,
            Event::SessionStarted { .. }
        ))));
        let root_before = root.status();
        let root_events = root_seen.borrow().len();
        let root_stored = root_store
            .events(&root_thread, None, 10_000)
            .expect("events")
            .len();

        let child = codex_child(Mode::Ask, TestGate::defer(false), |dir| {
            std::fs::write(dir.join("startup-model"), "child-model").expect("model");
        });
        assert_eq!(
            child.session.deliver_delegated(task("go\nsuccess")),
            DelegatedOutcome::TurnStarted
        );
        assert!(pump_until(ctx, 10, || child
            .hooks
            .turns
            .borrow()
            .iter()
            .any(|t| t.contains("ended"))));
        assert_eq!(
            child.session.status().model.as_deref(),
            Some("synthetic-model")
        );
        assert!(child
            .publish
            .published
            .borrow()
            .iter()
            .any(|e| matches!(e.event, Event::TurnCompleted { .. })));

        let root_after = root.status();
        assert_eq!(root_after.model, root_before.model);
        assert_eq!(root_after.mode, root_before.mode);
        assert_eq!(root_after.running_turn, root_before.running_turn);
        assert_eq!(
            root_seen.borrow().len(),
            root_events,
            "no child event in the root's sink"
        );
        assert_eq!(
            root_store
                .events(&root_thread, None, 10_000)
                .expect("events")
                .len(),
            root_stored
        );
        assert!(!root_seen.borrow().iter().any(|e| matches!(
            e.event,
            Event::TurnCompleted { .. } | Event::UsageUpdated { .. } | Event::ModelChanged { .. }
        )));
        child.session.shutdown(false);
        root.shutdown(false);
    });
}

// ---- generations, exit watches and rebinds ----

/// One watch per generation, kept until that generation is seen to exit; `process_stopping`
/// (the synthetic stop) runs once per generation, before the exit, and frees nothing; a rebind
/// starts nothing until `rebind_start`, and the next generation asks for its own binding.
#[test]
fn every_generation_keeps_its_exit_watch_until_it_has_exited() {
    in_loop(|ctx| {
        let child = codex_child(Mode::Ask, TestGate::defer(false), |_| {});
        let hooks = child.hooks.clone();
        assert_eq!(child.session.process_generation(), ProcessGeneration(1));
        assert_eq!(*hooks.asked.borrow(), vec![(Driver::Codex, 1)]);
        assert_eq!(hooks.started.borrow().len(), 1);
        assert!(hooks.started.borrow()[0].1.is_some(), "the pid is reported");
        assert_eq!(child.session.exit_watches().len(), 1);
        assert!(pump_until(ctx, 10, || !hooks.lines.borrow().is_empty()));
        assert!(pump_until(ctx, 10, || hooks.observed.get() > 0));

        let watch = child
            .session
            .rebind_stop()
            .expect("the running generation's watch");
        let stopping = hooks.stopping.borrow().clone();
        assert_eq!(stopping.len(), 1);
        assert_eq!(stopping[0].0, 1);
        assert_ne!(
            stopping[0].1,
            Some(StopPhase::Exited),
            "the synthetic stop comes before the exit"
        );
        assert_eq!(*child.publish.cancelled.borrow(), vec![1]);
        assert_eq!(
            child.session.exit_watches().len(),
            1,
            "kept until it has exited"
        );

        // While rebinding nothing starts: a prompt waits, a delegated input is refused.
        child.session.send_prompt("success");
        assert!(!child.session.status().alive);
        assert_eq!(child.session.inner.queued_prompts.borrow().len(), 1);
        assert_eq!(
            child.session.deliver_delegated(task("x")),
            DelegatedOutcome::Refused(agent_core::delegation::ErrorCode::RuntimeUnavailable)
        );
        assert_eq!(child.session.inner.queued_prompts.borrow().len(), 1);
        // A second stop of the same generation reports nothing again.
        child.session.inner.stop_current();
        assert_eq!(hooks.stopping.borrow().len(), 1);

        assert!(pump_until(ctx, 10, || watch.phase() == StopPhase::Exited));
        assert!(child.session.exit_watches().is_empty());

        child.session.rebind_start();
        assert_eq!(child.session.process_generation(), ProcessGeneration(2));
        assert_eq!(hooks.asked.borrow().last(), Some(&(Driver::Codex, 2)));
        assert_eq!(hooks.started.borrow().len(), 2);
        assert_eq!(child.session.exit_watches().len(), 1);
        // The queued prompt went out on the new generation.
        assert!(pump_until(ctx, 10, || !child.sent("turn/start").is_empty()));
        assert!(pump_until(ctx, 10, || !child.session.status().running_turn));
        assert!(child.session.inner.queued_prompts.borrow().is_empty());

        // Two generations alive at once are both returned (a rebind leftover).
        let old = child.session.exit_watch().expect("generation 2");
        child.session.inner.stop_current();
        child.session.inner.rebinding.set(false);
        child.session.inner.ensure_alive();
        let watches = child.session.exit_watches();
        if old.phase() != StopPhase::Exited {
            assert_eq!(watches.len(), 2);
        }
        child.session.shutdown(false);
        assert!(pump_until(ctx, 10, || child
            .session
            .exit_watches()
            .is_empty()));
    });
}

/// A binding taken for a start that never spawned is released (its token and overlay file
/// would otherwise outlive it), and only then: a start that spawns keeps its binding until
/// the process's own exit ends it.
#[test]
fn a_refused_launch_releases_its_binding_and_a_spawned_one_keeps_it() {
    in_loop(|ctx| {
        let tmp = tempfile::tempdir().expect("tmp");
        let store = Rc::new(Store::open_in_memory().expect("store"));
        let thread = fresh(&store, &tmp.path().to_string_lossy());
        let (sink, _seen) = make_sink();
        let hooks = Rc::new(TestHooks::default());
        let stage = || {
            *hooks.staged.borrow_mut() = Some((
                1,
                LaunchBinding {
                    open: None,
                    env: vec![("AGENT_TERMINAL_DELEGATION_ENDPOINT".into(), "/run/x".into())],
                    argv_tail: Vec::new(),
                },
            ));
        };
        stage();
        hooks.refuse_spawn.set(true);
        let session = ChatSession::with_options(
            Box::new(AgyAdapter::new("agy")),
            scripted(tmp.path(), "cat > \"$D/stdin.log\""),
            SessionStore::from(store),
            thread,
            sink,
            Err(None),
            LaunchEnv::default(),
            SessionOptions {
                generation_base: ProcessGeneration(0),
                hooks: Some(hooks.clone()),
                child: None,
            },
        );
        session.send_prompt("refused");
        assert!(pump_until(ctx, 10, || !session.status().alive));
        assert_eq!(
            *hooks.abandoned.borrow(),
            vec![1],
            "the refused start's binding"
        );

        stage();
        hooks.refuse_spawn.set(false);
        session.send_prompt("spawned");
        assert!(pump_until(ctx, 10, || !hooks.started.borrow().is_empty()));
        assert_eq!(
            *hooks.abandoned.borrow(),
            vec![1],
            "a spawned start's binding is not released by the session"
        );
    });
}

/// `launch_spec`: the binding staged for the next generation is taken once: its argv tail
/// follows the adapter's argv, its environment follows the profile's, and every inherited
/// delegation variable is removed first. A start nothing was staged for gets no binding.
#[test]
fn the_staged_binding_is_used_once_and_inherited_credentials_are_stripped() {
    in_loop(|ctx| {
        let tmp = tempfile::tempdir().expect("tmp");
        let store = Rc::new(Store::open_in_memory().expect("store"));
        let thread = fresh(&store, &tmp.path().to_string_lossy());
        let (sink, _seen) = make_sink();
        let hooks = Rc::new(TestHooks::default());
        *hooks.staged.borrow_mut() = Some((
            8,
            LaunchBinding {
                open: None,
                env: vec![("AGENT_TERMINAL_DELEGATION_ENDPOINT".into(), "/run/x".into())],
                argv_tail: vec!["--mcp-config".into(), "/run/overlay.json".into()],
            },
        ));
        let open = scripted(tmp.path(), "cat > \"$D/stdin.log\"");
        let session = ChatSession::with_options(
            Box::new(agent_core::claude::ClaudeAdapter::new()),
            open,
            SessionStore::from(store),
            thread,
            sink,
            Err(None),
            LaunchEnv::default(),
            SessionOptions {
                generation_base: ProcessGeneration(7),
                hooks: Some(hooks.clone()),
                child: None,
            },
        );
        assert!(pump_until(ctx, 10, || {
            std::fs::read_to_string(tmp.path().join("args.log")).is_ok_and(|a| !a.is_empty())
        }));
        let args = std::fs::read_to_string(tmp.path().join("args.log")).expect("args");
        assert_eq!(session.process_generation(), ProcessGeneration(8));
        assert!(args.contains("--replay-user-messages"));
        assert!(
            args.trim_end().ends_with("--mcp-config /run/overlay.json"),
            "{args}"
        );
        // A reopened principal starts above its persisted floor; generation 9 has no binding.
        let spec = session.launch_spec();
        assert!(!spec.argv.iter().any(|a| a == "--mcp-config"));
        assert!(!spec
            .env
            .iter()
            .any(|(k, _)| k.starts_with("AGENT_TERMINAL_DELEGATION_")));
        for name in agent_core::delegation::env::STRIP_ON_SPAWN {
            assert!(spec.unset.iter().any(|u| u == name), "{name}");
        }
        assert_eq!(session.inner.open.borrow().delegation, None);
        assert_eq!(
            *hooks.asked.borrow(),
            vec![(Driver::Claude, 8), (Driver::Claude, 9)]
        );
        session.shutdown(false);
    });
}

#[test]
fn only_a_bound_dynamic_bridge_is_auto_allowed_and_the_child_gate_still_wins() {
    for (bound, source, deny) in [
        (false, "dynamic", false),
        (true, "user", false),
        (true, "dynamic", false),
        (true, "dynamic", true),
    ] {
        in_loop(|ctx| {
            let tmp = tempfile::tempdir().expect("tmp");
            let gate = TestGate::new(
                move |_, _| {
                    if deny {
                        GateVerdict::Deny {
                            reason: "bootstrap".into(),
                        }
                    } else {
                        GateVerdict::Defer
                    }
                },
                false,
            );
            let (session, seen) = claude_child(ctx, tmp.path(), Mode::Ask, gate);
            session.inner.bound_generation.set(bound);
            let mut request: Value = serde_json::from_str(&claude_tool(
                "bridge",
                "mcp__agent_terminal__cross_agent_list",
                json!({}),
            ))
            .expect("request");
            request["request"]["mcp_server"] = json!({"name":"agent_terminal","source":source});
            feed(&session, &request.to_string());
            if deny {
                assert_eq!(resolution(&seen, "bridge"), Some(Decision::Deny));
            } else if bound && source == "dynamic" {
                assert!(pump_until(ctx, 10, || resolution(&seen, "bridge").is_some()));
                assert_eq!(resolution(&seen, "bridge"), Some(Decision::Allow));
            } else {
                assert_eq!(resolution(&seen, "bridge"), None);
            }
            session.shutdown(false);
        });
    }
}

/// Revocation ends every open approval: Claude's are answered Deny with the reason.
#[test]
fn expiring_approvals_denies_every_open_claude_request() {
    in_loop(|ctx| {
        let tmp = tempfile::tempdir().expect("tmp");
        let (session, seen) = claude_child(ctx, tmp.path(), Mode::Ask, TestGate::defer(false));
        feed(&session, &can_use_tool("r1", "touch a"));
        feed(&session, &can_use_tool("r2", "touch b"));
        assert_eq!(resolution(&seen, "r1"), None);
        session.expire_approvals("the root stopped");
        assert_eq!(resolution(&seen, "r1"), Some(Decision::Deny));
        assert_eq!(resolution(&seen, "r2"), Some(Decision::Deny));
        assert!(stdin_has(ctx, tmp.path(), "the root stopped"));
        session.shutdown(false);
    });
}

/// Hooks see every raw line, including a tool-server status frame mid-turn, and the turn
/// boundaries with the native turn id.
#[test]
fn hooks_see_raw_lines_and_turn_boundaries_with_the_native_turn() {
    in_loop(|ctx| {
        let child = codex_child(Mode::Ask, TestGate::defer(false), |_| {});
        assert_eq!(
            child.session.deliver_delegated(task("go\nmcp:codex_apps")),
            DelegatedOutcome::TurnStarted
        );
        assert!(pump_until(ctx, 10, || child
            .hooks
            .lines
            .borrow()
            .iter()
            .any(
                |l| l.contains("mcpServer/startupStatus/updated") && l.contains("codex_apps")
            )));
        assert!(child
            .hooks
            .turns
            .borrow()
            .iter()
            .any(|t| t.starts_with("1 started connection-1-turn-")));
        child.session.interrupt_run();
        assert!(pump_until(ctx, 10, || child
            .hooks
            .turns
            .borrow()
            .iter()
            .any(|t| t.starts_with("1 ended"))));
        child.session.shutdown(false);
    });
}

/// A delegation root is suspended only with its hooks' consent (a planned rotation, nothing
/// revoked), and the start that follows is known as that resume, then forgotten.
#[test]
fn a_delegation_root_suspends_only_with_its_hooks_consent() {
    in_loop(|ctx| {
        let dir = tempfile::tempdir().expect("dir");
        let store = Rc::new(Store::open_in_memory().expect("store"));
        let thread = fresh(&store, &dir.path().to_string_lossy());
        let (sink, seen) = make_sink();
        let hooks = Rc::new(TestHooks::default());
        let session = ChatSession::with_options(
            Box::new(agent_core::claude::ClaudeAdapter::new()),
            super::idle_claude(dir.path()),
            SessionStore::from(store),
            thread,
            sink,
            Err(None),
            bare_env(dir.path()),
            SessionOptions {
                generation_base: ProcessGeneration(0),
                hooks: Some(hooks.clone()),
                child: None,
            },
        );
        session.reload_session();
        assert!(pump_until(ctx, 10, || has(&seen, |e| matches!(
            e,
            Event::TurnCompleted { .. }
        ))));
        // A prompted turn, so there is a conversation to resume.
        session.send_prompt("first");
        assert!(pump_until(ctx, 10, || session.inner.has_conversation.get()
            && !session.status().running_turn));
        let generation = session.process_generation().0;

        assert!(!session.suspend_if_idle(), "the hooks refused");
        assert_eq!(*hooks.suspend_asks.borrow(), vec![generation]);
        assert!(session.status().alive, "a refusal stops nothing");
        assert!(!session.resumes_from_suspension());

        hooks.allow_suspend.set(true);
        assert!(session.suspend_if_idle(), "the hooks agreed");
        assert!(!session.status().alive);
        assert!(session.resumes_from_suspension());
        assert!(
            hooks
                .stopping
                .borrow()
                .iter()
                .any(|(g, _)| *g == generation),
            "the generation was closed out (as a rotation, by the supervisor's consent)"
        );

        session.send_prompt("back again");
        assert!(pump_until(ctx, 10, || session.status().alive));
        assert!(
            !session.resumes_from_suspension(),
            "a started process is no longer that resume"
        );
    });
}

/// Sub-agent news offered to a root: it waits while a turn runs, resumes a suspended process,
/// starts a turn shown as the app's notice (never a user message), is only a notice once the
/// agent was woken too often without the user, and never wakes a root the user stopped. The
/// user speaking ends both.
#[test]
fn a_root_is_woken_by_news_only_when_idle_within_the_cap_and_never_after_a_stop() {
    use crate::chat::session::WakeOutcome;
    use agent_core::delegation::wake::MAX_CONSECUTIVE_WAKES;
    in_loop(|ctx| {
        let dir = tempfile::tempdir().expect("dir");
        let store = Rc::new(Store::open_in_memory().expect("store"));
        let thread = fresh(&store, &dir.path().to_string_lossy());
        let (sink, seen) = make_sink();
        let hooks = Rc::new(TestHooks::default());
        hooks.allow_suspend.set(true);
        let session = ChatSession::with_options(
            Box::new(agent_core::claude::ClaudeAdapter::new()),
            super::idle_claude(dir.path()),
            SessionStore::from(store),
            thread,
            sink,
            Err(None),
            bare_env(dir.path()),
            SessionOptions {
                generation_base: ProcessGeneration(0),
                hooks: Some(hooks.clone()),
                child: None,
            },
        );
        let prompts =
            || std::fs::read_to_string(dir.path().join("prompts.log")).unwrap_or_default();
        let idle = |s: &ChatSession| s.status().alive && !s.status().running_turn;
        assert!(pump_until(ctx, 10, || has(&seen, |e| matches!(
            e,
            Event::TurnCompleted { .. }
        )) && idle(&session)));
        session.send_prompt("first");
        assert!(pump_until(ctx, 10, || session.inner.has_conversation.get()
            && idle(&session)));

        // A running turn is never cut into.
        session.send_prompt("second");
        assert!(session.status().running_turn);
        assert_eq!(session.wake_with("news while busy"), WakeOutcome::Busy);
        assert!(pump_until(ctx, 10, || idle(&session)));
        assert!(!prompts().contains("news while busy"));

        // A suspended process is resumed for the news.
        assert!(session.suspend_if_idle());
        assert_eq!(
            session.wake_with("news after a pause"),
            WakeOutcome::Started
        );
        assert!(pump_until(ctx, 10, || prompts().contains("news after a pause")));
        assert!(has(&seen, |e| matches!(
            e,
            Event::Notice { text } if text == "news after a pause"
        )));
        assert!(!has(&seen, |e| matches!(
            e,
            Event::ContentSnapshot { text, .. } if text.contains("news after a pause")
        )));
        assert!(pump_until(ctx, 10, || idle(&session)));

        // Past the cap, a notice only.
        session.inner.auto_wakes.set(MAX_CONSECUTIVE_WAKES);
        assert_eq!(session.wake_with("news past the cap"), WakeOutcome::Noticed);
        assert!(has(&seen, |e| matches!(
            e,
            Event::Notice { text } if text.starts_with("news past the cap")
                && text.contains("Not sent to the agent")
        )));
        assert!(!session.status().running_turn);
        assert!(!prompts().contains("news past the cap"));

        // The user speaking resets the count.
        session.send_prompt("go on");
        assert!(pump_until(ctx, 10, || idle(&session)));
        assert_eq!(session.wake_with("news in a new run"), WakeOutcome::Started);
        assert_eq!(session.inner.auto_wakes.get(), 1);
        assert!(pump_until(ctx, 10, || idle(&session)));

        // A stopped root takes none until the user speaks again.
        session.hold_wakes();
        assert_eq!(session.wake_with("news after a stop"), WakeOutcome::Held);
        assert!(!has(&seen, |e| matches!(
            e,
            Event::Notice { text } if text.contains("news after a stop")
        )));
        session.send_prompt("carry on");
        assert!(pump_until(ctx, 10, || idle(&session)));
        assert_eq!(session.wake_with("news later"), WakeOutcome::Started);
        session.shutdown(false);
    });
}

/// A wake whose send is refused posts no notice (the supervisor offers it again each second,
/// and each offer would otherwise leave one); the retry that is sent posts exactly one.
#[test]
fn a_refused_wake_posts_no_notice_and_its_retry_posts_one() {
    use crate::chat::session::WakeOutcome;
    in_loop(|ctx| {
        let dir = tempfile::tempdir().expect("dir");
        let store = Rc::new(Store::open_in_memory().expect("store"));
        let thread = fresh(&store, &dir.path().to_string_lossy());
        let (sink, seen) = make_sink();
        let hooks = Rc::new(TestHooks::default());
        let session = ChatSession::with_options(
            Box::new(agent_core::claude::ClaudeAdapter::new()),
            super::idle_claude(dir.path()),
            SessionStore::from(store),
            thread,
            sink,
            Err(None),
            bare_env(dir.path()),
            SessionOptions {
                generation_base: ProcessGeneration(0),
                hooks: Some(hooks.clone()),
                child: None,
            },
        );
        let prompts =
            || std::fs::read_to_string(dir.path().join("prompts.log")).unwrap_or_default();
        let notices = || {
            seen.borrow()
                .iter()
                .filter(|e| matches!(&e.event, Event::Notice { text } if text == "news"))
                .count()
        };
        assert!(pump_until(ctx, 10, || has(&seen, |e| matches!(
            e,
            Event::TurnCompleted { .. }
        )) && session.status().alive
            && !session.status().running_turn));

        // `start_wake`'s own gate passes; the send's gate refuses it.
        hooks
            .hold_after_check
            .set(Some(hooks.prompt_checks.get() + 1));
        for _ in 0..3 {
            assert_eq!(session.wake_with("news"), WakeOutcome::Busy);
        }
        assert_eq!(notices(), 0, "a refused send leaves no notice");
        assert!(!session.status().running_turn);
        assert!(!session.inner.prompt_in_flight.get());
        assert_eq!(session.inner.auto_wakes.get(), 0);

        hooks.hold_after_check.set(None);
        assert_eq!(session.wake_with("news"), WakeOutcome::Started);
        assert_eq!(notices(), 1);
        assert!(pump_until(ctx, 10, || prompts().contains("news")));
        assert!(pump_until(ctx, 10, || !session.status().running_turn));
        assert_eq!(notices(), 1, "exactly one notice for the one wake");
        session.shutdown(false);
    });
}

/// The supervisor's held news, faked: what it gives, what was given back, and releases.
#[derive(Default)]
struct FakeNews {
    text: Rc<RefCell<Option<String>>>,
    taken: Cell<usize>,
    given_back: Rc<Cell<usize>>,
    released: Cell<usize>,
}

impl crate::chat::session::HeldNews for FakeNews {
    fn take(&self) -> Option<crate::chat::session::TakenNews> {
        let text = self.text.borrow_mut().take()?;
        self.taken.set(self.taken.get() + 1);
        let (slot, back) = (self.text.clone(), self.given_back.clone());
        let kept = text.clone();
        Some(crate::chat::session::TakenNews {
            text,
            give_back: Box::new(move || {
                *slot.borrow_mut() = Some(kept);
                back.set(back.get() + 1);
            }),
        })
    }

    fn release(&self) {
        self.released.set(self.released.get() + 1);
    }
}

/// Dangerous permissive mode never starts a turn nobody asked for: news offered to an idle root
/// in it is held for the user (no prompt, no notice from the session), however often it is
/// offered. The user's next message carries it, shown as the app's notice; a message that is
/// not sent gives it back and leaves no notice; a slash command never carries it; leaving the
/// mode releases it, and an ordinary wake then starts.
#[test]
fn a_root_in_dangerous_permissive_mode_takes_news_only_with_the_users_message() {
    use crate::chat::session::WakeOutcome;
    in_loop(|ctx| {
        let dir = tempfile::tempdir().expect("dir");
        let store = Rc::new(Store::open_in_memory().expect("store"));
        let thread = fresh(&store, &dir.path().to_string_lossy());
        let (sink, seen) = make_sink();
        let hooks = Rc::new(TestHooks::default());
        let session = ChatSession::with_options(
            Box::new(agent_core::claude::ClaudeAdapter::new()),
            super::idle_claude(dir.path()),
            SessionStore::from(store),
            thread,
            sink,
            Err(None),
            bare_env(dir.path()),
            SessionOptions {
                generation_base: ProcessGeneration(0),
                hooks: Some(hooks.clone()),
                child: None,
            },
        );
        let prompts =
            || std::fs::read_to_string(dir.path().join("prompts.log")).unwrap_or_default();
        let notices = |needle: &str| {
            seen.borrow()
                .iter()
                .filter(|e| matches!(&e.event, Event::Notice { text } if text.contains(needle)))
                .count()
        };
        let idle = |s: &ChatSession| s.status().alive && !s.status().running_turn;
        assert!(pump_until(ctx, 10, || has(&seen, |e| matches!(
            e,
            Event::TurnCompleted { .. }
        )) && idle(&session)));

        session.confirm_mode(Mode::Bypass);
        assert_eq!(session.status().mode, Mode::Bypass);
        for _ in 0..3 {
            assert_eq!(session.wake_with("news held"), WakeOutcome::HeldForUser);
        }
        assert!(!session.status().running_turn);
        assert!(!session.inner.prompt_in_flight.get());
        assert_eq!(session.inner.auto_wakes.get(), 0);
        assert_eq!(
            notices("news held"),
            0,
            "the supervisor tells the thread, once"
        );
        assert!(!prompts().contains("news held"));

        let news = Rc::new(FakeNews::default());
        *news.text.borrow_mut() = Some("[agent-terminal] held news".into());
        session.hold_news_for_user(news.clone());

        // A slash command never carries it.
        session.send_prompt("/hello");
        assert_eq!(news.taken.get(), 0);
        assert!(pump_until(ctx, 10, || idle(&session)));

        // A message whose send is refused gives it back and leaves no notice.
        hooks
            .hold_after_check
            .set(Some(hooks.prompt_checks.get() + 1));
        session.send_prompt("first try");
        assert_eq!(news.taken.get(), 1);
        assert_eq!(news.given_back.get(), 1);
        assert_eq!(notices("held news"), 0);
        assert!(!prompts().contains("held news"));

        // Sent from the queue once the send is allowed: it carries the news, and the thread
        // shows the news as the app's notice, not as the user's message.
        hooks.hold_after_check.set(None);
        session.inner.dispatch_queued_prompt();
        assert!(pump_until(ctx, 10, || prompts().contains("first try")));
        let sent = prompts();
        assert!(sent.contains("held news"), "{sent}");
        assert!(sent.contains("The user's message follows."), "{sent}");
        assert_eq!(news.taken.get(), 2);
        assert_eq!(notices("held news"), 1);
        assert!(!has(&seen, |e| matches!(
            e,
            Event::ContentSnapshot { text, .. } if text.contains("held news")
        )));
        assert!(pump_until(ctx, 10, || idle(&session)));

        // Nothing waits now: the next message goes alone.
        session.send_prompt("second");
        assert!(pump_until(ctx, 10, || prompts().contains("second")));
        assert_eq!(news.taken.get(), 2);
        assert!(pump_until(ctx, 10, || idle(&session)));

        // Leaving the mode releases the news to an ordinary wake.
        session.confirm_mode(Mode::Ask);
        assert_eq!(news.released.get(), 1);
        assert_eq!(session.wake_with("news after"), WakeOutcome::Started);
        assert!(pump_until(ctx, 10, || prompts().contains("news after")));
        session.shutdown(false);
    });
}
