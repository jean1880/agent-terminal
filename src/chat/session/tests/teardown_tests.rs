use super::*;

fn turn_completed(session: &ChatSession, state: TurnState) {
    session.inner.emit(Envelope::turn_completed(state, None));
}

fn queued_session(driver: Driver, log: &Log) -> (Rc<Store>, ThreadId, Rc<ChatSession>, Seen) {
    let store = Rc::new(Store::open_in_memory().expect("store"));
    let thread = fresh(&store, "/w");
    let (sink, seen) = make_sink();
    let session = ChatSession::new(
        FakeAdapter::writing_with_open_turn(driver, log),
        open_on("m1"),
        store.clone(),
        thread.clone(),
        sink,
        None,
    );
    let log = log.clone();
    session.set_adapter_factory(Rc::new(move |d| {
        launch_of(FakeAdapter::writing_with_open_turn(d, &log))
    }));
    (store, thread, session, seen)
}

#[test]
fn a_busy_model_switch_keeps_the_queue_for_the_next_process() {
    in_loop(|ctx| {
        let log: Log = Rc::default();
        let (_, _, session, seen) = queued_session(Driver::Agy, &log);
        session.send_prompt("first");
        session.queue_prompt("second");
        let generation = session.inner.generation.get();
        session.switch(Driver::Agy, Some("m2".into()), None);
        // stop_current invalidates old callbacks; start_process adds the new generation.
        assert_eq!(
            session.inner.generation.get(),
            generation + 2,
            "one replacement"
        );
        assert_eq!(session.inner.queued_prompts.borrow().len(), 1);
        let replacement = session.inner.generation.get();
        // Command logs pin the order independently of asynchronous stdin.
        assert_eq!(prompts(&log).len(), 2, "first and the continue prompt only");
        turn_completed(&session, TurnState::Completed);
        assert_eq!(prompts(&log).last().map(String::as_str), Some("second"));
        assert_eq!(prompts(&log).iter().filter(|p| *p == "second").count(), 1);
        assert_eq!(session.inner.generation.get(), replacement);
        // cat echoes actual writes from the replacement; old callbacks are discarded.
        assert!(pump_until(ctx, 5, || has(
            &seen,
            |e| matches!(e, Event::Notice { text } if text == "second")
        )));
        assert_eq!(
            seen.borrow()
                .iter()
                .filter(|e| matches!(&e.event, Event::Notice { text } if text == "second"))
                .count(),
            1
        );
        session.shutdown(false);
    });
}

#[test]
fn a_busy_switch_writes_one_prompt_per_turn() {
    in_loop(|_| {
        let log: Log = Rc::default();
        let (_, _, session, _) = queued_session(Driver::Agy, &log);
        session.send_prompt("first");
        session.queue_prompt("second");
        session.switch(Driver::Claude, Some("sonnet".into()), None);
        let sent = prompts(&log);
        assert_eq!(sent.len(), 2, "one prompt on each process");
        assert!(sent[1].contains("Continue the unfinished task"));
        assert!(
            !sent[1].contains("second"),
            "unsent text is not handoff history"
        );
        assert!(session.status().running_turn);
        turn_completed(&session, TurnState::Interrupted);
        let sent = prompts(&log);
        assert_eq!(sent.len(), 3);
        assert!(
            sent[2].contains("first"),
            "the unconfirmed handoff rides now"
        );
        assert!(sent[2].ends_with("second"));
        session.shutdown(false);
    });
}

/// A message waiting in a start that never spawned, as `note_unspawned_prompt` records one:
/// stored in the thread when `stored`, as a sent message is.
fn plant_unspawned(
    store: &Store,
    thread: &ThreadId,
    session: &ChatSession,
    text: &str,
    stored: bool,
) {
    let item_id = if stored {
        let pt = pt_of(store, thread);
        store
            .append_user_message(thread, Some(&pt), text)
            .expect("store")
    } else {
        "unstored".into()
    };
    *session.inner.starting.borrow_mut() = Some(PendingStart {
        generation: session.inner.generation.get(),
        writes: Vec::new(),
        prompts: vec![QueuedPrompt {
            item_id,
            text: text.into(),
            stored,
        }],
        news: Vec::new(),
    });
}

fn sent_alone(log: &Log, text: &str) -> usize {
    prompts(log).iter().filter(|p| p.as_str() == text).count()
}

/// A busy switch carries the unfinished task itself (handoff and continuation): a stored
/// message waiting in an unspawned start is in its handoff, and never also sent on its own.
/// One that could not be stored is not in it, so it goes after the continuation. An idle switch
/// carries nothing, so the message goes to the new agent, once.
#[test]
fn a_switch_sends_an_unspawned_message_exactly_once() {
    in_loop(|_| {
        let log: Log = Rc::default();
        let (store, thread, session, _) = queued_session(Driver::Agy, &log);
        session.send_prompt("first");
        plant_unspawned(&store, &thread, &session, "append one line", true);
        assert!(session.status().running_turn);
        session.switch(Driver::Claude, Some("sonnet".into()), None);
        assert!(session.inner.queued_prompts.borrow().is_empty());
        assert!(
            session
                .inner
                .pending_handoff
                .borrow()
                .as_ref()
                .is_some_and(|h| h.summary.contains("append one line")),
            "the replacement gets it through the handoff"
        );
        turn_completed(&session, TurnState::Completed);
        assert_eq!(
            sent_alone(&log, "append one line"),
            0,
            "{:?}",
            prompts(&log)
        );
        session.shutdown(false);
    });
    in_loop(|_| {
        let log: Log = Rc::default();
        let (store, thread, session, _) = queued_session(Driver::Agy, &log);
        session.send_prompt("first");
        plant_unspawned(&store, &thread, &session, "never stored", false);
        session.switch(Driver::Claude, Some("sonnet".into()), None);
        assert_eq!(
            session.inner.queued_prompts.borrow().len(),
            1,
            "not in the handoff, so it queues"
        );
        turn_completed(&session, TurnState::Completed);
        assert!(
            prompts(&log)
                .last()
                .is_some_and(|p| p.ends_with("never stored")),
            "{:?}",
            prompts(&log)
        );
        session.shutdown(false);
    });
    in_loop(|_| {
        let log: Log = Rc::default();
        let (store, thread, session, _) = queued_session(Driver::Agy, &log);
        plant_unspawned(&store, &thread, &session, "append one line", true);
        assert!(!session.status().running_turn);
        session.switch(Driver::Claude, Some("sonnet".into()), None);
        let sent = prompts(&log);
        assert_eq!(
            sent.iter()
                .filter(|p| p.ends_with("append one line"))
                .count(),
            1,
            "an idle switch sends it once, to the new agent: {sent:?}"
        );
        session.shutdown(false);
    });
}

/// A replacement that spawns, takes the continuation, then exits before it ever starts rolls
/// back too: the message the switch held is queued again for the old agent, once. A replacement
/// that does start lets it go.
#[test]
fn a_replacement_that_dies_before_starting_gives_the_message_back() {
    in_loop(|_| {
        let log: Log = Rc::default();
        let (store, thread, session, _) = queued_session(Driver::Agy, &log);
        session.send_prompt("first");
        plant_unspawned(&store, &thread, &session, "append one line", true);
        session.switch(Driver::Claude, Some("sonnet".into()), None);
        assert_eq!(session.status().driver, Driver::Claude);
        assert_eq!(session.inner.carried_unsent.borrow().len(), 1, "held");
        // Exits before any session or turn started: the switch fails.
        session
            .inner
            .on_exit(session.inner.generation.get(), Some(1));
        assert_eq!(session.status().driver, Driver::Agy, "rolled back");
        let queued = |text: &str| {
            session
                .inner
                .queued_prompts
                .borrow()
                .iter()
                .filter(|q| q.text == text)
                .count()
        };
        assert_eq!(queued("append one line"), 1);
        assert!(session.inner.carried_unsent.borrow().is_empty());
        session.shutdown(false);
    });
    in_loop(|_| {
        let log: Log = Rc::default();
        let (store, thread, session, _) = queued_session(Driver::Agy, &log);
        session.send_prompt("first");
        plant_unspawned(&store, &thread, &session, "append one line", true);
        session.switch(Driver::Claude, Some("sonnet".into()), None);
        session
            .inner
            .apply(&Envelope::new(Event::TurnStarted { model: None }));
        assert!(
            session.inner.carried_unsent.borrow().is_empty(),
            "a started replacement owns the task"
        );
        assert!(session.inner.queued_prompts.borrow().is_empty());
        session.shutdown(false);
    });
}

/// A busy switch whose replacement cannot spawn rolls back to the old agent and sends no
/// continuation: the message it took over is queued again, never lost.
#[test]
fn a_switch_that_rolls_back_keeps_the_unspawned_message() {
    in_loop(|_| {
        let log: Log = Rc::default();
        let (store, thread, session, _) = queued_session(Driver::Agy, &log);
        let factory_log = log.clone();
        session.set_adapter_factory(Rc::new(move |d| {
            launch_of(if d == Driver::Claude {
                FakeAdapter::unspawnable(d, &factory_log)
            } else {
                FakeAdapter::writing_with_open_turn(d, &factory_log)
            })
        }));
        session.send_prompt("first");
        plant_unspawned(&store, &thread, &session, "append one line", true);
        session.switch(Driver::Claude, Some("sonnet".into()), None);
        assert_eq!(session.status().driver, Driver::Agy, "rolled back");
        assert_eq!(
            session
                .inner
                .queued_prompts
                .borrow()
                .iter()
                .filter(|q| q.text == "append one line")
                .count(),
            1,
            "queued again for the old agent"
        );
        session.shutdown(false);
    });
}

#[test]
fn shutdown_with_a_queue_spawns_nothing() {
    in_loop(|_| {
        let log: Log = Rc::default();
        let (store, thread, session, _) = queued_session(Driver::Agy, &log);
        session.send_prompt("first");
        session.queue_prompt("second");
        let generation = session.inner.generation.get();
        session.shutdown(false);
        assert_eq!(session.inner.generation.get(), generation + 1);
        assert!(!session.status().alive);
        assert_eq!(prompts(&log), ["first"]);
        assert_eq!(
            session.pop_queued_prompt().map(|(_, text)| text),
            Some("second".into())
        );
        assert_eq!(
            store.transcript_messages(&thread).expect("messages").len(),
            1
        );
    });
}

#[test]
fn a_recalled_queued_prompt_leaves_nothing_in_the_store() {
    in_loop(|_| {
        let log: Log = Rc::default();
        let (store, thread, session, _) = queued_session(Driver::Agy, &log);
        session.send_prompt("first");
        let before = store.events(&thread, None, 1000).expect("events").len();
        session.queue_prompt("withdrawn");
        assert_eq!(
            session.pop_queued_prompt().map(|(_, t)| t),
            Some("withdrawn".into())
        );
        assert_eq!(
            store.events(&thread, None, 1000).expect("events").len(),
            before
        );
        session.shutdown(false);
    });
}

#[test]
fn a_queued_prompt_is_stored_when_sent() {
    in_loop(|_| {
        let log: Log = Rc::default();
        let (store, thread, session, seen) = queued_session(Driver::Agy, &log);
        session.send_prompt("first");
        session.queue_prompt("second");
        let id = session.inner.queued_prompts.borrow()[0].item_id.clone();
        assert_eq!(
            store.transcript_messages(&thread).expect("messages").len(),
            1
        );
        turn_completed(&session, TurnState::Completed);
        let events = store.events(&thread, None, 1000).expect("events");
        let starts = events
            .iter()
            .filter(|(_, e)| {
                e.item.as_deref() == Some(&id)
                    && matches!(
                        e.event,
                        Event::ItemStarted {
                            kind: ItemKind::UserMessage,
                            ..
                        }
                    )
            })
            .count();
        assert_eq!(starts, 1);
        assert_eq!(
            store.transcript_messages(&thread).expect("messages").len(),
            2
        );
        assert!(seen.borrow().iter().any(|e| e.item.as_deref() == Some(&id)
            && matches!(&e.event, Event::ItemStarted { title, .. } if title.is_empty())));
        session.shutdown(false);
    });
}

#[test]
fn handoff_transcript_excludes_a_withdrawn_queued_prompt() {
    in_loop(|_| {
        let log: Log = Rc::default();
        let (store, thread, session, _) = queued_session(Driver::Agy, &log);
        session.send_prompt("first");
        session.queue_prompt("withdrawn");
        session.pop_queued_prompt().expect("queued");
        let handoff = build_handoff(
            &store.transcript_messages(&thread).expect("messages"),
            "test",
        );
        assert!(handoff.summary.contains("first"));
        assert!(!handoff.summary.contains("withdrawn"));
        session.shutdown(false);
    });
}

#[test]
fn a_retirement_never_restarts_a_running_turn() {
    in_loop(|_| {
        let log: Log = Rc::default();
        let (session, _) = claude_on(Mode::Ask, &log);
        session.send_prompt("first");
        let generation = session.inner.generation.get();
        session.set_retired(Some(retired("old", "new")));
        session.steer_prompt("also check tests");
        assert_eq!(session.inner.generation.get(), generation);
        assert_eq!(prompts(&log), ["first"]);
        assert!(session.status().running_turn);
        turn_completed(&session, TurnState::Completed);
        session.send_prompt("next");
        assert!(session.inner.retired.borrow().is_none());
        assert!(log
            .borrow()
            .iter()
            .any(|c| matches!(c, Command::SetModel { model, .. } if model == "new")));
        session.shutdown(false);
    });
}

#[test]
fn a_queued_retirement_waits_and_preserves_prompt_order() {
    in_loop(|_| {
        let log: Log = Rc::default();
        let (_, _, session, _) = queued_session(Driver::Agy, &log);
        session.send_prompt("first");
        let generation = session.inner.generation.get();
        session.set_retired(Some(retired("m1", "m2")));
        session.queue_prompt("second");
        session.queue_prompt("third");
        assert_eq!(session.inner.generation.get(), generation);
        turn_completed(&session, TurnState::Completed);
        assert_eq!(prompts(&log), ["first", "second"]);
        assert!(session.inner.retired.borrow().is_none());
        assert!(session.status().running_turn);
        turn_completed(&session, TurnState::Completed);
        assert_eq!(prompts(&log), ["first", "second", "third"]);
        session.shutdown(false);
    });
}

#[test]
fn a_control_on_a_dead_process_is_answered_at_once() {
    in_loop(|_| {
        for driver in [Driver::Claude, Driver::Codex] {
            let log: Log = Rc::default();
            let (_, _, session, seen) = queued_session(driver, &log);
            session.shutdown(false);
            log.borrow_mut().clear();
            let id = session.control(Control::ContextUsage);
            assert!(
                session.inner.proc.borrow().is_none(),
                "the control starts no process to write to"
            );
            assert!(!has(&seen, |e| matches!(e, Event::Error { .. })));
            assert!(seen
                .borrow()
                .iter()
                .any(|e| e.request.as_deref() == Some(&id)
                    && matches!(&e.event, Event::ControlResult { ok: None, error: Some(error) }
                    if error.contains("send a message to start it again"))));
        }
    });
}

#[test]
fn a_failed_control_write_is_answered() {
    in_loop(|_| {
        let log: Log = Rc::default();
        let (_, _, session, seen) = queued_session(Driver::Claude, &log);
        // The status has not caught up with a transport that can no longer write.
        let proc = session.inner.proc.borrow_mut().take();
        drop(proc);
        assert!(session.status().alive);
        let id = session.control(Control::McpStatus);
        assert!(seen
            .borrow()
            .iter()
            .any(|e| e.request.as_deref() == Some(&id)
                && matches!(
                    e.event,
                    Event::ControlResult {
                        ok: None,
                        error: Some(_)
                    }
                )));
        session.shutdown(false);
    });
}

#[test]
fn a_reload_preserves_queued_prompt_order_and_one_prompt_per_turn() {
    in_loop(|_| {
        let log: Log = Rc::default();
        let (_, _, session, _) = queued_session(Driver::Agy, &log);
        session.send_prompt("first");
        session.queue_prompt("second");
        session.reload_session();
        assert_eq!(prompts(&log), ["first", "second"]);
        assert!(session.inner.queued_prompts.borrow().is_empty());
        session.steer_prompt("third");
        assert_eq!(prompts(&log), ["first", "second"]);
        assert!(session.status().running_turn);
        assert_eq!(session.inner.queued_prompts.borrow().len(), 1);
        turn_completed(&session, TurnState::Completed);
        assert_eq!(prompts(&log), ["first", "second", "third"]);
        assert!(session.status().running_turn);
        session.shutdown(false);
    });
}

#[test]
fn a_canary_restart_preserves_queued_prompt_order_and_one_prompt_per_turn() {
    in_loop(|_| {
        let log: Log = Rc::default();
        let (_, _, session, _) = queued_session(Driver::Agy, &log);
        session.send_prompt("first");
        session.queue_prompt("second");
        session.inner.trip_canary();
        assert_eq!(session.status().mode, Mode::Plan);
        assert_eq!(prompts(&log), ["first", "second"]);
        assert!(session.inner.queued_prompts.borrow().is_empty());
        session.steer_prompt("third");
        assert_eq!(prompts(&log), ["first", "second"]);
        assert!(session.status().running_turn);
        assert_eq!(session.inner.queued_prompts.borrow().len(), 1);
        turn_completed(&session, TurnState::Completed);
        assert_eq!(prompts(&log), ["first", "second", "third"]);
        assert!(session.status().running_turn);
        session.shutdown(false);
    });
}

#[test]
fn a_completed_turn_reaches_the_view_before_the_next_user_message() {
    in_loop(|_| {
        let log: Log = Rc::default();
        let (_, _, session, seen) = queued_session(Driver::Agy, &log);
        session.send_prompt("first");
        session.queue_prompt("second");
        let queued_id = session.inner.queued_prompts.borrow()[0].item_id.clone();
        seen.borrow_mut().clear();
        turn_completed(&session, TurnState::Completed);
        let events = seen.borrow();
        let ended = events
            .iter()
            .position(|e| matches!(e.event, Event::TurnCompleted { .. }))
            .expect("turn end");
        let started = events
            .iter()
            .position(|e| {
                e.item.as_deref() == Some(&queued_id)
                    && matches!(
                        e.event,
                        Event::ItemStarted {
                            kind: ItemKind::UserMessage,
                            ..
                        }
                    )
            })
            .expect("next user message");
        assert!(
            ended < started,
            "old turn end must precede queued dispatch: {events:?}"
        );
    });
}

#[test]
fn a_failed_control_write_has_one_transient_result_and_no_stored_error() {
    in_loop(|_| {
        let log: Log = Rc::default();
        let (store, thread, session, seen) = queued_session(Driver::Claude, &log);
        session.inner.proc.borrow_mut().take();
        let before = store.events(&thread, None, 1000).expect("events").len();
        seen.borrow_mut().clear();
        let id = session.control(Control::McpStatus);
        assert_eq!(
            seen.borrow()
                .iter()
                .filter(|e| e.request.as_deref() == Some(&id)
                    && matches!(e.event, Event::ControlResult { .. }))
                .count(),
            1
        );
        assert!(!has(&seen, |e| matches!(e, Event::Error { .. })));
        assert_eq!(
            store.events(&thread, None, 1000).expect("events").len(),
            before
        );
    });
}

#[test]
fn a_dead_codex_process_still_answers_cached_context_usage() {
    in_loop(|_| {
        let store = Rc::new(Store::open_in_memory().expect("store"));
        let thread = fresh(&store, "/w");
        let (sink, seen) = make_sink();
        let session = ChatSession::new(
            Box::new(agent_core::codex::CodexAdapter::new()),
            open_on("m1"),
            store.clone(),
            thread.clone(),
            sink,
            None,
        );
        let generation = session.inner.generation.get();
        session.inner.on_line(generation,
            r#"{"method":"thread/tokenUsage/updated","params":{"threadId":"th-1","turnId":"tu","tokenUsage":{"total":{"totalTokens":9,"inputTokens":5,"cachedInputTokens":0,"outputTokens":4,"reasoningOutputTokens":0},"last":{"totalTokens":7,"inputTokens":4,"cachedInputTokens":0,"outputTokens":3,"reasoningOutputTokens":0},"modelContextWindow":1000}}}"#, false);
        session.shutdown(false);
        seen.borrow_mut().clear();
        let before = store.events(&thread, None, 1000).expect("events").len();
        let id = session.control(Control::ContextUsage);
        assert!(seen
            .borrow()
            .iter()
            .any(|e| e.request.as_deref() == Some(&id)
                && matches!(&e.event, Event::ControlResult { ok: Some(value), error: None }
                if value == &serde_json::json!({"used": 7, "max": 1000}))));
        assert!(!session.status().alive);
        assert_eq!(session.inner.generation.get(), generation + 1);
        assert_eq!(
            store.events(&thread, None, 1000).expect("events").len(),
            before
        );
    });
}

fn session_with_reentering_sink(
    log: &Log,
    reenter: impl Fn(&ChatSession, &Envelope) + 'static,
) -> (Rc<Store>, ThreadId, Rc<ChatSession>) {
    let store = Rc::new(Store::open_in_memory().expect("store"));
    let thread = fresh(&store, "/w");
    let target = Rc::new(RefCell::new(None::<std::rc::Weak<ChatSession>>));
    let callback_target = target.clone();
    let session = ChatSession::new(
        FakeAdapter::writing_with_open_turn(Driver::Agy, log),
        open_on("m1"),
        store.clone(),
        thread.clone(),
        Rc::new(move |event| {
            let session = callback_target.borrow().as_ref().and_then(|s| s.upgrade());
            if let Some(session) = session {
                reenter(&session, event);
            }
        }),
        None,
    );
    *target.borrow_mut() = Some(Rc::downgrade(&session));
    (store, thread, session)
}

#[test]
fn a_restart_from_the_sink_waits_for_current_delivery_before_dispatching() {
    in_loop(|_| {
        for canary in [false, true] {
            let log: Log = Rc::default();
            let callback_log = log.clone();
            let (_, _, session) = session_with_reentering_sink(&log, move |session, env| {
                if matches!(
                    env.event,
                    Event::TurnCompleted {
                        state: TurnState::Completed,
                        ..
                    }
                ) {
                    if canary {
                        session.inner.trip_canary();
                    } else {
                        session.reload_session();
                    }
                    assert_eq!(
                        prompts(&callback_log),
                        ["first"],
                        "dispatch waits until the sink returns"
                    );
                }
            });
            session.send_prompt("first");
            session.queue_prompt("second");
            turn_completed(&session, TurnState::Completed);
            assert_eq!(prompts(&log), ["first", "second"]);
            assert!(session.status().running_turn);
            session.shutdown(false);
        }
    });
}

#[test]
fn a_restart_during_queued_delivery_dispatches_after_requeueing() {
    in_loop(|ctx| {
        for canary in [false, true] {
            let log: Log = Rc::default();
            let callback_log = log.clone();
            let armed = Rc::new(Cell::new(false));
            let callback_armed = armed.clone();
            let (_, _, session) = session_with_reentering_sink(&log, move |session, env| {
                if matches!(&env.event, Event::ItemStarted { kind: ItemKind::UserMessage, title, .. } if title.is_empty())
                    && callback_armed.replace(false)
                {
                    if canary {
                        session.inner.trip_canary();
                    } else {
                        session.reload_session();
                    }
                    assert_eq!(
                        prompts(&callback_log),
                        ["first"],
                        "restart never dispatches inside the sink"
                    );
                }
            });
            session.send_prompt("first");
            session.queue_prompt("second");
            armed.set(true);
            turn_completed(&session, TurnState::Completed);
            assert!(pump_until(ctx, 5, || prompts(&log) == ["first", "second"]));
            assert!(session.status().running_turn);
            assert!(session.inner.queued_prompts.borrow().is_empty());
            session.shutdown(false);
        }
    });
}

#[test]
fn a_direct_prompt_store_failure_keeps_unique_ids_after_reopen() {
    in_loop(|_| {
        let store = Rc::new(Store::open_in_memory().expect("store"));
        let thread = fresh(&store, "/w");
        let log: Log = Rc::default();
        let mut ids = Vec::new();
        for round in 0..2 {
            let (sink, seen) = make_sink();
            let session = ChatSession::new(
                FakeAdapter::writing_with_open_turn(Driver::Agy, &log),
                open_on("m1"),
                store.clone(),
                thread.clone(),
                sink,
                None,
            );
            let provider = session
                .inner
                .provider_thread
                .replace("missing-provider".into());
            session.send_prompt(&format!("message {round}"));
            ids.push(
                seen.borrow()
                    .iter()
                    .find_map(|env| {
                        matches!(
                            env.event,
                            Event::ItemStarted {
                                kind: ItemKind::UserMessage,
                                ..
                            }
                        )
                        .then(|| env.item.clone())
                        .flatten()
                    })
                    .expect("user item"),
            );
            session.inner.provider_thread.replace(provider);
            session.shutdown(false);
        }
        assert_ne!(
            ids[0], ids[1],
            "a failed write never falls back to a reused session counter"
        );
        assert_eq!(prompts(&log), ["message 0", "message 1"]);
    });
}

#[test]
fn a_failed_reload_keeps_queued_text_unsent() {
    in_loop(|_| {
        let log: Log = Rc::default();
        let (store, thread, session, _) = queued_session(Driver::Agy, &log);
        session.send_prompt("first");
        session.queue_prompt("second");
        *session.inner.adapter.borrow_mut() = FakeAdapter::unspawnable(Driver::Agy, &log);
        session.reload_session();
        assert!(!session.status().alive);
        assert_eq!(prompts(&log), ["first"]);
        assert_eq!(
            session.pop_queued_prompt().map(|(_, text)| text),
            Some("second".into())
        );
        assert_eq!(
            store.transcript_messages(&thread).expect("messages").len(),
            1
        );
    });
}

#[test]
fn a_sink_shutdown_on_turn_completion_cancels_pending_dispatch() {
    in_loop(|_| {
        let log: Log = Rc::default();
        let (store, thread, session) = session_with_reentering_sink(&log, |session, env| {
            if matches!(
                env.event,
                Event::TurnCompleted {
                    state: TurnState::Completed,
                    ..
                }
            ) {
                session.shutdown(false);
            }
        });
        session.send_prompt("first");
        session.queue_prompt("second");
        turn_completed(&session, TurnState::Completed);
        assert_eq!(prompts(&log), ["first"]);
        assert!(!session.status().alive);
        assert_eq!(
            session.pop_queued_prompt().map(|(_, t)| t),
            Some("second".into())
        );
        assert_eq!(
            store.transcript_messages(&thread).expect("messages").len(),
            1
        );
    });
}

#[test]
fn a_sink_shutdown_during_queued_delivery_keeps_the_message_unsent() {
    in_loop(|_| {
        let log: Log = Rc::default();
        let armed = Rc::new(Cell::new(false));
        let callback_armed = armed.clone();
        let (store, thread, session) = session_with_reentering_sink(&log, move |session, env| {
            if matches!(&env.event, Event::ItemStarted { kind: ItemKind::UserMessage, title, .. } if title.is_empty())
                && callback_armed.replace(false)
            {
                session.shutdown(false);
            }
        });
        session.send_prompt("first");
        session.queue_prompt("second");
        armed.set(true);
        turn_completed(&session, TurnState::Completed);
        assert_eq!(prompts(&log), ["first"]);
        assert!(!session.status().alive);
        assert_eq!(
            session.pop_queued_prompt().map(|(_, t)| t),
            Some("second".into())
        );
        assert_eq!(
            store.transcript_messages(&thread).expect("messages").len(),
            1
        );
    });
}

#[test]
fn a_sink_message_during_queued_delivery_waits_for_its_own_turn() {
    in_loop(|_| {
        let log: Log = Rc::default();
        let armed = Rc::new(Cell::new(false));
        let callback_armed = armed.clone();
        let (_, _, session) = session_with_reentering_sink(&log, move |session, env| {
            if matches!(&env.event, Event::ItemStarted { kind: ItemKind::UserMessage, title, .. } if title.is_empty())
                && callback_armed.replace(false)
            {
                session.send_prompt("third");
            }
        });
        session.send_prompt("first");
        session.queue_prompt("second");
        armed.set(true);
        turn_completed(&session, TurnState::Completed);
        assert_eq!(prompts(&log), ["first", "second"]);
        assert_eq!(session.inner.queued_prompts.borrow().len(), 1);
        turn_completed(&session, TurnState::Completed);
        assert_eq!(prompts(&log), ["first", "second", "third"]);
        session.shutdown(false);
    });
}

#[test]
fn a_sink_shutdown_on_a_retirement_notice_keeps_the_queue_unsent() {
    in_loop(|_| {
        let log: Log = Rc::default();
        let (store, thread, session) = session_with_reentering_sink(&log, |session, env| {
            if matches!(&env.event, Event::Notice { text } if text.contains("was retired")) {
                session.shutdown(false);
            }
        });
        session.send_prompt("first");
        session.queue_prompt("second");
        session.set_retired(Some(retired("m1", "m2")));
        turn_completed(&session, TurnState::Completed);
        assert_eq!(prompts(&log), ["first"]);
        assert!(!session.status().alive);
        assert_eq!(
            session.pop_queued_prompt().map(|(_, t)| t),
            Some("second".into())
        );
        assert_eq!(
            store.transcript_messages(&thread).expect("messages").len(),
            1
        );
    });
}

#[test]
fn an_unspawnable_switch_clears_its_pending_model() {
    in_loop(|_| {
        let (_, _, _, session, _, _) =
            agy_thread_switching_to(FakeAdapter::unspawnable, FakeAdapter::unboxed_dyn);
        session.switch(Driver::Claude, Some("opus".into()), Some("high".into()));
        assert_eq!(session.status().driver, Driver::Agy);
        assert_eq!(session.status().pending_model, None);
        assert_eq!(session.status().effort, None);
        assert_eq!(session.inner.open.borrow().effort, None);
        session.shutdown(false);
    });
}

#[test]
fn an_idle_switch_sends_the_queue_with_its_new_handoff() {
    in_loop(|_| {
        let log: Log = Rc::default();
        let (_, _, session, _) = queued_session(Driver::Agy, &log);
        session.send_prompt("first");
        session.queue_prompt("second");
        session.inner.stop_current();
        session.switch(Driver::Claude, Some("sonnet".into()), None);
        let sent = prompts(&log);
        assert_eq!(sent.len(), 2);
        assert!(sent[1].contains("first"));
        assert!(sent[1].ends_with("second"));
        assert!(!sent[1].contains("Continue the unfinished task"));
        assert!(session.status().running_turn);
        session.shutdown(false);
    });
}

#[test]
fn a_refused_steer_is_not_stored_twice_at_dispatch() {
    in_loop(|_| {
        let store = Rc::new(Store::open_in_memory().expect("store"));
        let thread = fresh(&store, "/w");
        let log: Log = Rc::default();
        let (sink, _) = make_sink();
        let adapter = Box::new(FakeAdapter {
            refuse_steer: true,
            ..*FakeAdapter::unboxed(Driver::Codex, &log)
        });
        let session = ChatSession::new(
            adapter,
            open_on("m1"),
            store.clone(),
            thread.clone(),
            sink,
            None,
        );
        session.send_prompt("first");
        session.steer_prompt("second");
        assert_eq!(session.inner.queued_prompts.borrow().len(), 1);
        assert_eq!(
            store.transcript_messages(&thread).expect("messages").len(),
            2
        );
        turn_completed(&session, TurnState::Completed);
        assert_eq!(prompts(&log), ["first", "second"]);
        assert_eq!(
            store.transcript_messages(&thread).expect("messages").len(),
            2
        );
        session.shutdown(false);
    });
}

#[test]
fn a_refused_steer_with_a_failed_store_write_keeps_a_unique_id_after_reopen() {
    in_loop(|_| {
        let store = Rc::new(Store::open_in_memory().expect("store"));
        let thread = fresh(&store, "/w");
        let log: Log = Rc::default();
        let mut ids = Vec::new();
        for round in 0..2 {
            let (sink, _) = make_sink();
            let adapter = Box::new(FakeAdapter {
                refuse_steer: true,
                ..*FakeAdapter::unboxed(Driver::Codex, &log)
            });
            let session = ChatSession::new(
                adapter,
                open_on("m1"),
                store.clone(),
                thread.clone(),
                sink,
                None,
            );
            session.send_prompt(&format!("first {round}"));
            let provider = session
                .inner
                .provider_thread
                .replace("missing-provider".into());
            session.steer_prompt(&format!("second {round}"));
            ids.push(session.inner.queued_prompts.borrow()[0].item_id.clone());
            session.inner.provider_thread.replace(provider);
            turn_completed(&session, TurnState::Completed);
            session.shutdown(false);
        }
        assert_ne!(
            ids[0], ids[1],
            "reopening must never reuse a persisted item id"
        );
        let messages = store.transcript_messages(&thread).expect("messages");
        assert_eq!(
            messages.len(),
            4,
            "the refused steers are stored at dispatch"
        );
        assert!(messages.iter().any(|m| m.text == "second 0"));
        assert!(messages.iter().any(|m| m.text == "second 1"));
    });
}
