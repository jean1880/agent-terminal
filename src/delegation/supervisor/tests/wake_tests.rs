//! A root hears its sub-agents' results without polling: an idle root is woken with them, a
//! busy one gets them after its turn, one that already waited for them is not told twice, and a
//! stopped one is not woken at all.
use super::*;
use crate::chat::ChatBackend;
use agent_core::event::{Envelope, Event};

type Seen = Rc<RefCell<Vec<Envelope>>>;

/// The root's fake Claude, in its own directory (the child's writes the shared one): it takes
/// the handshake, ends Claude's prompt-less init turn, then logs every line it is sent and
/// completes a turn for each prompt, holding that turn while a `hold` file exists.
struct Root {
    dir: PathBuf,
    session: Rc<ChatSession>,
    seen: Seen,
    thread: String,
}

impl Root {
    fn prompts(&self) -> String {
        std::fs::read_to_string(self.dir.join("prompts.log")).unwrap_or_default()
    }

    fn wakes(&self) -> usize {
        self.prompts()
            .lines()
            .filter(|l| l.contains("[agent-terminal] Sub-agent worker"))
            .count()
    }

    fn notices(&self) -> Vec<String> {
        self.seen
            .borrow()
            .iter()
            .filter_map(|e| match &e.event {
                Event::Notice { text } if text.starts_with("[agent-terminal]") => {
                    Some(text.clone())
                }
                _ => None,
            })
            .collect()
    }

    fn turns_ended(&self) -> usize {
        self.seen
            .borrow()
            .iter()
            .filter(|e| matches!(e.event, Event::TurnCompleted { .. }))
            .count()
    }

    fn hold(&self, on: bool) {
        let path = self.dir.join("hold");
        if on {
            std::fs::write(path, "").unwrap();
        } else {
            std::fs::remove_file(path).unwrap();
        }
    }
}

async fn root_session(sup: &Rc<Supervisor>, dir: &std::path::Path, root: &WorkerId) -> Root {
    let rdir = dir.join("root");
    std::fs::create_dir(&rdir).unwrap();
    let script = rdir.join("agent.sh");
    std::fs::write(
        &script,
        "D=$(dirname \"$0\")\n\
         IFS= read -r handshake\n\
         echo '{\"type\":\"system\",\"subtype\":\"init\",\"session_id\":\"s1\",\"model\":\"m\",\"claude_code_version\":\"2.1.293\"}'\n\
         echo '{\"type\":\"result\",\"subtype\":\"success\",\"is_error\":false}'\n\
         while IFS= read -r line; do printf '%s\\n' \"$line\" >> \"$D/prompts.log\"; \
         case \"$line\" in *'\"type\":\"user\"'*) \
         while [ -e \"$D/hold\" ]; do sleep 0.02; done; \
         echo '{\"type\":\"result\",\"subtype\":\"success\",\"is_error\":false}';; esac; done\n",
    )
    .unwrap();
    let thread = sup.state.borrow().roots[root].row.root_thread_id.clone();
    let provider = sup
        .services
        .handle
        .reader()
        .add_provider_thread(&thread, "claude", "default")
        .unwrap();
    let open = agent_core::adapter::OpenSession {
        program: "/bin/sh".into(),
        extra_args: vec![script.to_string_lossy().into_owned()],
        cwd: rdir.to_string_lossy().into_owned(),
        model: None,
        effort: None,
        mode: Mode::Ask,
        resume: None,
        new_session_id: None,
        approval_hook: false,
        delegation: None,
    };
    let seen: Seen = Rc::default();
    let sink = {
        let seen = seen.clone();
        Rc::new(move |e: &Envelope| seen.borrow_mut().push(e.clone()))
    };
    let session = ChatSession::with_options(
        Box::new(agent_core::claude::ClaudeAdapter::new()),
        open,
        crate::chat::session::SessionStore {
            handle: sup.services.handle.clone(),
            provider: Some(Some(provider)),
        },
        thread.clone(),
        sink,
        Err(None),
        crate::agent_proc::AgentEnv::default(),
        crate::delegation::seam::SessionOptions {
            generation_base: sup.root_generation_base(&thread),
            hooks: None,
            child: None,
        },
    );
    sup.attach_root_session(&thread, Rc::downgrade(&session));
    let root = Root {
        dir: rdir,
        session,
        seen,
        thread,
    };
    pump_until(|| root.turns_ended() >= 1 && !root.session.status().running_turn).await;
    root
}

fn worker_of(receipt: &Value) -> WorkerId {
    WorkerId::parse(receipt["worker_id"].as_str().unwrap()).unwrap()
}

/// Longer than a coalescing delay and a retry together: what has not happened by then is not
/// merely late.
async fn settle() {
    glib::timeout_future(
        crate::delegation::wake::WAKE_COALESCE + crate::delegation::wake::WAKE_RETRY * 2,
    )
    .await;
}

async fn completed(sup: &Rc<Supervisor>, worker: &WorkerId) {
    pump_until(|| {
        let state = sup.state.borrow();
        let w = &state.workers[worker];
        assert_ne!(w.row.state, "failed", "{:?}", w.row.delegation_reason);
        w.row.state == "completed"
    })
    .await;
}

/// The root's held turn ends, as the window reports it.
async fn release(sup: &Rc<Supervisor>, root: &Root) {
    let ended = root.turns_ended();
    root.hold(false);
    pump_until(|| root.turns_ended() > ended && !root.session.status().running_turn).await;
    sup.on_root_turn_end(&root.thread);
}

#[test]
fn an_idle_root_is_woken_once_with_its_childs_result() {
    in_context(async {
        let (dir, sup, root) = harness();
        let r = root_session(&sup, dir.path(), &root).await;
        let worker = worker_of(
            &sup.spawn_worker(&root, spawn_request("wake-idle"))
                .await
                .unwrap(),
        );
        completed(&sup, &worker).await;
        pump_until(|| r.wakes() == 1).await;
        let header = format!(
            "[agent-terminal] Sub-agent worker (claude · synthetic-model, {worker}) finished: \
             completed."
        );
        let prompts = r.prompts();
        assert!(prompts.contains(&header), "{prompts}");
        assert!(prompts.contains("not a message from the user"));
        // Shown in the thread as the app's notice, never as the user's message.
        let notices = r.notices();
        assert_eq!(notices.len(), 1, "{notices:?}");
        assert!(notices[0].starts_with(&header));
        assert!(!r.seen.borrow().iter().any(|e| matches!(
            &e.event,
            Event::ContentSnapshot { text, .. } if text.contains("[agent-terminal]")
        )));
        settle().await;
        assert_eq!(r.wakes(), 1, "told once");

        // A report the child sends its idle parent wakes it too, with the report's text,
        // redacted.
        pump_until(|| !r.session.status().running_turn).await;
        connect(&sup, &worker, ConnId(42)).await;
        let secret = format!("ghp_{}", "B".repeat(36));
        tool(
            &sup,
            ConnId(42),
            1,
            ToolRequest::Send(SendRequest {
                target: Target::Parent,
                text: format!("halfway there; token {secret}"),
                kind: agent_core::delegation::mailbox::MessageKind::Report,
                reply_to: None,
                delivery: Delivery::NextTurn,
                expected_run_id: None,
                expected_authority_epoch: None,
                idempotency_key: IdempotencyKey::parse("wake-report").unwrap(),
            }),
        )
        .await;
        pump_until(|| r.wakes() == 2).await;
        let prompts = r.prompts();
        assert!(prompts.contains(") sent you a message.\\nhalfway there; token "));
        assert!(!prompts.contains(&secret));
        assert!(!r.notices().iter().any(|n| n.contains(&secret)));
        r.session.shutdown(false);
        stop(&sup).await;
    });
}

#[test]
fn a_busy_root_that_did_not_wait_is_told_after_its_turn() {
    in_context(async {
        let (dir, sup, root) = harness();
        let r = root_session(&sup, dir.path(), &root).await;
        r.hold(true);
        r.session.send_prompt("busy with my own work");
        pump_until(|| r.prompts().contains("busy with my own work")).await;
        let worker = worker_of(
            &sup.spawn_worker(&root, spawn_request("wake-busy"))
                .await
                .unwrap(),
        );
        completed(&sup, &worker).await;
        settle().await;
        assert_eq!(r.wakes(), 0, "never cut into a running turn");
        assert!(r.notices().is_empty());
        release(&sup, &r).await;
        pump_until(|| r.wakes() == 1).await;
        assert!(r
            .prompts()
            .contains(&format!("{worker}) finished: completed.")));
        assert_eq!(r.notices().len(), 1);
        r.session.shutdown(false);
        stop(&sup).await;
    });
}

#[test]
fn a_root_that_waited_for_the_result_is_not_told_it_again() {
    in_context(async {
        let (dir, sup, root) = harness();
        let r = root_session(&sup, dir.path(), &root).await;
        connect(&sup, &root, ConnId(41)).await;
        r.hold(true);
        r.session.send_prompt("spawn and wait");
        pump_until(|| r.prompts().contains("spawn and wait")).await;
        let worker = worker_of(
            &sup.spawn_worker(&root, spawn_request("wake-waited"))
                .await
                .unwrap(),
        );
        completed(&sup, &worker).await;
        let waited = tool(
            &sup,
            ConnId(41),
            1,
            ToolRequest::Wait(WaitRequest {
                workers: Some(vec![worker.clone()]),
                timeout_ms: Some(1000),
                ..Default::default()
            }),
        )
        .await;
        // The wait was handed the run's end and the result's report.
        assert!(waited.to_string().contains("turn completed"), "{waited:#}");
        assert_eq!(waited["messages"][0]["kind"], "report", "{waited:#}");
        release(&sup, &r).await;
        settle().await;
        assert_eq!(r.wakes(), 0, "the wait already delivered it");
        assert!(r.notices().is_empty());
        r.session.shutdown(false);
        stop(&sup).await;
    });
}

#[test]
fn a_stopped_root_is_not_woken() {
    in_context(async {
        let (dir, sup, root) = harness();
        let r = root_session(&sup, dir.path(), &root).await;
        r.hold(true);
        r.session.send_prompt("start some work");
        pump_until(|| r.prompts().contains("start some work")).await;
        let worker = worker_of(
            &sup.spawn_worker(&root, spawn_request("wake-stopped"))
                .await
                .unwrap(),
        );
        completed(&sup, &worker).await;
        sup.root_stop(&r.thread);
        release(&sup, &r).await;
        settle().await;
        assert_eq!(r.wakes(), 0, "the user stopped it");
        assert!(r.notices().is_empty());
        assert_eq!(
            sup.state
                .borrow()
                .wakes
                .get(&root)
                .map_or(0, |q| q.pending()),
            0,
            "nothing is kept for later either"
        );
        r.session.shutdown(false);
        stop(&sup).await;
    });
}
