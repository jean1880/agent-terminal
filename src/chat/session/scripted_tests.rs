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
        self.session_observing(Rc::new(|_| {}))
    }

    fn session_observing(&self, observe: EnvelopeSink) -> Rc<ChatSession> {
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
                observe(env);
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
            | PendingInterruption::Question { request, .. }
            | PendingInterruption::Form { request, .. } => request,
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
        assert_eq!(starts[1]["params"]["approvalPolicy"], "untrusted");
        assert_eq!(
            starts[1]["params"]["sandboxPolicy"]["type"],
            "workspaceWrite"
        );
        assert_eq!(session.status().mode, Mode::AcceptEdits);
        session.shutdown(false);
    });
}

#[test]
fn scripted_busy_model_switch_resumes_with_the_selected_model_and_continues_once() {
    in_loop(|ctx| {
        let fixture = Fixture::new();
        let session = fixture.session();
        fixture.prompt(&session, "pending");
        let old_request = fixture.wait_pending(ctx);
        let old_generation = session.inner.generation.get();
        // A resume response may describe the previous thread's default. The next turn must
        // still explicitly use the new choice rather than restoring that old default.
        std::fs::write(fixture.dir.path().join("startup-model"), "synthetic-model")
            .expect("reported startup model");
        session.switch(
            Driver::Codex,
            Some("synthetic-new-model".into()),
            Some("high".into()),
        );
        fixture.wait(ctx, "continued replacement turn", || {
            fixture.model.borrow().activity(false) == Activity::Finished
                && session.status().model.as_deref() == Some("synthetic-new-model")
        });
        assert!(session.inner.generation.get() > old_generation);
        assert!(session.status().pending_model.is_none());
        assert_eq!(session.status().effort.as_deref(), Some("high"));
        assert!(fixture.pending().is_none(), "old approval expired");
        let before = fixture.seen.borrow().len();
        session.inner.on_line(
            old_generation,
            r#"{"method":"turn/started","params":{"turn":{"model":"synthetic-model"}}}"#,
            false,
        );
        assert_eq!(
            fixture.seen.borrow().len(),
            before,
            "old process cannot replace the selected model"
        );
        session.respond_approval(&old_request, Decision::Allow);
        let incoming = fixture.incoming();
        let resumes: Vec<_> = incoming
            .iter()
            .filter(|f| f["method"] == "thread/resume")
            .collect();
        assert_eq!(resumes.len(), 1);
        assert_eq!(resumes[0]["params"]["threadId"], "fixture-thread");
        assert_eq!(resumes[0]["params"]["model"], "synthetic-new-model");
        let turns: Vec<_> = incoming
            .iter()
            .filter(|f| f["method"] == "turn/start")
            .collect();
        assert_eq!(
            turns.len(),
            2,
            "original turn plus exactly one continuation"
        );
        assert_eq!(turns[1]["params"]["model"], "synthetic-new-model");
        assert_eq!(turns[1]["params"]["effort"], "high");
        let continued = turns[1]["params"]["input"][0]["text"]
            .as_str()
            .expect("continuation");
        assert!(continued.contains("Continue the unfinished task"));
        assert!(
            continued.contains("pending"),
            "unfinished original request is carried"
        );
        assert!(fixture.seen.borrow().iter().any(|e| matches!(
            e.event,
            Event::TurnCompleted {
                state: TurnState::Interrupted,
                ..
            }
        )));
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

/// Plan 2.5: a stored Codex thread id whose thread never completed a turn has no rollout. The
/// reopened thread still works: the next prompt runs in a new Codex thread, carrying the
/// thread's history as a handoff, with a notice, and only that completed turn stores the new id.
#[test]
fn scripted_resume_with_no_rollout_continues_in_a_new_codex_thread() {
    in_loop(|ctx| {
        let fixture = Fixture::new();
        let pt = fixture
            .store
            .add_provider_thread(&fixture.thread, "codex", "synthetic-model")
            .expect("provider thread");
        fixture
            .store
            .set_active_provider_thread(&fixture.thread, &pt)
            .expect("active");
        fixture
            .store
            .set_native_id(&pt, "stale-thread")
            .expect("stale id");
        fixture
            .store
            .append_user_message(&fixture.thread, Some(&pt), "remember the blue door")
            .expect("history");
        let native = || {
            fixture
                .store
                .provider_threads(&fixture.thread)
                .expect("providers")
                .into_iter()
                .find(|p| p.id == pt)
                .and_then(|p| p.native_id)
        };

        let session = fixture.session();
        fixture.wait(ctx, "the new Codex thread", || {
            session.inner.state.borrow().native_id.as_deref() == Some("fixture-thread")
        });
        assert!(fixture.seen.borrow().iter().any(|e| matches!(&e.event,
            Event::Notice { text } if text.contains("new Codex session"))));
        assert!(!fixture
            .seen
            .borrow()
            .iter()
            .any(|e| matches!(e.event, Event::Error { .. })));
        assert_eq!(
            native().as_deref(),
            Some("stale-thread"),
            "provisional until a turn completes"
        );

        fixture.prompt(&session, "success");
        fixture.wait_finished(ctx);
        let incoming = fixture.incoming();
        let methods: Vec<&str> = incoming
            .iter()
            .filter_map(|f| f["method"].as_str())
            .filter(|m| m.starts_with("thread/") || *m == "turn/start")
            .collect();
        assert_eq!(methods, ["thread/resume", "thread/start", "turn/start"]);
        let turn = incoming
            .iter()
            .find(|f| f["method"] == "turn/start")
            .expect("turn");
        assert_eq!(turn["params"]["threadId"], "fixture-thread");
        let text = turn["params"]["input"][0]["text"]
            .as_str()
            .unwrap_or_default();
        assert!(text.starts_with("Context handoff (HANDOFF-"), "{text}");
        assert!(text.contains("remember the blue door"), "{text}");
        assert!(text.ends_with("\nsuccess"), "{text}");
        assert_eq!(native().as_deref(), Some("fixture-thread"));
        session.shutdown(false);
    });
}

/// Plan 2.7 (F19): an answered question card leaves "Sending…" for "Answered" once the adapter
/// has accepted the answer; the agent never acknowledges it. A refused answer stays sent.
#[test]
fn scripted_an_accepted_answer_moves_the_question_card_to_answered() {
    use crate::chat::view::model::QuestionState;
    in_loop(|ctx| {
        let fixture = Fixture::new();
        let session = fixture.session();
        let state = |request: &str| {
            let model = fixture.model.borrow();
            model
                .order()
                .iter()
                .find_map(|id| match model.get(id).map(|i| &i.body) {
                    Some(Body::Question(q)) if q.request == request => Some(q.state),
                    _ => None,
                })
        };
        fixture.prompt(&session, "question");
        let question = fixture.wait_pending(ctx);
        fixture.model.borrow_mut().mark_questions_sent(&question);
        assert_eq!(state(&question), Some(QuestionState::Sent));
        // An answer the adapter refuses (no such question) resolves nothing.
        session.answer_questions("no-such-question", json!({"Continue?": "Yes"}));
        assert_eq!(state(&question), Some(QuestionState::Sent));
        session.answer_questions(&question, json!({"Continue?": "Yes"}));
        assert_eq!(state(&question), Some(QuestionState::Answered));
        assert!(fixture
            .seen
            .borrow()
            .iter()
            .any(|e| e.request.as_deref() == Some(question.as_str())
                && e.event == Event::QuestionResolved { answered: true }));
        fixture.wait_finished(ctx);
        assert_eq!(state(&question), Some(QuestionState::Answered));
        session.shutdown(false);
    });
}

/// A message written while a Codex turn runs goes into that turn (`turn/steer`, pinned to it)
/// and shows as an ordinary message, not a queued one; no second turn starts. A slash command
/// mid-turn still waits in the queue.
#[test]
fn scripted_a_message_mid_turn_steers_the_running_codex_turn() {
    in_loop(|ctx| {
        let fixture = Fixture::new();
        let session = fixture.session();
        fixture.prompt(&session, "approval");
        let request = fixture.wait_pending(ctx);
        let user_rows = |fixture: &Fixture| -> Vec<(String, bool)> {
            let model = fixture.model.borrow();
            model
                .order()
                .iter()
                .filter_map(|id| match model.get(id).map(|i| &i.body) {
                    Some(Body::User { text, queued }) => Some((text.clone(), *queued)),
                    _ => None,
                })
                .collect()
        };

        session.steer_prompt("also check the tests");
        fixture.wait(ctx, "the steer reaches the running turn", || {
            fixture.dir.path().join("steered").exists()
        });
        let steers: Vec<Value> = fixture
            .incoming()
            .into_iter()
            .filter(|f| f["method"] == "turn/steer")
            .collect();
        assert_eq!(steers.len(), 1, "{steers:?}");
        assert_eq!(
            steers[0]["params"]["input"][0]["text"],
            "also check the tests"
        );
        let starts = fixture
            .incoming()
            .iter()
            .filter(|f| f["method"] == "turn/start")
            .count();
        assert_eq!(starts, 1, "no second turn");
        assert_eq!(
            user_rows(&fixture).last(),
            Some(&("also check the tests".to_owned(), false)),
            "shown as sent, not queued"
        );

        // A slash command mid-turn keeps waiting for the turn to end.
        session.steer_prompt("/review");
        assert_eq!(
            user_rows(&fixture).last(),
            Some(&("/review".to_owned(), true)),
            "queued"
        );
        assert_eq!(
            fixture
                .incoming()
                .iter()
                .filter(|f| f["method"] == "turn/steer")
                .count(),
            1,
            "a slash command is never steered"
        );

        session.respond_approval(&request, Decision::Allow);
        fixture.wait_finished(ctx);
        session.shutdown(false);
    });
}

/// Plan 2.5 (review): a turn that produced an item but never completed (the process died
/// mid-command) still confirms the new Codex thread, so the next reopen resumes it.
#[test]
fn scripted_a_turn_with_an_item_confirms_the_new_codex_thread() {
    in_loop(|ctx| {
        let fixture = Fixture::new();
        let pt = fixture
            .store
            .add_provider_thread(&fixture.thread, "codex", "synthetic-model")
            .expect("provider thread");
        fixture
            .store
            .set_active_provider_thread(&fixture.thread, &pt)
            .expect("active");
        fixture
            .store
            .set_native_id(&pt, "stale-thread")
            .expect("stale id");
        let native = || {
            fixture
                .store
                .provider_threads(&fixture.thread)
                .expect("providers")
                .into_iter()
                .find(|p| p.id == pt)
                .and_then(|p| p.native_id)
        };
        let session = fixture.session();
        fixture.wait(ctx, "the new Codex thread", || {
            session.inner.state.borrow().native_id.as_deref() == Some("fixture-thread")
        });
        assert_eq!(native().as_deref(), Some("stale-thread"));
        session.send_prompt("disconnect-command");
        fixture.wait(ctx, "the process dies mid-command", || {
            !session.status().alive
        });
        assert!(!fixture.seen.borrow().iter().any(|e| matches!(
            e.event,
            Event::TurnCompleted {
                state: TurnState::Completed,
                ..
            }
        )));
        assert_eq!(native().as_deref(), Some("fixture-thread"));
        session.shutdown(false);
    });
}

// ---- Codex permission requests and forms (plan 2026-10-11, lane L1) ----

/// The latest Codex permission card the view received: request, options and input.
fn permission_card(seen: &RefCell<Vec<Envelope>>) -> Option<(String, Vec<Decision>, Value)> {
    seen.borrow().iter().rev().find_map(|e| match &e.event {
        Event::ApprovalRequested {
            tool,
            options,
            input,
            ..
        } if tool == agent_core::codex::PERMISSIONS_TOOL => {
            Some((e.request.clone()?, options.clone(), input.clone()))
        }
        _ => None,
    })
}

fn notices(seen: &RefCell<Vec<Envelope>>, needle: &str) -> usize {
    seen.borrow()
        .iter()
        .filter(|e| matches!(&e.event, Event::Notice { text } if text.contains(needle)))
        .count()
}

impl Fixture {
    fn results(&self) -> Vec<Value> {
        self.incoming()
            .into_iter()
            .filter_map(|f| f.get("result").cloned())
            .collect()
    }

    fn wait_card(&self, ctx: &glib::MainContext) -> (String, Vec<Decision>, Value) {
        self.wait(ctx, "permission card", || {
            permission_card(&self.seen).is_some()
        });
        permission_card(&self.seen).expect("card")
    }

    fn no_rules(&self, session: &ChatSession) {
        assert!(session.inner.session_rules.borrow().rules.is_empty());
        assert!(session.inner.pending_rules.borrow().is_empty());
    }
}

#[test]
fn scripted_a_sensitive_permission_request_never_offers_or_sends_a_session_grant() {
    in_loop(|ctx| {
        for (permissions, says) in [
            (
                json!({"fileSystem": {"entries": [{"access": "write",
                    "path": {"type": "special", "value": {"kind": "root"}}}]}}),
                "the whole filesystem",
            ),
            (json!({"fileSystem": {"write": [".git/hooks"]}}), ".git"),
        ] {
            let fixture = Fixture::new();
            let session = fixture.session();
            fixture.prompt(&session, &format!("permissions:{permissions}"));
            let (request, options, input) = fixture.wait_card(ctx);
            assert_eq!(options, [Decision::Allow, Decision::Deny, Decision::Cancel]);
            assert_eq!(input["session_allowed"], false);
            assert!(
                input["warning"].as_str().is_some_and(|w| w.contains(says)),
                "{input}"
            );
            // Refused at answer time too, whatever the card offered: nothing is written.
            for refused in [Decision::AllowForSession, Decision::AllowAlways] {
                session.respond_approval(&request, refused);
                assert!(fixture.results().is_empty(), "{refused:?} sent nothing");
            }
            assert_eq!(
                notices(&fixture.seen, "Allow for session is not available"),
                1
            );
            assert_eq!(notices(&fixture.seen, "cannot be always allowed"), 1);
            session.respond_approval(&request, Decision::Allow);
            fixture.wait_finished(ctx);
            let results = fixture.results();
            assert_eq!(results.len(), 1);
            assert_eq!(results[0]["scope"], "turn");
            assert!(results[0]["permissions"]["fileSystem"].is_object());
            assert!(fixture.seen.borrow().iter().any(|e| e.request.as_deref()
                == Some(request.as_str())
                && e.event
                    == Event::ApprovalResolved {
                        decision: Decision::Allow
                    }));
            assert_eq!(notices(&fixture.seen, "extra access"), 0);
            fixture.no_rules(&session);
            session.shutdown(false);
        }
    });
}

#[test]
fn dangerous_permissive_mode_grants_codex_permissions_as_shown() {
    in_loop(|ctx| {
        let fixture = Fixture::new();
        let session = fixture.session();
        let outside = tempfile::tempdir().expect("resolved target");
        std::os::unix::fs::symlink(outside.path(), fixture.dir.path().join("build")).expect("link");
        session.set_mode(Mode::Bypass);
        fixture.prompt(
            &session,
            r#"permissions:{"fileSystem":{"write":["build"]},"network":{"enabled":true}}"#,
        );
        fixture.wait_finished(ctx);
        let (request, options, input) = fixture.wait_card(ctx);
        let real = std::fs::canonicalize(outside.path()).expect("real");
        assert!(options.contains(&Decision::Allow));
        assert_eq!(input["entries"][0]["target"]["resolved"], json!(real));
        assert_eq!(
            fixture.results(),
            [json!({"scope": "turn", "permissions": {
                "fileSystem": {"write": [real]}, "network": {"enabled": true}
            }})]
        );
        assert!(fixture
            .seen
            .borrow()
            .iter()
            .any(|e| e.request.as_deref() == Some(&request)
                && e.event
                    == Event::ApprovalResolved {
                        decision: Decision::Allow
                    }));
        session.shutdown(false);
    });
}

#[test]
fn dangerous_permissive_mode_never_persists_a_permission_grant() {
    in_loop(|ctx| {
        let fixture = Fixture::new();
        let session = fixture.session();
        *session.inner.saved_rules.borrow_mut() = Some(crate::always_allow::StoreCache::new(
            fixture.dir.path().join(crate::always_allow::FILE_NAME),
        ));
        session.set_mode(Mode::Bypass);
        fixture.prompt(
            &session,
            r#"permissions:{"fileSystem":{"write":["build"]}}"#,
        );
        fixture.wait_finished(ctx);
        assert_eq!(fixture.results()[0]["scope"], "turn");
        fixture.no_rules(&session);
        assert!(session.inner.codex_grants.borrow().is_empty());
        assert!(session.inner.codex_grant_paths.borrow().is_empty());
        assert!(
            session.inner.codex_turn_grant_paths.borrow().is_empty(),
            "turn ended"
        );
        assert_eq!(notices(&fixture.seen, "extra access"), 0);
        assert!(!fixture
            .dir
            .path()
            .join(crate::always_allow::FILE_NAME)
            .exists());
        let events = session
            .inner
            .store
            .reader()
            .events(&fixture.thread, None, 10_000)
            .expect("events");
        assert!(!events.iter().any(|(_, e)| matches!(
            e.event,
            Event::ApprovalResolved {
                decision: Decision::AllowAlways | Decision::AllowForSession
            }
        )));
        session.shutdown(false);
    });
}

#[test]
fn dangerous_permissive_mode_permission_recheck_honours_a_later_mode() {
    in_loop(|ctx| {
        let fixture = Fixture::new();
        let session = fixture.session();
        session.set_mode(Mode::Bypass);
        fixture.prompt(&session, r#"permissions:{"network":{"enabled":true}}"#);
        let (request, _, _) = fixture.wait_card(ctx);
        assert!(session.inner.codex_permissions.borrow()[&request].checking);
        // A real ModeChanged settles the effective policy; Codex's SetMode alone defers it.
        session
            .inner
            .emit(Envelope::new(Event::ModeChanged { mode: Mode::Plan }));
        settle(ctx);
        assert!(
            fixture.results().is_empty(),
            "Bypass ended before the recheck finished"
        );
        assert!(!session.inner.codex_permissions.borrow()[&request].checking);
        session.respond_approval(&request, Decision::Deny);
        fixture.wait_finished(ctx);
        session.shutdown(false);
    });
}

#[test]
fn dangerous_permissive_mode_permission_request_asks_after_bypass_is_left_mid_turn() {
    in_loop(|ctx| {
        let fixture = Fixture::new();
        let slot: Rc<RefCell<std::rc::Weak<ChatSession>>> = Rc::default();
        let callback = slot.clone();
        let session = fixture.session_observing(Rc::new(move |env| {
            if matches!(env.event, Event::TurnStarted { .. }) {
                callback
                    .borrow()
                    .upgrade()
                    .expect("session")
                    .set_mode(Mode::Ask);
            }
        }));
        *slot.borrow_mut() = Rc::downgrade(&session);
        session.set_mode(Mode::Bypass);
        fixture.prompt(&session, r#"permissions:{"network":{"enabled":true}}"#);
        let (request, options, _) = fixture.wait_card(ctx);
        assert_eq!(
            session.status().mode,
            Mode::Bypass,
            "Codex retains this turn's mode"
        );
        assert_eq!(session.inner.wanted_mode.get(), Mode::Ask);
        assert_eq!(fixture.model.borrow().pending_mode, Some(Mode::Ask));
        settle(ctx);
        assert!(
            fixture.results().is_empty(),
            "the new request needs an answer"
        );
        assert!(!session.inner.codex_permissions.borrow()[&request].checking);
        assert!(options.contains(&Decision::Allow));
        session.respond_approval(&request, Decision::Allow);
        fixture.wait_finished(ctx);
        assert_eq!(
            fixture.results(),
            [json!({"scope": "turn", "permissions": {
                "network": {"enabled": true}
            }})]
        );
        fixture.no_rules(&session);
        session.shutdown(false);
    });
}

#[test]
fn dangerous_permissive_mode_permission_recheck_stops_when_ask_is_chosen_mid_turn() {
    in_loop(|ctx| {
        let fixture = Fixture::new();
        let session = fixture.session();
        session.set_mode(Mode::Bypass);
        fixture.prompt(&session, r#"permissions:{"network":{"enabled":true}}"#);
        let (request, _, _) = fixture.wait_card(ctx);
        assert!(session.inner.codex_permissions.borrow()[&request].checking);
        session.set_mode(Mode::Ask);
        assert_eq!(session.status().mode, Mode::Bypass);
        assert_eq!(session.inner.wanted_mode.get(), Mode::Ask);
        assert_eq!(fixture.model.borrow().pending_mode, Some(Mode::Ask));
        settle(ctx);
        assert!(
            fixture.results().is_empty(),
            "Ask was selected before the recheck finished"
        );
        assert!(!session.inner.codex_permissions.borrow()[&request].checking);
        session.respond_approval(&request, Decision::Deny);
        fixture.wait_finished(ctx);
        assert_eq!(fixture.results(), [json!({"permissions": {}})]);
        session.shutdown(false);
    });
}

#[test]
fn dangerous_permissive_mode_permission_rechecks_stop_after_three_rounds() {
    in_loop(|ctx| {
        let fixture = Fixture::new();
        let outside = tempfile::tempdir().expect("outside");
        let real = std::fs::canonicalize(outside.path()).expect("real");
        let targets = (0..4)
            .map(|n| real.join(format!("target-{n}")))
            .collect::<Vec<_>>();
        for target in &targets {
            std::fs::create_dir(target).expect("target");
        }
        let link = fixture.dir.path().join("build");
        let changes = targets.clone();
        let count = Cell::new(0);
        let session = fixture.session_observing(Rc::new(move |env| {
            if matches!(&env.event, Event::ApprovalRequested { tool, .. }
                if tool == agent_core::codex::PERMISSIONS_TOOL)
            {
                let round = count.get();
                count.set(round + 1);
                if let Some(target) = changes.get(round) {
                    if round > 0 {
                        std::fs::remove_file(&link).expect("unlink");
                    }
                    std::os::unix::fs::symlink(target, &link).expect("change the target");
                }
            }
        }));
        session.set_mode(Mode::Bypass);
        fixture.prompt(
            &session,
            r#"permissions:{"fileSystem":{"write":["build"]}}"#,
        );
        fixture.wait(ctx, "three automatic checks", || {
            permission_cards(&fixture) >= 4
        });
        settle(ctx);
        assert_eq!(permission_cards(&fixture), 4, "automatic checking stopped");
        assert!(fixture.results().is_empty());
        let (request, options, _) = fixture.wait_card(ctx);
        assert!(options.contains(&Decision::Allow));
        assert_eq!(session.inner.codex_permissions.borrow()[&request].round, 3);
        assert!(!session.inner.codex_permissions.borrow()[&request].checking);
        // Re-entering Bypass must not reset the request's automatic-check budget.
        session.set_mode(Mode::Ask);
        session.set_mode(Mode::Bypass);
        session.inner.auto_codex_permissions(&request);
        settle(ctx);
        assert!(fixture.results().is_empty());
        assert_eq!(permission_cards(&fixture), 4);
        session.respond_approval(&request, Decision::Allow);
        fixture.wait(
            ctx,
            "the manual recheck shows the last changed target",
            || permission_cards(&fixture) == 5,
        );
        let (again, _, _) = fixture.wait_card(ctx);
        session.respond_approval(&again, Decision::Allow);
        fixture.wait_finished(ctx);
        assert_eq!(
            fixture.results(),
            [json!({"scope": "turn", "permissions": {
                "fileSystem": {"write": [targets[3]]}
            }})]
        );
        fixture.no_rules(&session);
        session.shutdown(false);
    });
}

#[test]
fn dangerous_permissive_mode_rechecks_a_target_changed_after_the_card() {
    in_loop(|ctx| {
        let fixture = Fixture::new();
        let outside = tempfile::tempdir().expect("outside");
        let real = std::fs::canonicalize(outside.path()).expect("real");
        let target = real.clone();
        let link = fixture.dir.path().join("build");
        let swapped = Cell::new(false);
        let session = fixture.session_observing(Rc::new(move |env| {
            if matches!(&env.event, Event::ApprovalRequested { tool, .. }
                if tool == agent_core::codex::PERMISSIONS_TOOL)
                && !swapped.replace(true)
            {
                std::os::unix::fs::symlink(&target, &link).expect("swap after showing the card");
            }
        }));
        session.set_mode(Mode::Bypass);
        fixture.prompt(
            &session,
            r#"permissions:{"fileSystem":{"write":["build"]}}"#,
        );
        fixture.wait_finished(ctx);
        assert_eq!(
            permission_cards(&fixture),
            2,
            "the changed target was shown again"
        );
        let (_, _, input) = fixture.wait_card(ctx);
        assert_eq!(input["entries"][0]["target"]["resolved"], json!(real));
        assert_eq!(
            fixture.results(),
            [json!({"scope": "turn", "permissions": {
                "fileSystem": {"write": [real]}
            }})]
        );
        fixture.no_rules(&session);
        session.shutdown(false);
    });
}

#[test]
fn dangerous_permissive_mode_checks_sensitive_and_unrenderable_permissions() {
    in_loop(|ctx| {
        for permissions in [
            json!({"fileSystem": {"write": [".git/hooks"]}}),
            json!({"fileSystem": {"entries": [{"access": "write",
                "path": {"type": "special", "value": {"kind": "root"}}}]}}),
        ] {
            let fixture = Fixture::new();
            let session = fixture.session();
            session.set_mode(Mode::Bypass);
            fixture.prompt(&session, &format!("permissions:{permissions}"));
            fixture.wait_finished(ctx);
            let (_, options, input) = fixture.wait_card(ctx);
            assert!(!options.contains(&Decision::AllowForSession));
            assert_eq!(input["session_allowed"], false);
            assert_eq!(fixture.results()[0]["scope"], "turn");
            fixture.no_rules(&session);
            session.shutdown(false);
        }
        for permissions in [
            json!({"unknown": true}),
            json!({"fileSystem": {"write": ["/secret/password=super-sensitive-value/file"]}}),
        ] {
            let fixture = Fixture::new();
            let session = fixture.session();
            session.set_mode(Mode::Bypass);
            fixture.prompt(&session, &format!("permissions:{permissions}"));
            let (request, options, _) = fixture.wait_card(ctx);
            settle(ctx);
            assert!(!options.contains(&Decision::Allow));
            assert!(fixture.results().is_empty());
            session.respond_approval(&request, Decision::Deny);
            fixture.wait_finished(ctx);
            assert_eq!(fixture.results(), [json!({"permissions": {}})]);
            session.shutdown(false);
        }
    });
}

#[test]
fn scripted_a_session_grant_is_announced_and_outlives_a_mode_change_until_restart() {
    in_loop(|ctx| {
        let fixture = Fixture::new();
        let session = fixture.session();
        fixture.prompt(
            &session,
            r#"permissions:{"fileSystem":{"write":["build"]},"network":{"enabled":true}}"#,
        );
        let (request, options, input) = fixture.wait_card(ctx);
        assert_eq!(
            options,
            [
                Decision::Allow,
                Decision::AllowForSession,
                Decision::Deny,
                Decision::Cancel
            ]
        );
        let build = format!("{}/build", fixture.dir.path().display());
        assert_eq!(input["entries"][0]["target"]["path"], build.as_str());
        session.respond_approval(&request, Decision::AllowForSession);
        fixture.wait_finished(ctx);
        assert_eq!(
            fixture.results(),
            [json!({"scope": "session", "permissions": {
                "fileSystem": {"write": [build]}, "network": {"enabled": true}}})]
        );
        assert_eq!(
            notices(
                &fixture.seen,
                &format!("Codex has extra access for this session: write to {build} and use the network. It lasts until this thread's Codex session restarts.")
            ),
            1
        );
        fixture.no_rules(&session);
        session.set_mode(Mode::AcceptEdits);
        assert_eq!(notices(&fixture.seen, "still applies in the new mode"), 1);
        session.set_mode(Mode::AcceptEdits);
        assert_eq!(
            notices(&fixture.seen, "still applies"),
            1,
            "no change, no notice"
        );
        // A new app-server process holds no grant.
        session.reload_session();
        session.set_mode(Mode::Ask);
        assert_eq!(notices(&fixture.seen, "still applies"), 1);
        session.shutdown(false);
    });
}

#[test]
fn scripted_stop_turn_on_a_permission_request_grants_nothing_and_interrupts() {
    in_loop(|ctx| {
        let fixture = Fixture::new();
        let session = fixture.session();
        fixture.prompt(&session, r#"permissions:{"network":{"enabled":true}}"#);
        let (request, ..) = fixture.wait_card(ctx);
        session.respond_approval(&request, Decision::Cancel);
        fixture.wait(ctx, "interrupt", || {
            fixture
                .incoming()
                .iter()
                .any(|f| f["method"] == "turn/interrupt")
        });
        assert_eq!(fixture.results(), [json!({"permissions": {}})]);
        fixture.wait_finished(ctx);
        session.shutdown(false);
    });
}

#[test]
fn scripted_forms_answer_through_the_adapter_and_a_link_with_a_secret_is_declined() {
    use agent_core::form::{Form, FormAction};
    in_loop(|ctx| {
        let form_request = |fixture: &Fixture| {
            fixture.seen.borrow().iter().find_map(|e| match &e.event {
                Event::FormRequested { form, .. } => Some((e.request.clone()?, form.clone())),
                _ => None,
            })
        };
        let resolved = |fixture: &Fixture| {
            fixture
                .seen
                .borrow()
                .iter()
                .filter(|e| matches!(e.event, Event::FormResolved { .. }))
                .count()
        };

        // A link whose masking would change it is never shown: declined with a notice.
        let token = format!("ghp_{}", "a1B2c3D4e5".repeat(4));
        let fixture = Fixture::new();
        let session = fixture.session();
        fixture.prompt(
            &session,
            &format!(
                r#"form:{{"mode":"url","url":"https://example.com/cb?code={token}","elicitationId":"e1"}}"#
            ),
        );
        fixture.wait_finished(ctx);
        assert_eq!(fixture.results(), [json!({"action": "decline"})]);
        assert!(form_request(&fixture).is_none(), "no card");
        assert_eq!(
            notices(&fixture.seen, "Open it yourself only if you trust it"),
            1
        );
        assert!(!fixture
            .seen
            .borrow()
            .iter()
            .any(|e| serde_json::to_string(e)
                .unwrap_or_default()
                .contains(&token)));
        session.shutdown(false);

        // A plain link: Done sends accept, and only the session resolves the card, once.
        let fixture = Fixture::new();
        let session = fixture.session();
        fixture.prompt(
            &session,
            r#"form:{"mode":"url","url":"https://example.com/login","elicitationId":"e2"}"#,
        );
        fixture.wait(ctx, "link card", || form_request(&fixture).is_some());
        let (request, form) = form_request(&fixture).expect("card");
        assert!(matches!(form, Form::Link { .. }));
        session.respond_form(&request, FormAction::Accept, None);
        fixture.wait_finished(ctx);
        assert_eq!(fixture.results(), [json!({"action": "accept"})]);
        assert_eq!(resolved(&fixture), 1);
        session.shutdown(false);

        // A field form: an answer the form refuses is not sent and leaves the card open.
        let fixture = Fixture::new();
        let session = fixture.session();
        fixture.prompt(
            &session,
            r#"form:{"mode":"form","requestedSchema":{"type":"object","required":["name"],"properties":{"name":{"type":"string"}}}}"#,
        );
        fixture.wait(ctx, "form card", || form_request(&fixture).is_some());
        let (request, _) = form_request(&fixture).expect("card");
        session.respond_form(&request, FormAction::Accept, Some(json!({})));
        assert!(fixture.results().is_empty());
        assert_eq!(resolved(&fixture), 0);
        session.respond_form(&request, FormAction::Accept, Some(json!({"name": "Ada"})));
        fixture.wait_finished(ctx);
        assert_eq!(
            fixture.results(),
            [json!({"action": "accept", "content": {"name": "Ada"}})]
        );
        assert_eq!(resolved(&fixture), 1);
        session.shutdown(false);
    });
}

/// Red-team #382: a link in the project that leads outside it is judged where it leads. The card
/// shows the resolved target and offers no session grant; the grant itself is still exactly
/// the path the card listed.
#[test]
fn scripted_a_symlinked_target_is_shown_resolved_and_never_session_granted() {
    in_loop(|ctx| {
        let fixture = Fixture::new();
        let outside = tempfile::tempdir().expect("outside");
        std::os::unix::fs::symlink(outside.path(), fixture.dir.path().join("out"))
            .expect("symlink");
        let session = fixture.session();
        fixture.prompt(&session, r#"permissions:{"fileSystem":{"write":["out"]}}"#);
        let (request, options, input) = fixture.wait_card(ctx);
        assert_eq!(options, [Decision::Allow, Decision::Deny, Decision::Cancel]);
        let real = std::fs::canonicalize(outside.path()).expect("real");
        assert_eq!(
            input["entries"][0]["target"]["resolved"],
            real.to_string_lossy().as_ref()
        );
        assert_eq!(input["session_allowed"], false);
        assert!(input["warning"]
            .as_str()
            .is_some_and(|w| w.contains("outside the project")));
        session.respond_approval(&request, Decision::AllowForSession);
        assert!(fixture.results().is_empty());
        session.respond_approval(&request, Decision::Allow);
        fixture.wait_finished(ctx);
        // The grant names the place by where it really leads, as the card showed it.
        assert_eq!(
            fixture.results(),
            [json!({"scope": "turn", "permissions": {"fileSystem": {"write": [real]}}})]
        );
        session.shutdown(false);
    });
}

/// A Codex permission frame for the fixture's running turn, fed straight to the session.
fn permission_line(fixture: &Fixture, id: u64) -> String {
    json!({"id": id, "method": "item/permissions/requestApproval", "params": {
        "threadId": "fixture-thread", "turnId": "connection-1-turn-1", "itemId": format!("p{id}"),
        "startedAtMs": 1, "cwd": fixture.dir.path(), "permissions": {"network": {"enabled": true}},
    }})
    .to_string()
}

fn permission_cards(fixture: &Fixture) -> usize {
    fixture
        .seen
        .borrow()
        .iter()
        .filter(|e| {
            matches!(&e.event, Event::ApprovalRequested { tool, .. }
            if tool == agent_core::codex::PERMISSIONS_TOOL)
        })
        .count()
}

/// Lets every held check finish (they are quick; this outlasts them).
fn settle(ctx: &glib::MainContext) {
    pump_until(ctx, 1, || false);
}

/// Rust review #397: a card whose request ends while its targets are being checked is never
/// shown, whether the agent resolved it or the process went away; a duplicate shows once.
#[test]
fn scripted_a_held_permission_card_never_outlives_its_request() {
    in_loop(|ctx| {
        // Resolved by the agent while held.
        let fixture = Fixture::new();
        let session = fixture.session();
        fixture.prompt(&session, "pending");
        fixture.wait_pending(ctx);
        let generation = session.inner.generation.get();
        session
            .inner
            .on_line(generation, &permission_line(&fixture, 900), false);
        session.inner.on_line(
            generation,
            r#"{"method":"serverRequest/resolved","params":{"threadId":"fixture-thread","requestId":900}}"#,
            false,
        );
        settle(ctx);
        assert_eq!(permission_cards(&fixture), 0, "expired while held");
        assert!(session.inner.held_permissions.borrow().is_empty());
        session.shutdown(false);

        // The process ends while held.
        let fixture = Fixture::new();
        let session = fixture.session();
        fixture.prompt(&session, "pending");
        fixture.wait_pending(ctx);
        let generation = session.inner.generation.get();
        session
            .inner
            .on_line(generation, &permission_line(&fixture, 901), false);
        session.shutdown(false);
        settle(ctx);
        assert_eq!(
            permission_cards(&fixture),
            0,
            "the process ended while held"
        );

        // The same request twice while held: one card.
        let fixture = Fixture::new();
        let session = fixture.session();
        fixture.prompt(&session, "pending");
        fixture.wait_pending(ctx);
        let generation = session.inner.generation.get();
        for _ in 0..2 {
            session
                .inner
                .on_line(generation, &permission_line(&fixture, 902), false);
        }
        settle(ctx);
        assert_eq!(permission_cards(&fixture), 1);
        session
            .inner
            .on_line(generation, &permission_line(&fixture, 902), false);
        settle(ctx);
        assert_eq!(
            permission_cards(&fixture),
            1,
            "a shown card is not shown twice"
        );
        session.shutdown(false);
    });
}

/// Red-team #401: an Allow is checked on disk again; if the place changed (here, swapped for a
/// link) nothing is sent and the card is shown again under a new id, as it is now.
#[test]
fn scripted_a_request_that_changed_while_waiting_is_shown_again_not_sent() {
    in_loop(|ctx| {
        let fixture = Fixture::new();
        let session = fixture.session();
        fixture.prompt(
            &session,
            r#"permissions:{"fileSystem":{"write":["build"]}}"#,
        );
        let (request, options, _) = fixture.wait_card(ctx);
        assert!(options.contains(&Decision::AllowForSession));
        let outside = tempfile::tempdir().expect("outside");
        std::os::unix::fs::symlink(outside.path(), fixture.dir.path().join("build"))
            .expect("swap the name for a link");
        session.respond_approval(&request, Decision::AllowForSession);
        fixture.wait(ctx, "the card shown again", || {
            permission_card(&fixture.seen).is_some_and(|(r, ..)| r != request)
        });
        assert!(fixture.results().is_empty(), "nothing was sent");
        assert!(fixture
            .seen
            .borrow()
            .iter()
            .any(|e| e.request.as_deref() == Some(request.as_str())
                && e.event == Event::ApprovalExpired));
        let (again, options, input) = permission_card(&fixture.seen).expect("card");
        assert!(!options.contains(&Decision::AllowForSession));
        assert!(input["warning"]
            .as_str()
            .is_some_and(|w| w.starts_with(super::CHECK_CHANGED)));
        session.respond_approval(&again, Decision::Allow);
        fixture.wait_finished(ctx);
        let real = std::fs::canonicalize(outside.path()).expect("real");
        assert_eq!(
            fixture.results(),
            [json!({"scope": "turn", "permissions": {"fileSystem": {"write": [real]}}})]
        );
        assert!(fixture
            .seen
            .borrow()
            .iter()
            .any(|e| e.request.as_deref() == Some(again.as_str())
                && e.event
                    == Event::ApprovalResolved {
                        decision: Decision::Allow
                    }));
        session.shutdown(false);
    });
}

/// Rust review #403: a check that outlives its deadline vouches for nothing; one in time does.
#[test]
fn a_disk_check_past_its_deadline_vouches_for_nothing() {
    in_loop(|ctx| {
        let slow = super::run_permission_check(
            || {
                std::thread::sleep(std::time::Duration::from_millis(300));
                super::codex_policy::Verdict::default()
            },
            std::time::Duration::from_millis(30),
        );
        assert_eq!(ctx.block_on(slow), None);
        let fast = super::run_permission_check(
            super::codex_policy::Verdict::default,
            std::time::Duration::from_secs(3),
        );
        assert!(ctx.block_on(fast).is_some());
    });
}

/// Rust review #403: making a sub-folder inside a granted folder between showing the card and
/// answering it is not a change: the Allow is sent.
#[test]
fn scripted_a_new_sub_folder_does_not_hold_back_an_allow() {
    in_loop(|ctx| {
        let fixture = Fixture::new();
        std::fs::create_dir_all(fixture.dir.path().join("build")).expect("build");
        let session = fixture.session();
        fixture.prompt(
            &session,
            r#"permissions:{"fileSystem":{"write":["build"]}}"#,
        );
        let (request, options, _) = fixture.wait_card(ctx);
        assert!(options.contains(&Decision::AllowForSession));
        std::fs::create_dir_all(fixture.dir.path().join("build/sub")).expect("sub");
        session.respond_approval(&request, Decision::AllowForSession);
        fixture.wait_finished(ctx);
        assert_eq!(fixture.results().len(), 1);
        assert_eq!(fixture.results()[0]["scope"], "session");
        session.shutdown(false);
    });
}
