//! Quick replies: a question with choices at the end of a finished Codex or agy reply.
//!
//! The session watches each turn's events ([`ReplyTurn::observe`]) and, when the turn completes,
//! asks [`agent_core::question_extract`] whether the last assistant message ends in one
//! ([`Inner::offer_reply_question`]). An offer is `QuestionRequested { source: Reply }` with the
//! request id `reply:<assistant item id>`. It is delivered to the view but never stored: the
//! store has no copy to replay, so a reopened thread never shows chips again.
//!
//! One debug line per decision, naming the reason and counts only. A reply may hold a secret, so
//! its text is never logged.

use std::collections::HashSet;

use agent_core::adapter::Driver;
use agent_core::event::{
    Envelope, Event, ItemKind, Question, QuestionSource, StreamKind, TurnState,
};
use agent_core::question_extract;
use tracing::debug;

use super::Inner;

/// What one turn said, as far as the reply question needs it.
#[derive(Default)]
pub(super) struct ReplyTurn {
    /// The last assistant message of the turn, and its text so far.
    item: Option<String>,
    text: String,
    /// Nothing but reasoning followed that message: it is the end of the reply, not a remark
    /// before a tool call.
    last_is_reply: bool,
    /// The agent asked through its own question tool this turn.
    native_question: bool,
    /// Sub-agent items: their messages are not the reply.
    nested: HashSet<String>,
}

/// Why no chips were offered: something about the turn, or something about the reply. Nothing
/// is allocated to say it.
#[derive(Clone, Copy)]
pub(super) enum Why {
    Turn(&'static str),
    Reply(question_extract::Skip),
}

impl std::fmt::Display for Why {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Turn(text) => f.write_str(text),
            Self::Reply(skip) => skip.fmt(f),
        }
    }
}

/// Why no chips were offered, or the chips.
pub(super) enum Verdict {
    Offer { item: String, question: Question },
    Skip(Why),
}

impl ReplyTurn {
    pub(super) fn observe(&mut self, env: &Envelope) {
        match &env.event {
            Event::TurnStarted { .. } => *self = Self::default(),
            Event::ItemStarted { kind, parent, .. } => {
                if parent.is_some() {
                    if let Some(id) = &env.item {
                        self.nested.insert(id.clone());
                    }
                    return;
                }
                match kind {
                    ItemKind::AssistantMessage => {
                        self.open(env.item.as_deref(), false);
                    }
                    ItemKind::UserMessage | ItemKind::Reasoning => {}
                    _ => self.last_is_reply = false,
                }
            }
            Event::ContentDelta {
                stream: StreamKind::Assistant,
                text,
            } => {
                if self.open(env.item.as_deref(), true) {
                    self.text.push_str(text);
                }
            }
            Event::ContentSnapshot {
                stream: StreamKind::Assistant,
                text,
            } => {
                if self.open(env.item.as_deref(), true) {
                    self.text.clone_from(text);
                }
            }
            Event::QuestionRequested {
                source: QuestionSource::Agent,
                ..
            } => self.native_question = true,
            _ => {}
        }
    }

    /// Makes `item` the message being written; true when text for it should be taken. A message
    /// that is already the current one keeps its text.
    fn open(&mut self, item: Option<&str>, content: bool) -> bool {
        let Some(item) = item else {
            return content && self.item.is_some();
        };
        if self.nested.contains(item) {
            return false;
        }
        if self.item.as_deref() != Some(item) {
            self.item = Some(item.to_owned());
            self.text.clear();
        }
        self.last_is_reply = true;
        true
    }

    /// Whether to offer chips for a turn that ended in `state`.
    pub(super) fn decide(
        &self,
        driver: Driver,
        state: TurnState,
        setting_on: bool,
        next_prompt_started: bool,
    ) -> Verdict {
        let skip = |why: &'static str| Verdict::Skip(Why::Turn(why));
        if !matches!(driver, Driver::Codex | Driver::Agy) {
            return skip("this agent asks with its own question tool");
        }
        if state != TurnState::Completed {
            return skip("the turn did not complete");
        }
        if !setting_on {
            return skip("quick replies are off");
        }
        if self.native_question {
            return skip("the agent asked a question of its own this turn");
        }
        if next_prompt_started {
            return skip("a queued prompt already started");
        }
        let Some(item) = self.item.as_ref().filter(|_| self.last_is_reply) else {
            return skip("the turn did not end with a reply");
        };
        match question_extract::extract_or_why(&self.text) {
            Ok(question) => Verdict::Offer {
                item: item.clone(),
                question,
            },
            Err(why) => Verdict::Skip(Why::Reply(why)),
        }
    }

    /// The length of the reply read, for the log.
    fn chars(&self) -> usize {
        self.text.chars().count()
    }
}

impl Inner {
    /// The `TurnCompleted` hook: offers quick replies under the reply when it ends in a question
    /// with choices. Called after the event reached the view, so the chips follow the reply.
    pub(super) fn offer_reply_question(&self, env: &Envelope) {
        let Event::TurnCompleted { state, .. } = &env.event else {
            return;
        };
        let driver = self.adapter.borrow().driver();
        let verdict = self.reply_turn.borrow().decide(
            driver,
            *state,
            crate::config::quick_replies_on(),
            self.prompt_in_flight.get(),
        );
        let chars = self.reply_turn.borrow().chars();
        let Verdict::Offer { item, question } = verdict else {
            if let Verdict::Skip(why) = verdict {
                debug!(%why, chars, "quick replies not offered");
            }
            return;
        };
        let offer = Envelope::new(Event::QuestionRequested {
            questions: vec![question],
            source: QuestionSource::Reply,
        })
        .request(format!("reply:{item}"));
        // A chip puts its text in the composer: text the view had to mask is not what the
        // agent said, so offer nothing rather than a changed option.
        if crate::approval_server::masked_for_view(offer.clone()) != offer {
            debug!(chars, "quick replies not offered: an option held a secret");
            return;
        }
        if let Event::QuestionRequested { questions, .. } = &offer.event {
            debug!(
                options = questions.first().map_or(0, |q| q.options.len()),
                chars, "quick replies offered"
            );
        }
        // Delivered, not stored: a reopened thread must not offer them again.
        self.deliver(&offer);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn started(kind: ItemKind, id: &str) -> Envelope {
        Envelope::item_started(id, kind, "", None, None)
    }

    fn text(id: &str, text: &str) -> Envelope {
        Envelope::delta(id, StreamKind::Assistant, text)
    }

    const ASKS: &str = "Which do you prefer?\n\n- Keep it\n- Drop it";

    fn turn(events: &[Envelope]) -> ReplyTurn {
        let mut t = ReplyTurn::default();
        t.observe(&Envelope::new(Event::TurnStarted { model: None }));
        for e in events {
            t.observe(e);
        }
        t
    }

    fn offered(v: &Verdict) -> bool {
        matches!(v, Verdict::Offer { .. })
    }

    #[test]
    fn the_last_assistant_message_is_read_across_deltas() {
        let t = turn(&[
            started(ItemKind::AssistantMessage, "a1"),
            text("a1", "Which do you prefer?\n\n"),
            text("a1", "- Keep it\n- Drop it"),
        ]);
        match t.decide(Driver::Codex, TurnState::Completed, true, false) {
            Verdict::Offer { item, question } => {
                assert_eq!(item, "a1");
                assert_eq!(question.options.len(), 2);
            }
            Verdict::Skip(why) => panic!("expected an offer: {why}"),
        }
    }

    #[test]
    fn a_remark_before_a_tool_call_is_not_the_reply() {
        let t = turn(&[
            started(ItemKind::AssistantMessage, "a1"),
            text("a1", ASKS),
            started(ItemKind::Command, "c1"),
        ]);
        assert!(!offered(&t.decide(
            Driver::Codex,
            TurnState::Completed,
            true,
            false
        )));
        // Reasoning after the message does not make it a remark.
        let t = turn(&[
            started(ItemKind::AssistantMessage, "a1"),
            text("a1", ASKS),
            started(ItemKind::Reasoning, "r1"),
        ]);
        assert!(offered(&t.decide(
            Driver::Agy,
            TurnState::Completed,
            true,
            false
        )));
    }

    #[test]
    fn a_sub_agents_message_is_not_the_reply() {
        let nested =
            Envelope::item_started("sub", ItemKind::AssistantMessage, "", None, Some("tool1"));
        let t = turn(&[
            started(ItemKind::AssistantMessage, "a1"),
            text("a1", "Done."),
            nested,
            text("sub", ASKS),
        ]);
        assert!(!offered(&t.decide(
            Driver::Codex,
            TurnState::Completed,
            true,
            false
        )));
    }

    #[test]
    fn each_condition_alone_withholds_the_offer() {
        let t = turn(&[started(ItemKind::AssistantMessage, "a1"), text("a1", ASKS)]);
        let go = |d, s, on, next| offered(&t.decide(d, s, on, next));
        assert!(go(Driver::Agy, TurnState::Completed, true, false));
        assert!(!go(Driver::Claude, TurnState::Completed, true, false));
        assert!(!go(Driver::Codex, TurnState::Interrupted, true, false));
        assert!(!go(Driver::Codex, TurnState::Failed, true, false));
        assert!(!go(Driver::Codex, TurnState::Completed, false, false));
        assert!(!go(Driver::Codex, TurnState::Completed, true, true));
    }

    #[test]
    fn a_native_question_this_turn_withholds_the_offer_and_a_new_turn_forgets_it() {
        let ask = Envelope::new(Event::QuestionRequested {
            questions: Vec::new(),
            source: QuestionSource::Agent,
        })
        .request("q1");
        let mut t = turn(&[
            ask,
            started(ItemKind::AssistantMessage, "a1"),
            text("a1", ASKS),
        ]);
        assert!(!offered(&t.decide(
            Driver::Codex,
            TurnState::Completed,
            true,
            false
        )));
        t.observe(&Envelope::new(Event::TurnStarted { model: None }));
        t.observe(&started(ItemKind::AssistantMessage, "a2"));
        t.observe(&text("a2", ASKS));
        assert!(offered(&t.decide(
            Driver::Codex,
            TurnState::Completed,
            true,
            false
        )));
    }

    #[test]
    fn a_snapshot_replaces_the_deltas() {
        let snapshot = Envelope::snapshot("a1", StreamKind::Assistant, ASKS);
        let t = turn(&[
            started(ItemKind::AssistantMessage, "a1"),
            text("a1", "partial"),
            snapshot,
        ]);
        assert!(offered(&t.decide(
            Driver::Codex,
            TurnState::Completed,
            true,
            false
        )));
    }

    #[test]
    fn a_turn_with_no_reply_text_offers_nothing() {
        let t = turn(&[]);
        assert!(!offered(&t.decide(
            Driver::Codex,
            TurnState::Completed,
            true,
            false
        )));
    }
}

/// The hook end to end: events through a live session to the view's sink and the store.
#[cfg(test)]
mod session_tests {
    use std::rc::Rc;

    use agent_core::adapter::Adapter;
    use agent_core::event::{Envelope, Event, ItemKind, QuestionSource, StreamKind, TurnState};
    use agent_kit::store::Store;

    use crate::chat::session::tests::{make_sink, Seen};
    use crate::chat::session::ChatSession;
    use crate::testutil::{in_loop, open_session, pump_until};

    const ASKS: &str = "Which do you prefer?\n\n- Keep it\n- Drop it";

    /// A session over an inert process, its sink log and its store.
    fn session(
        ctx: &gtk4::glib::MainContext,
        adapter: Box<dyn Adapter>,
        dir: &std::path::Path,
    ) -> (Rc<ChatSession>, Seen, Rc<Store>, String) {
        let store = Rc::new(Store::open_in_memory().expect("store"));
        let thread = store
            .create_thread(&dir.to_string_lossy(), Some("t"))
            .expect("thread");
        let (sink, seen) = make_sink();
        let open = open_session(
            "/bin/sh",
            vec!["-c".into(), "cat > /dev/null".into()],
            dir.to_string_lossy().into_owned(),
        );
        let session = ChatSession::new(adapter, open, store.clone(), thread.clone(), sink, None);
        assert!(pump_until(ctx, 10, || session.inner.state.borrow().alive));
        (session, seen, store, thread)
    }

    /// One turn: the reply `text` as an assistant message, then the turn ends in `state`.
    fn run_turn(session: &ChatSession, text: &str, state: TurnState, native_question: bool) {
        let emit = |e: Envelope| session.inner.emit(e);
        emit(Envelope::new(Event::TurnStarted { model: None }));
        if native_question {
            emit(
                Envelope::new(Event::QuestionRequested {
                    questions: Vec::new(),
                    source: QuestionSource::Agent,
                })
                .request("q1"),
            );
        }
        emit(Envelope::item_started(
            "a1",
            ItemKind::AssistantMessage,
            "",
            None,
            None,
        ));
        emit(Envelope::delta("a1", StreamKind::Assistant, text));
        emit(Envelope::turn_completed(state, None));
    }

    fn reply_offers(seen: &Seen) -> Vec<Envelope> {
        seen.borrow()
            .iter()
            .filter(|e| {
                matches!(
                    e.event,
                    Event::QuestionRequested {
                        source: QuestionSource::Reply,
                        ..
                    }
                )
            })
            .cloned()
            .collect()
    }

    fn offered(adapter: Box<dyn Adapter>, state: TurnState, native: bool) -> Vec<Envelope> {
        in_loop(|ctx| {
            let tmp = tempfile::tempdir().expect("tmp");
            let (session, seen, _, _) = session(ctx, adapter, tmp.path());
            run_turn(&session, ASKS, state, native);
            reply_offers(&seen)
        })
    }

    #[test]
    fn codex_and_agy_are_offered_the_choices_with_the_reply_request_id() {
        for adapter in [
            Box::new(agent_core::codex::CodexAdapter::new()) as Box<dyn Adapter>,
            Box::new(agent_core::agy::AgyAdapter::new("agy")),
        ] {
            let offers = offered(adapter, TurnState::Completed, false);
            assert_eq!(offers.len(), 1);
            assert_eq!(offers[0].request.as_deref(), Some("reply:a1"));
            let Event::QuestionRequested { questions, .. } = &offers[0].event else {
                unreachable!()
            };
            let labels: Vec<_> = questions[0].options.iter().map(|o| &o.label).collect();
            assert_eq!(labels, ["Keep it", "Drop it"]);
        }
    }

    #[test]
    fn claude_is_never_offered_them() {
        let adapter = Box::new(agent_core::claude::ClaudeAdapter::new());
        assert!(offered(adapter, TurnState::Completed, false).is_empty());
    }

    #[test]
    fn an_interrupted_or_failed_turn_offers_nothing() {
        for state in [TurnState::Interrupted, TurnState::Failed] {
            let adapter = Box::new(agent_core::codex::CodexAdapter::new());
            assert!(offered(adapter, state, false).is_empty(), "{state:?}");
        }
    }

    #[test]
    fn a_native_question_in_the_turn_offers_nothing() {
        let adapter = Box::new(agent_core::codex::CodexAdapter::new());
        assert!(offered(adapter, TurnState::Completed, true).is_empty());
    }

    #[test]
    fn the_setting_off_offers_nothing_and_back_on_offers_again() {
        crate::config::mirror_quick_replies(false);
        let off = offered(
            Box::new(agent_core::codex::CodexAdapter::new()),
            TurnState::Completed,
            false,
        );
        crate::config::mirror_quick_replies(true);
        let on = offered(
            Box::new(agent_core::codex::CodexAdapter::new()),
            TurnState::Completed,
            false,
        );
        assert!(off.is_empty());
        assert_eq!(on.len(), 1);
    }

    #[test]
    fn a_reply_with_no_question_offers_nothing() {
        in_loop(|ctx| {
            let tmp = tempfile::tempdir().expect("tmp");
            let (session, seen, _, _) = session(
                ctx,
                Box::new(agent_core::codex::CodexAdapter::new()),
                tmp.path(),
            );
            run_turn(
                &session,
                "All done.\n\n- a\n- b",
                TurnState::Completed,
                false,
            );
            assert!(reply_offers(&seen).is_empty());
        });
    }

    /// The offer reaches the view but not the store, so a reopened thread never shows it again.
    #[test]
    fn an_offer_is_delivered_but_never_stored() {
        in_loop(|ctx| {
            let tmp = tempfile::tempdir().expect("tmp");
            let (session, seen, store, thread) = session(
                ctx,
                Box::new(agent_core::codex::CodexAdapter::new()),
                tmp.path(),
            );
            run_turn(&session, ASKS, TurnState::Completed, false);
            assert_eq!(reply_offers(&seen).len(), 1);
            let stored = store.events(&thread, None, 1_000).expect("events");
            assert!(stored
                .iter()
                .any(|(_, e)| matches!(e.event, Event::TurnCompleted { .. })));
            assert!(
                !stored
                    .iter()
                    .any(|(_, e)| matches!(e.event, Event::QuestionRequested { .. })),
                "a reply question must not be persisted"
            );
        });
    }

    /// The offer follows the turn's end in the sink, so the chips sit under the reply.
    #[test]
    fn the_offer_follows_the_turn_completed_event() {
        in_loop(|ctx| {
            let tmp = tempfile::tempdir().expect("tmp");
            let (session, seen, _, _) = session(
                ctx,
                Box::new(agent_core::codex::CodexAdapter::new()),
                tmp.path(),
            );
            run_turn(&session, ASKS, TurnState::Completed, false);
            let events = seen.borrow();
            let done = events
                .iter()
                .position(|e| matches!(e.event, Event::TurnCompleted { .. }))
                .expect("turn completed");
            let offer = events
                .iter()
                .position(|e| matches!(e.event, Event::QuestionRequested { .. }))
                .expect("offer");
            assert!(offer > done);
        });
    }
}
