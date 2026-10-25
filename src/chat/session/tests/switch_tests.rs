//! A switch that waits for the store writer before it reads the thread, and what happens when
//! something overtakes it meanwhile: a stop, a canary restart, or the end of the turn it was to
//! continue. Fake adapters over `cat`; a real store writer kept busy by a slow write.

use super::*;
use agent_kit::store_writer::StoreWriter;

struct Slow {
    _tmp: tempfile::TempDir,
    writer: StoreWriter,
    session: Rc<ChatSession>,
    seen: Seen,
    /// What the agent the thread started on was sent.
    log: Log,
    /// What an agent the factory makes (the switch's new one) was sent.
    new_log: Log,
}

/// A thread on `old` over a queued store, whose factory makes fake agents.
fn slow(old: Driver) -> Slow {
    slow_with(old, |_, _| {})
}

/// [`slow`] on a thread `seed` fills first.
fn slow_with(old: Driver, seed: impl FnOnce(&Store, &ThreadId)) -> Slow {
    let tmp = tempfile::tempdir().expect("tmp");
    let path = tmp.path().join("threads.sqlite");
    let store = Rc::new(Store::open(&path).expect("store"));
    let thread = fresh(&store, "/w");
    seed(&store, &thread);
    let writer = StoreWriter::spawn(&path).expect("writer");
    let handle = StoreHandle::Queued {
        reader: store,
        writer: writer.clone(),
    };
    let log: Log = Rc::default();
    let new_log: Log = Rc::default();
    let open = if old == Driver::Claude {
        open_on("sonnet")
    } else {
        OpenSession {
            model: Some("gemini-pro".into()),
            mode: Mode::Plan,
            ..open_session("unused", Vec::new(), "/")
        }
    };
    let (session, seen) = harness(FakeAdapter::boxed(old, &log), open, &handle, &thread);
    let factory_log = new_log.clone();
    session.set_adapter_factory(Rc::new(move |d| {
        launch_of(FakeAdapter::boxed(d, &factory_log))
    }));
    Slow {
        _tmp: tmp,
        writer,
        session,
        seen,
        log,
        new_log,
    }
}

impl Slow {
    /// Keeps the writer busy for `ms` with a write queued before anything that follows.
    fn hold_writer(&self, ms: u64) {
        self.writer
            .write("a slow write", None, move |_| {
                std::thread::sleep(Duration::from_millis(ms));
                Ok(())
            })
            .expect("queued");
    }

    fn cancelled_notice(&self) -> bool {
        has(&self.seen, |e| {
            matches!(e, Event::Notice { text }
                if text.starts_with("The switch to Claude was cancelled, so 1 message written \
                                     for it was not sent."))
        })
    }

    fn sent_anywhere(&self, needle: &str) -> bool {
        prompts(&self.log)
            .iter()
            .chain(prompts(&self.new_log).iter())
            .any(|p| p.contains(needle))
    }
}

/// How a switch still waiting for the writer is overtaken, in the tests below.
#[derive(Debug, Clone, Copy)]
enum Overtaken {
    /// The toolbar's Stop.
    Stop,
    /// An approval card's Stop turn.
    StopTurn,
    /// agy ran a tool without its hook: the canary restarts it.
    Canary,
}

/// A message written for the new agent while the switch waited is never sent to the old one
/// unasked once the switch is dropped: the user is told, and it waits in the queue until the
/// user sends again (then it goes, with the new one, to the agent the thread is on).
#[test]
fn a_dropped_switch_holds_the_messages_written_for_its_new_agent_and_says_so() {
    for how in [Overtaken::Stop, Overtaken::StopTurn, Overtaken::Canary] {
        in_loop(|ctx| {
            let s = slow(Driver::Agy);
            s.hold_writer(800);
            s.session.switch(Driver::Claude, Some("opus".into()), None);
            s.session.send_prompt("for Claude");
            assert!(!s.sent_anywhere("for Claude"), "it waits for the switch");
            match how {
                Overtaken::Stop => ChatBackend::interrupt(&*s.session),
                Overtaken::StopTurn => {
                    s.session
                        .inner
                        .finish_hook_approval("h1", Decision::Cancel, false);
                }
                Overtaken::Canary => s.session.inner.trip_canary(),
            }
            assert!(s.cancelled_notice(), "the user is told ({how:?})");
            // Well past the writer's wait: the switch never happens, nothing is sent.
            assert!(
                !pump_until(ctx, 2, || s.sent_anywhere("for Claude")),
                "a message for Claude was sent ({how:?})"
            );
            assert_eq!(s.session.status().driver, Driver::Agy, "{how:?}");
            assert_eq!(s.session.inner.queued_prompts.borrow().len(), 1, "{how:?}");
            // The user acts: the queue goes to the agent the thread is on, in order (the new
            // message after the held one, behind its turn).
            s.session.send_prompt("and this");
            assert!(
                pump_until(ctx, 5, || prompts(&s.log)
                    .first()
                    .is_some_and(|p| p.contains("for Claude"))),
                "the held message never went once the user sent again ({how:?}): {:?}",
                prompts(&s.log)
            );
            let queued_or_sent = s
                .session
                .inner
                .queued_prompts
                .borrow()
                .iter()
                .any(|q| q.text == "and this")
                || s.sent_anywhere("and this");
            assert!(queued_or_sent, "{how:?}");
            assert!(prompts(&s.new_log).is_empty(), "{how:?}");
            s.session.shutdown(false);
        });
    }
}

/// Taking the held message back (the composer's edit) ends the hold: nothing is left to send.
#[test]
fn taking_the_held_message_back_ends_the_hold() {
    in_loop(|ctx| {
        let s = slow(Driver::Agy);
        s.hold_writer(300);
        s.session.switch(Driver::Claude, Some("opus".into()), None);
        s.session.send_prompt("for Claude");
        ChatBackend::interrupt(&*s.session);
        assert!(s.cancelled_notice());
        let taken = s.session.pop_queued_prompt().expect("held");
        assert_eq!(taken.1, "for Claude");
        // The queue goes as usual again: the next message is sent, the one taken back is not.
        s.session.send_prompt("something else");
        assert!(pump_until(ctx, 5, || s.sent_anywhere("something else")));
        assert!(!pump_until(ctx, 1, || s.sent_anywhere("for Claude")));
        s.session.shutdown(false);
    });
}

/// A busy switch whose turn ended while the thread was read has nothing to continue: it goes
/// ahead as an idle switch and never sends the continuation, which would redo finished work.
#[test]
fn a_busy_switch_whose_turn_ended_meanwhile_sends_no_continuation() {
    for old in [Driver::Claude, Driver::Agy] {
        in_loop(|ctx| {
            let s = slow(old);
            s.session.send_prompt("the task");
            assert!(s.session.status().running_turn);
            s.hold_writer(500);
            s.session.switch(Driver::Claude, Some("opus".into()), None);
            s.session.inner.emit(Envelope::new(Event::TurnCompleted {
                state: TurnState::Completed,
                error: None,
                usage: None,
                cost_usd: None,
            }));
            assert!(!s.session.status().running_turn);
            let switched = || {
                if old == Driver::Claude {
                    s.session.inner.open.borrow().model.as_deref() == Some("opus")
                        || s.session.status().pending_model.as_deref() == Some("opus")
                } else {
                    s.session.status().driver == Driver::Claude
                }
            };
            assert!(pump_until(ctx, 5, switched), "no switch ({old:?})");
            // Past any continuation the switch could still send.
            pump_until(ctx, 1, || false);
            assert!(
                !s.sent_anywhere("Continue the unfinished task"),
                "a finished turn was continued ({old:?}): {:?} / {:?}",
                prompts(&s.log),
                prompts(&s.new_log)
            );
            s.session.shutdown(false);
        });
    }
}

fn failed_notice(s: &Slow) -> bool {
    has(&s.seen, |e| {
        matches!(e, Event::Notice { text }
            if text.starts_with("The switch to Claude failed, so 1 message written for it was \
                                 not sent."))
    })
}

/// How a switch to another agent fails once it has waited.
#[derive(Debug, Clone, Copy)]
enum Failure {
    /// The store writer never reads the thread (here: it is gone).
    Read,
    /// The new agent cannot be made.
    Factory,
}

/// A switch to another agent that fails after the wait never sends what the user wrote for the
/// new agent to the outgoing one, even in dangerous permissive mode: it is held, and the user
/// told, as for a cancelled switch.
#[test]
fn a_failed_switch_never_sends_the_messages_written_for_its_new_agent_to_the_old_one() {
    use agent_kit::store_writer::Shutdown;
    for how in [Failure::Read, Failure::Factory] {
        in_loop(|ctx| {
            let s = slow(Driver::Agy);
            s.session.confirm_mode(Mode::Bypass);
            match how {
                Failure::Read => {
                    assert_eq!(s.writer.shutdown(Duration::from_secs(5)), Shutdown::Drained);
                }
                Failure::Factory => {
                    s.session
                        .set_adapter_factory(Rc::new(|_| Err("Claude is not ready".into())));
                    s.hold_writer(300);
                }
            }
            s.session.switch(Driver::Claude, Some("opus".into()), None);
            s.session.send_prompt("for Claude");
            assert!(
                pump_until(ctx, 5, || failed_notice(&s)),
                "the user is not told ({how:?})"
            );
            assert!(
                !pump_until(ctx, 1, || s.sent_anywhere("for Claude")),
                "a message for Claude went to agy ({how:?})"
            );
            assert_eq!(s.session.status().driver, Driver::Agy, "{how:?}");
            assert_eq!(s.session.inner.queued_prompts.borrow().len(), 1, "{how:?}");
            s.session.shutdown(false);
        });
    }
}

/// A busy switch on the same agent whose read fails: the task was not carried, so what the user
/// wrote for the new model is held (the one rule for a switch that did not happen), and the end
/// of the old turn does not send it. One whose turn ended first goes ahead as an idle switch,
/// whose read is not needed: the message then goes, on the new model.
#[test]
fn a_busy_same_agent_switch_whose_read_fails_holds_or_sends_on_the_new_model() {
    use agent_kit::store_writer::Shutdown;
    for turn_ends_first in [false, true] {
        in_loop(|ctx| {
            let s = slow(Driver::Claude);
            s.session.send_prompt("the task");
            assert!(s.session.status().running_turn);
            assert_eq!(s.writer.shutdown(Duration::from_secs(5)), Shutdown::Drained);
            s.session.switch(Driver::Claude, Some("opus".into()), None);
            s.session.send_prompt("for opus");
            let end_turn = || {
                s.session.inner.emit(Envelope::new(Event::TurnCompleted {
                    state: TurnState::Completed,
                    error: None,
                    usage: None,
                    cost_usd: None,
                }));
            };
            if turn_ends_first {
                end_turn();
                let set_then_sent = || {
                    let log = s.log.borrow();
                    let set = log.iter().position(
                        |c| matches!(c, Command::SetModel { model, .. } if model == "opus"),
                    );
                    let sent = log.iter().position(
                        |c| matches!(c, Command::Prompt { text } if text.contains("for opus")),
                    );
                    matches!((set, sent), (Some(a), Some(b)) if a < b)
                };
                assert!(
                    pump_until(ctx, 5, set_then_sent),
                    "not sent on the new model: {:?}",
                    s.log.borrow()
                );
            } else {
                assert!(pump_until(ctx, 5, || failed_notice(&s)), "not told");
                end_turn();
                assert!(
                    !pump_until(ctx, 1, || s.sent_anywhere("for opus")),
                    "sent on the old model"
                );
                assert_eq!(s.session.inner.queued_prompts.borrow().len(), 1);
            }
            assert!(!s.sent_anywhere("Continue the unfinished task"));
            s.session.shutdown(false);
        });
    }
}

/// The whole switch keeps the window responsive on a long thread: the transcript is read and
/// the handoff built off the main thread, so no main-loop iteration of the switch, from the
/// click to the new agent, takes long.
#[test]
fn a_switch_on_a_long_thread_never_holds_the_main_loop() {
    in_loop(|ctx| {
        let s = slow_with(Driver::Agy, |store, thread| {
            let pt = pt_of(store, thread);
            let line = "a long line of earlier work in this thread ".repeat(12);
            store
                .atomically(|st| {
                    for i in 0..6000 {
                        st.append_user_message(thread, Some(&pt), &format!("{i}: {line}"))?;
                    }
                    Ok(())
                })
                .expect("seed");
        });
        let started = std::time::Instant::now();
        s.session.switch(Driver::Claude, Some("opus".into()), None);
        let mut longest = started.elapsed();
        let deadline = std::time::Instant::now() + Duration::from_secs(30);
        while s.session.status().driver != Driver::Claude {
            assert!(
                std::time::Instant::now() < deadline,
                "the switch never ended"
            );
            let at = std::time::Instant::now();
            ctx.iteration(false);
            longest = longest.max(at.elapsed());
            std::thread::sleep(Duration::from_millis(1));
        }
        assert!(
            longest < Duration::from_millis(150),
            "a main-loop step of the switch took {longest:?}"
        );
        s.session.shutdown(false);
    });
}

// ---- the composer's own paths: steer_prompt and queue_prompt ----

fn steered(log: &Log, needle: &str) -> bool {
    log.borrow()
        .iter()
        .any(|c| matches!(c, Command::Steer { text } if text.contains(needle)))
}

fn end_turn(session: &ChatSession) {
    session.inner.emit(Envelope::new(Event::TurnCompleted {
        state: TurnState::Completed,
        error: None,
        usage: None,
        cost_usd: None,
    }));
}

fn notice_starting(seen: &Seen, start: &str) -> bool {
    has(
        seen,
        |e| matches!(e, Event::Notice { text } if text.starts_with(start)),
    )
}

/// The composer's mid-turn path to an agent that takes live input (`steer_prompt`): while a
/// switch waits, the message is for the new agent and is never written into the outgoing one,
/// even in dangerous permissive mode; when the switch is cancelled or fails it is held.
#[test]
fn a_message_steered_while_a_switch_waits_never_reaches_the_outgoing_agent() {
    for fails in [false, true] {
        in_loop(|ctx| {
            let s = slow(Driver::Claude);
            s.session.confirm_mode(Mode::Bypass);
            ChatBackend::send_prompt(&*s.session, "the task");
            assert!(s.session.status().running_turn);
            if fails {
                s.session
                    .set_adapter_factory(Rc::new(|_| Err("agy is not ready".into())));
            }
            s.hold_writer(300);
            s.session
                .switch(Driver::Agy, Some("gemini-flash".into()), None);
            ChatBackend::steer_prompt(&*s.session, "for agy");
            assert!(
                !steered(&s.log, "for agy"),
                "steered into Claude (fails: {fails})"
            );
            if !fails {
                ChatBackend::interrupt(&*s.session);
            }
            let start = if fails {
                "The switch to Antigravity failed, so 1 message"
            } else {
                "The switch to Antigravity was cancelled, so 1 message"
            };
            let label = Driver::Agy.info().long_label;
            let start = start.replace("Antigravity", label);
            assert!(
                pump_until(ctx, 5, || notice_starting(&s.seen, &start)),
                "not held (fails: {fails})"
            );
            end_turn(&s.session);
            assert!(!pump_until(ctx, 1, || steered(&s.log, "for agy")
                || prompts(&s.log).iter().any(|p| p.contains("for agy"))));
            assert!(s.session.has_unsent_held(), "a close would lose it");
            s.session.shutdown(false);
        });
    }
}

/// The composer's path for a busy agent without live input (`queue_prompt`): a message queued
/// while a switch waits is the new agent's, and is held if the switch does not happen, not sent
/// to the old agent when its turn ends. The same for a failed busy model switch on one agent.
#[test]
fn a_message_queued_while_a_switch_waits_is_held_if_the_switch_does_not_happen() {
    use agent_kit::store_writer::Shutdown;
    for same_agent in [false, true] {
        in_loop(|ctx| {
            let s = slow(if same_agent {
                Driver::Claude
            } else {
                Driver::Agy
            });
            ChatBackend::send_prompt(&*s.session, "the task");
            assert!(s.session.status().running_turn);
            if same_agent {
                assert_eq!(s.writer.shutdown(Duration::from_secs(5)), Shutdown::Drained);
                s.session.switch(Driver::Claude, Some("opus".into()), None);
            } else {
                s.hold_writer(300);
                s.session.switch(Driver::Claude, Some("opus".into()), None);
            }
            ChatBackend::queue_prompt(&*s.session, "for the new one");
            if same_agent {
                assert!(
                    pump_until(ctx, 5, || failed_notice(&s)),
                    "a failed model switch did not hold it"
                );
            } else {
                ChatBackend::interrupt(&*s.session);
                assert!(s.cancelled_notice(), "a cancelled switch did not hold it");
            }
            end_turn(&s.session);
            assert!(
                !pump_until(ctx, 1, || s.sent_anywhere("for the new one")),
                "the old agent got it when its turn ended (same agent: {same_agent})"
            );
            s.session.shutdown(false);
        });
    }
}

/// A new agent that cannot even be spawned rolls back to the old one at once: that switch
/// failed, so what was written for the new agent is held, not drained to the old one.
#[test]
fn a_switch_whose_new_agent_cannot_spawn_holds_what_was_written_for_it() {
    in_loop(|ctx| {
        let s = slow(Driver::Agy);
        let new_log = s.new_log.clone();
        s.session.set_adapter_factory(Rc::new(move |d| {
            launch_of(FakeAdapter::unspawnable(d, &new_log))
        }));
        s.hold_writer(300);
        s.session.switch(Driver::Claude, Some("opus".into()), None);
        ChatBackend::send_prompt(&*s.session, "for Claude");
        assert!(
            pump_until(ctx, 5, || failed_notice(&s)),
            "the rollback was taken for a switch that went ahead"
        );
        assert_eq!(s.session.status().driver, Driver::Agy, "rolled back");
        assert!(
            !pump_until(ctx, 1, || s.sent_anywhere("for Claude")),
            "drained to agy: {:?}",
            prompts(&s.log)
        );
        s.session.shutdown(false);
    });
}

/// Held messages count as unsent for the close (`has_unsent_held`), and a later message from
/// the user, steered mid-turn, releases them like a send: they then go as usual.
#[test]
fn held_messages_block_a_silent_close_and_a_steer_releases_them() {
    in_loop(|ctx| {
        let s = slow(Driver::Claude);
        ChatBackend::send_prompt(&*s.session, "the task");
        s.hold_writer(300);
        s.session
            .switch(Driver::Agy, Some("gemini-flash".into()), None);
        ChatBackend::queue_prompt(&*s.session, "written for agy");
        assert!(s.session.has_unsent_held(), "unsent while the switch waits");
        ChatBackend::interrupt(&*s.session);
        assert!(s.session.has_unsent_held(), "held after the cancel");
        // The user writes again mid-turn: the hold ends, the steer goes live.
        ChatBackend::steer_prompt(&*s.session, "carry on");
        assert!(steered(&s.log, "carry on"));
        assert!(!s.session.has_unsent_held());
        end_turn(&s.session);
        assert!(
            pump_until(ctx, 5, || s.sent_anywhere("written for agy")),
            "the released message never went"
        );
        s.session.shutdown(false);
    });
}

/// Closing the thread drops a waiting switch without a "cancelled" notice: the close is not
/// news (it asked first about what was held).
#[test]
fn a_close_drops_a_waiting_switch_without_a_notice() {
    in_loop(|ctx| {
        let s = slow(Driver::Agy);
        s.hold_writer(300);
        s.session.switch(Driver::Claude, Some("opus".into()), None);
        ChatBackend::send_prompt(&*s.session, "for Claude");
        ChatBackend::queue_prompt(&*s.session, "and this");
        s.session.shutdown(false);
        pump_until(ctx, 1, || false);
        assert!(
            !notice_starting(&s.seen, "The switch to Claude"),
            "a close posted a cancelled-switch notice"
        );
        assert!(!s.sent_anywhere("for Claude"));
    });
}
