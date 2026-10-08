//! Recovery through a real, deterministic app-server process and a private SQLite store.
//! The fixture never invokes a provider, tool, shell command or network service.

use super::*;
use crate::chat::view::model::{Activity, Body, PendingInterruption, Transcript};
use crate::testutil::{in_loop, pump_until};
use agent_core::claude::ClaudeAdapter;
use agent_core::codex::CodexAdapter;
use serde_json::{json, Value};
use std::path::Path;

struct Fixture {
    dir: tempfile::TempDir,
    store: Rc<Store>,
    thread: String,
    model: Rc<RefCell<Transcript>>,
    seen: Rc<RefCell<Vec<Envelope>>>,
    completion_target: Cell<usize>,
}

impl Fixture {
    fn new() -> Self {
        let dir = tempfile::tempdir().expect("fixture directory");
        let store = Rc::new(Store::open(&dir.path().join("threads.sqlite")).expect("store"));
        let thread = store
            .create_thread(&dir.path().to_string_lossy(), Some("synthetic recovery"))
            .expect("thread");
        Self {
            dir,
            store,
            thread,
            model: Rc::default(),
            seen: Rc::default(),
            completion_target: Cell::new(0),
        }
    }

    fn open(&self) -> OpenSession {
        OpenSession {
            program: "/usr/bin/python3".into(),
            extra_args: vec![
                "-I".into(),
                format!(
                    "{}/tests/support/scripted_codex.py",
                    env!("CARGO_MANIFEST_DIR")
                ),
                self.dir.path().to_string_lossy().into_owned(),
            ],
            cwd: self.dir.path().to_string_lossy().into_owned(),
            model: Some("synthetic-model".into()),
            effort: None,
            mode: Mode::Ask,
            resume: self
                .store
                .provider_threads(&self.thread)
                .expect("providers")
                .last()
                .and_then(|provider| provider.native_id.clone()),
            new_session_id: None,
            approval_hook: false,
        }
    }

    fn session(&self) -> Rc<ChatSession> {
        let model = self.model.clone();
        let seen = self.seen.clone();
        ChatSession::with_env(
            Box::new(CodexAdapter::new()),
            self.open(),
            self.store.clone(),
            self.thread.clone(),
            Rc::new(move |env| {
                model.borrow_mut().apply(env, Driver::Codex);
                seen.borrow_mut().push(env.clone());
            }),
            Err(None),
            minimal_env(self.dir.path()),
        )
    }

    fn frames(&self) -> Vec<Value> {
        std::fs::read_to_string(self.dir.path().join("frames.ndjson"))
            .unwrap_or_default()
            .lines()
            .filter_map(|line| serde_json::from_str(line).ok())
            .collect()
    }

    fn incoming(&self) -> Vec<Value> {
        self.frames()
            .into_iter()
            .filter(|line| line["dir"] == "in")
            .map(|line| line["frame"].clone())
            .collect()
    }

    fn completions(&self) -> usize {
        self.seen
            .borrow()
            .iter()
            .filter(|e| matches!(e.event, Event::TurnCompleted { .. }))
            .count()
    }

    fn prompt(&self, session: &ChatSession, text: &str) {
        self.completion_target.set(self.completions() + 1);
        session.send_prompt(text);
    }

    fn pending(&self) -> Option<String> {
        self.model.borrow().pending_interruption().map(|p| match p {
            PendingInterruption::Approval { request, .. }
            | PendingInterruption::Question { request, .. } => request,
        })
    }

    fn wait(&self, ctx: &glib::MainContext, description: &str, condition: impl FnMut() -> bool) {
        assert!(
            pump_until(ctx, 5, condition),
            "{description}; synthetic frames: {:?}; events: {:?}",
            self.frames(),
            self.seen.borrow()
        );
    }

    fn wait_pending(&self, ctx: &glib::MainContext) -> String {
        self.wait(ctx, "awaiting request", || self.pending().is_some());
        self.pending().expect("request")
    }

    fn wait_finished(&self, ctx: &glib::MainContext) {
        self.wait(ctx, "turn completion", || {
            self.completions() >= self.completion_target.get()
                && self.model.borrow().activity(false) == Activity::Finished
        });
    }

    fn approval_rows(&self) -> usize {
        let model = self.model.borrow();
        model
            .order()
            .iter()
            .filter(|id| {
                matches!(
                    model.get(id).map(|item| &item.body),
                    Some(Body::Approval(_))
                )
            })
            .count()
    }
}

fn minimal_env(dir: &Path) -> LaunchEnv {
    LaunchEnv {
        // Enumerate names only; never read or copy any inherited credentials into the child.
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

#[test]
fn scripted_reconnect_reused_typed_ids_are_answerable_and_history_is_inert() {
    in_loop(|ctx| {
        let fixture = Fixture::new();
        let mut session = fixture.session();
        let mut requests = Vec::new();
        // Resolve 0..3, reconnect twice, then reopen the stored thread and use string 0..3.
        for round in 0..4 {
            if round == 3 {
                session.shutdown(false);
                let mut replay = Transcript::new();
                for (_, event) in fixture
                    .store
                    .events(&fixture.thread, None, 10_000)
                    .expect("history")
                {
                    replay.apply(&event, Driver::Codex);
                }
                replay.settle_stale();
                assert!(
                    replay.pending_interruption().is_none(),
                    "history cannot answer"
                );
                *fixture.model.borrow_mut() = replay;
                session = fixture.session();
            } else if round > 0 {
                session.reload_session();
            }
            fixture.prompt(&session, if round == 3 { "strings" } else { "approvals" });
            for wire_id in 0..4 {
                let request = fixture.wait_pending(ctx);
                assert!(
                    !requests.contains(&request),
                    "fresh native turn/item identity"
                );
                assert_eq!(
                    fixture.model.borrow().activity(true),
                    Activity::NeedsApproval
                );
                assert_eq!(
                    fixture.approval_rows(),
                    requests.len() + 1,
                    "duplicates add no card"
                );
                assert!(session.status().running_turn);
                // Old answers must never satisfy the new request, even on a reused RPC ID.
                if let Some(old) = requests.first() {
                    let replies_before = fixture
                        .incoming()
                        .iter()
                        .filter(|f| f.get("result").is_some())
                        .count();
                    session.respond_approval(old, Decision::Allow);
                    assert_eq!(fixture.pending().as_deref(), Some(request.as_str()));
                    assert_eq!(
                        fixture
                            .incoming()
                            .iter()
                            .filter(|f| f.get("result").is_some())
                            .count(),
                        replies_before
                    );
                }
                session.respond_approval(&request, Decision::Allow);
                let expected = if round == 3 {
                    json!(wire_id.to_string())
                } else {
                    json!(wire_id)
                };
                fixture.wait(ctx, "typed response reaches process", || {
                    fixture.incoming().iter().any(|f| {
                        f["id"] == expected
                            && f["result"] == json!({"decision": "accept"})
                            && fixture.frames().iter().any(|line| {
                                line["connection"] == round + 1
                                    && line["dir"] == "in"
                                    && line["frame"] == *f
                            })
                    })
                });
                requests.push(request);
            }
            fixture.wait_finished(ctx);
            assert_eq!(
                fixture.approval_rows(),
                requests.len(),
                "settled duplicate adds no card"
            );
            assert!(!session.status().running_turn);
        }
        assert_eq!(
            fixture
                .incoming()
                .iter()
                .filter(|f| f["method"] == "thread/resume")
                .count(),
            3
        );
        assert_eq!(
            fixture
                .store
                .provider_threads(&fixture.thread)
                .expect("providers")
                .len(),
            1
        );
        session.shutdown(false);
    });
}

#[test]
fn scripted_pending_cancel_question_and_retry_do_not_accept_stale_answers() {
    in_loop(|ctx| {
        let fixture = Fixture::new();
        let session = fixture.session();
        fixture.prompt(&session, "pending");
        let old = fixture.wait_pending(ctx);
        session.interrupt();
        fixture.wait(ctx, "interrupt expires approval", || {
            fixture.pending().is_none() && !session.status().running_turn
        });
        assert!(fixture
            .seen
            .borrow()
            .iter()
            .any(|e| matches!(e.event, Event::ApprovalExpired)));
        fixture.prompt(&session, "question");
        let question = fixture.wait_pending(ctx);
        assert_eq!(fixture.model.borrow().activity(true), Activity::NeedsAnswer);
        session.respond_approval(&old, Decision::Allow);
        assert_eq!(fixture.pending().as_deref(), Some(question.as_str()));
        // The view marks a submitted question before calling its backend (ChatView::act).
        fixture.model.borrow_mut().mark_questions_sent(&question);
        session.answer_questions(&question, json!({"Continue?": "Yes"}));
        fixture.wait_finished(ctx);
        assert!(fixture
            .incoming()
            .iter()
            .any(|f| f["id"] == 0
                && f["result"] == json!({"answers": {"choice": {"answers": ["Yes"]}}})));
        fixture.prompt(&session, "pending");
        let current = fixture.wait_pending(ctx);
        assert_ne!(old, current);
        session.respond_approval(&current, Decision::Deny);
        fixture.wait_finished(ctx);
        assert!(fixture
            .incoming()
            .iter()
            .any(|f| f["result"] == json!({"decision": "decline"})));
        assert_eq!(
            fixture
                .store
                .transcript_messages(&fixture.thread)
                .expect("messages")
                .iter()
                .filter(|message| message.role == "user")
                .count(),
            3
        );
        session.shutdown(false);
    });
}

#[test]
fn scripted_execution_errors_and_disconnects_preserve_history_and_can_retry() {
    in_loop(|ctx| {
        for scenario in [
            "denied",
            "ssh-error",
            "disconnect-command",
            "disconnect-approval",
        ] {
            let fixture = Fixture::new();
            let session = fixture.session();
            fixture.prompt(&session, scenario);
            fixture.wait(ctx, scenario, || {
                fixture.model.borrow().activity(false) == Activity::Failed
            });
            assert!(!session.status().running_turn);
            assert!(fixture.pending().is_none(), "dead approval must expire");
            let model = fixture.model.borrow();
            assert!(
                model.order().iter().any(|id| matches!(
                    model.get(id).map(|i| &i.body),
                    Some(Body::TurnError { .. })
                )),
                "visible error for {scenario}"
            );
            drop(model);
            if scenario.starts_with("disconnect") {
                fixture.wait(ctx, "process exit", || !session.status().alive);
            }
            fixture.prompt(&session, "success");
            fixture.wait_finished(ctx);
            assert!(session.status().alive);
            assert_eq!(
                fixture
                    .store
                    .transcript_messages(&fixture.thread)
                    .expect("messages")
                    .iter()
                    .filter(|message| message.role == "user")
                    .count(),
                2
            );
            session.shutdown(false);
        }
    });
}

#[test]
fn scripted_reload_rejects_previous_process_frames_and_adopts_deferred_mode_next_turn() {
    in_loop(|ctx| {
        let fixture = Fixture::new();
        let session = fixture.session();
        fixture.prompt(&session, "pending");
        let old = fixture.wait_pending(ctx);
        session.set_mode(Mode::AcceptEdits);
        assert_eq!(
            session.status().mode,
            Mode::Ask,
            "active policy remains effective"
        );
        assert!(fixture.seen.borrow().iter().any(|e| matches!(
            e.event,
            Event::ModeChangeDeferred {
                requested: Mode::AcceptEdits,
                effective: Mode::Ask
            }
        )));
        session.respond_approval(&old, Decision::Deny);
        fixture.wait_finished(ctx);
        let old_generation = session.inner.generation.get();
        session.reload_session();
        let before = fixture.seen.borrow().len();
        session.inner.on_line(old_generation, r#"{"method":"turn/started","params":{"threadId":"fixture-thread","turn":{"id":"stale"}}}"#, false);
        session
            .inner
            .on_line(old_generation, "late stderr must not enter history", true);
        session.inner.on_exit(old_generation, Some(99));
        assert_eq!(fixture.seen.borrow().len(), before);
        assert!(session.status().alive);
        fixture.prompt(&session, "success");
        fixture.wait_finished(ctx);
        let starts: Vec<Value> = fixture
            .incoming()
            .into_iter()
            .filter(|f| f["method"] == "turn/start")
            .collect();
        assert_eq!(starts[0]["params"]["approvalPolicy"], "untrusted");
        assert_eq!(starts[0]["params"]["sandboxPolicy"]["type"], "readOnly");
        assert_eq!(starts[1]["params"]["approvalPolicy"], "on-request");
        assert_eq!(
            starts[1]["params"]["sandboxPolicy"]["type"],
            "workspaceWrite"
        );
        assert_eq!(session.status().mode, Mode::AcceptEdits);
        session.shutdown(false);
    });
}

#[test]
fn scripted_spawn_failure_reports_an_error_and_retry_keeps_the_user_history() {
    in_loop(|ctx| {
        let fixture = Fixture::new();
        let mut open = fixture.open();
        open.program = fixture
            .dir
            .path()
            .join("missing-provider")
            .to_string_lossy()
            .into_owned();
        let seen = fixture.seen.clone();
        let model = fixture.model.clone();
        let session = ChatSession::with_env(
            Box::new(CodexAdapter::new()),
            open,
            fixture.store.clone(),
            fixture.thread.clone(),
            Rc::new(move |event| {
                model.borrow_mut().apply(event, Driver::Codex);
                seen.borrow_mut().push(event.clone());
            }),
            Err(None),
            minimal_env(fixture.dir.path()),
        );
        assert!(!session.status().alive);
        assert!(fixture
            .seen
            .borrow()
            .iter()
            .any(|e| matches!(e.event, Event::Error { .. })));
        fixture.prompt(&session, "success");
        assert!(!session.status().alive);
        // Repairing the configured executable is the same input the profile editor changes.
        *session.inner.open.borrow_mut() = fixture.open();
        session.reload_session();
        fixture.wait_finished(ctx);
        assert!(session.status().alive);
        assert_eq!(
            fixture
                .store
                .transcript_messages(&fixture.thread)
                .expect("history")[0]
                .text,
            "success"
        );
        session.shutdown(false);
    });
}

#[test]
fn scripted_codex_claude_codex_switch_routes_replies_and_recovers_a_failed_switch() {
    in_loop(|ctx| {
        let fixture = Fixture::new();
        let session = fixture.session();
        fixture.prompt(&session, "pending");
        let old = fixture.wait_pending(ctx);
        let old_generation = session.inner.generation.get();
        let fail_claude = Rc::new(Cell::new(true));
        let fail = fail_claude.clone();
        let open = fixture.open();
        let env = minimal_env(fixture.dir.path());
        session.set_adapter_factory(Rc::new(move |driver| {
            let mut args = open.extra_args.clone();
            if driver == Driver::Claude {
                args.push("--claude".into());
            }
            Ok(AgentLaunch {
                adapter: if driver == Driver::Claude {
                    Box::new(ClaudeAdapter::new())
                } else {
                    Box::new(CodexAdapter::new())
                },
                program: if driver == Driver::Claude && fail.get() {
                    "/no/synthetic-provider".into()
                } else {
                    open.program.clone()
                },
                extra_args: args,
                default_model: Some(
                    if driver == Driver::Claude {
                        "synthetic-claude"
                    } else {
                        "synthetic-model"
                    }
                    .into(),
                ),
                default_effort: None,
                env: env.clone(),
                approval: Err(None),
            })
        }));
        session.switch(Driver::Claude, None, None);
        fixture.wait(ctx, "failed switch rolls back", || {
            session.status().driver == Driver::Codex
                && session.status().alive
                && fixture
                    .frames()
                    .iter()
                    .any(|f| f["connection"] == 2 && f["frame"]["method"] == "thread/resume")
        });
        assert!(fixture
            .seen
            .borrow()
            .iter()
            .any(|e| matches!(&e.event, Event::Error { message } if message.contains("Back on"))));
        fail_claude.set(false);
        session.switch(Driver::Claude, None, None);
        fixture.wait(ctx, "alternate provider starts", || fixture.seen.borrow().iter().any(|e| matches!(&e.event, Event::SessionStarted { native_id, .. } if native_id == "fixture-claude")));
        assert_eq!(session.status().driver, Driver::Claude);
        fixture.prompt(&session, "continue in Claude");
        fixture.wait_finished(ctx);
        assert!(fixture.frames().iter().any(|f| f["connection"] == 3
            && f["dir"] == "in"
            && f["frame"]["type"] == "user"
            && f["frame"]["message"]["content"]
                .as_str()
                .is_some_and(|s| s.contains("continue in Claude"))));
        session.switch(Driver::Codex, None, None);
        fixture.prompt(&session, "pending");
        let current = fixture.wait_pending(ctx);
        let before = fixture.seen.borrow().len();
        session.inner.on_line(old_generation, r#"{"method":"turn/completed","params":{"threadId":"fixture-thread","turn":{"id":"old","status":"failed"}}}"#, false);
        session.inner.on_exit(old_generation, Some(17));
        assert_eq!(fixture.seen.borrow().len(), before);
        session.respond_approval(&old, Decision::Allow);
        assert_eq!(fixture.pending().as_deref(), Some(current.as_str()));
        session.respond_approval(&current, Decision::Allow);
        fixture.wait_finished(ctx);
        assert_eq!(session.status().driver, Driver::Codex);
        assert_eq!(session.status().mode, Mode::Ask);
        assert!(fixture.frames().iter().any(|f| f["connection"] == 4
            && f["dir"] == "in"
            && f["frame"]["result"] == json!({"decision": "accept"})));
        session.shutdown(false);
    });
}
