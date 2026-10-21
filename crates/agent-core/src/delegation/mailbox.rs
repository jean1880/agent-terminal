//! Messages between workers (`cross_agent_send` and ACKs,
//! `this_run` delivery).

use std::collections::VecDeque;

use serde::{Deserialize, Serialize};

use super::error::ErrorCode;
use super::ids::{MessageId, RunId, WorkerId};
use super::lifecycle::LifecycleError;
use super::route::DelegationCaps;
use super::wire::{Delivery, ThisRunSupport};
use crate::adapter::Driver;
use crate::event::WorkerState;

/// A message's kind.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MessageKind {
    Instruction,
    Report,
    Question,
    Reply,
}

impl MessageKind {
    pub const ALL: [MessageKind; 4] = [
        MessageKind::Instruction,
        MessageKind::Report,
        MessageKind::Question,
        MessageKind::Reply,
    ];
}

/// Which way a message travels.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Direction {
    ParentToChild,
    ChildToParent,
}

/// Parent→child: `instruction`, `reply`. Child→parent: `report`, `question`, `reply`.
pub fn kind_allowed(dir: Direction, kind: MessageKind) -> bool {
    match (dir, kind) {
        (Direction::ParentToChild, MessageKind::Instruction | MessageKind::Reply) => true,
        (Direction::ParentToChild, MessageKind::Report | MessageKind::Question) => false,
        (
            Direction::ChildToParent,
            MessageKind::Report | MessageKind::Question | MessageKind::Reply,
        ) => true,
        (Direction::ChildToParent, MessageKind::Instruction) => false,
    }
}

/// A message's delivery state.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DeliveryState {
    Queued,
    QueuedNextTurn,
    DispatchReserved,
    Submitted,
    ProviderConfirmed,
    /// Consumed in a run other than the one targeted at acceptance (Claude best-effort
    /// `this_run`). A delivery fact, never an error.
    Misdelivered,
    Failed,
    Cancelled,
    #[serde(other)]
    Unknown,
}

impl DeliveryState {
    pub const ALL: [DeliveryState; 9] = [
        DeliveryState::Queued,
        DeliveryState::QueuedNextTurn,
        DeliveryState::DispatchReserved,
        DeliveryState::Submitted,
        DeliveryState::ProviderConfirmed,
        DeliveryState::Misdelivered,
        DeliveryState::Failed,
        DeliveryState::Cancelled,
        DeliveryState::Unknown,
    ];

    pub fn is_terminal(self) -> bool {
        match self {
            DeliveryState::ProviderConfirmed
            | DeliveryState::Misdelivered
            | DeliveryState::Failed
            | DeliveryState::Cancelled => true,
            DeliveryState::Queued
            | DeliveryState::QueuedNextTurn
            | DeliveryState::DispatchReserved
            | DeliveryState::Submitted
            | DeliveryState::Unknown => false,
        }
    }
}

/// One delivery transition. Nothing leaves a terminal state.
pub fn delivery_step(
    from: DeliveryState,
    to: DeliveryState,
) -> Result<DeliveryState, LifecycleError> {
    use DeliveryState as D;
    let ok = match from {
        D::Queued => matches!(
            to,
            D::QueuedNextTurn | D::DispatchReserved | D::Cancelled | D::Failed
        ),
        D::QueuedNextTurn => matches!(to, D::DispatchReserved | D::Cancelled | D::Failed),
        D::DispatchReserved => matches!(
            to,
            D::Submitted | D::ProviderConfirmed | D::QueuedNextTurn | D::Failed | D::Unknown
        ),
        D::Submitted => matches!(
            to,
            D::ProviderConfirmed | D::Misdelivered | D::Failed | D::Unknown
        ),
        D::ProviderConfirmed | D::Misdelivered | D::Failed | D::Cancelled | D::Unknown => false,
    };
    if ok {
        Ok(to)
    } else {
        Err(LifecycleError::Illegal {
            from: WorkerState::Unknown,
            input: "delivery_step",
        })
    }
}

/// What the supervisor knows about a send's recipient at acceptance (and again at dispatch).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TargetRuntime {
    pub driver: Driver,
    /// The adapter takes a message while a turn runs (`caps::Capabilities::live_input`).
    pub live_input: bool,
    pub active_run: Option<RunId>,
    pub process_alive: bool,
    pub closed: bool,
    /// agy: a message was written and its `result` has not arrived; write only after it.
    pub awaiting_result: bool,
}

/// How a send is delivered.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DeliveryPlan {
    /// Codex strict: `turn/steer` pinned to the run.
    SteerNow(RunId),
    /// Claude: a live user line. `Some` for best-effort `this_run`, `None` for `auto`.
    WriteLive { target: Option<RunId> },
    /// No run is live: start a turn with it.
    StartTurn,
    /// Deliver after the current run (or result) ends.
    QueueNextTurn,
}

/// Plans a send. `this_run` follows `DelegationCaps::of(t.driver).this_run`: strict steers,
/// best effort writes live with the run as target, unsupported is `UnsupportedDelivery`. No
/// live run, or one that does not match `expected`, is `StaleRun`. At dispatch the app
/// rechecks liveness; a dead target run makes the message `failed` (`stale_run`).
pub fn plan_delivery(
    t: &TargetRuntime,
    d: Delivery,
    expected: Option<&RunId>,
) -> Result<DeliveryPlan, ErrorCode> {
    if t.closed {
        return Err(ErrorCode::WorkerClosed);
    }
    let live = t.active_run.as_ref().filter(|_| t.process_alive);
    if let Some(e) = expected {
        if live != Some(e) {
            return Err(ErrorCode::StaleRun);
        }
    }
    let this_run = DelegationCaps::of(t.driver).this_run;
    match d {
        Delivery::ThisRun => {
            if this_run == ThisRunSupport::Unsupported {
                return Err(ErrorCode::UnsupportedDelivery);
            }
            let Some(run) = live else {
                return Err(ErrorCode::StaleRun);
            };
            match this_run {
                ThisRunSupport::Strict => Ok(DeliveryPlan::SteerNow(run.clone())),
                ThisRunSupport::BestEffort => Ok(DeliveryPlan::WriteLive {
                    target: Some(run.clone()),
                }),
                ThisRunSupport::Unsupported => Err(ErrorCode::UnsupportedDelivery),
            }
        }
        Delivery::NextTurn => Ok(idle_or_queue(t, live)),
        Delivery::Auto => match live {
            Some(run) if t.live_input => match this_run {
                ThisRunSupport::Strict => Ok(DeliveryPlan::SteerNow(run.clone())),
                ThisRunSupport::BestEffort => Ok(DeliveryPlan::WriteLive { target: None }),
                ThisRunSupport::Unsupported => Ok(DeliveryPlan::QueueNextTurn),
            },
            _ => Ok(idle_or_queue(t, live)),
        },
    }
}

fn idle_or_queue(t: &TargetRuntime, live: Option<&RunId>) -> DeliveryPlan {
    if live.is_some() || t.awaiting_result {
        DeliveryPlan::QueueNextTurn
    } else {
        DeliveryPlan::StartTurn
    }
}

/// A refused steer: `auto` falls back to the next turn, `this_run` is `StaleRun` and nothing
/// is delivered.
pub fn on_steer_refused(d: Delivery) -> Result<DeliveryState, ErrorCode> {
    match d {
        Delivery::Auto | Delivery::NextTurn => Ok(DeliveryState::QueuedNextTurn),
        Delivery::ThisRun => Err(ErrorCode::StaleRun),
    }
}

/// Most lines awaiting their echo; the oldest is dropped past it.
pub const ECHO_QUEUE_CAP: usize = 256;

/// 128-bit FNV-1a, for text comparison only (not a secret digest).
pub(crate) fn fnv1a_128(bytes: &[u8]) -> u128 {
    const OFFSET: u128 = 0x6c62_272e_07bb_0142_62b8_2175_6295_c58d;
    const PRIME: u128 = 0x0000_0000_0100_0000_0000_0000_0000_013b;
    let mut h = OFFSET;
    for &b in bytes {
        h ^= u128::from(b);
        h = h.wrapping_mul(PRIME);
    }
    h
}

/// Claude echo correlation without uuids: a FIFO of written lines (scrubbed text) against
/// `isReplay` echoes; a mismatch makes the delivery `unknown`.
#[derive(Debug, Clone, Default)]
pub struct EchoQueue {
    pending: VecDeque<(MessageId, Option<RunId>, u128, usize)>,
}

/// What an echo means for the front of the queue.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EchoMatch {
    /// Target `None` (`auto`), or target == the echo's run.
    Confirmed { message: MessageId, run: RunId },
    Misdelivered {
        message: MessageId,
        target: RunId,
        actual: RunId,
    },
    /// Out of order or altered text: the front message's delivery becomes `unknown`.
    Mismatch { message: MessageId },
    /// Nothing was written.
    Unexpected,
}

impl EchoQueue {
    pub fn written(&mut self, m: MessageId, target: Option<RunId>, text: &str) {
        if self.pending.len() >= ECHO_QUEUE_CAP {
            self.pending.pop_front();
        }
        self.pending
            .push_back((m, target, fnv1a_128(text.as_bytes()), text.len()));
    }

    pub fn echoed(&mut self, text: &str, run: &RunId) -> EchoMatch {
        let Some((message, target, digest, len)) = self.pending.pop_front() else {
            return EchoMatch::Unexpected;
        };
        if len != text.len() || digest != fnv1a_128(text.as_bytes()) {
            return EchoMatch::Mismatch { message };
        }
        match target {
            Some(t) if &t != run => EchoMatch::Misdelivered {
                message,
                target: t,
                actual: run.clone(),
            },
            _ => EchoMatch::Confirmed {
                message,
                run: run.clone(),
            },
        }
    }

    pub fn len(&self) -> usize {
        self.pending.len()
    }

    pub fn is_empty(&self) -> bool {
        self.pending.is_empty()
    }
}

/// Whether `caller` may acknowledge a message for `recipient`. `Ok(false)` = already
/// acknowledged (idempotent); `NotFound` if the caller is not the recipient;
/// `InvalidArgument` (`details.reason: "not_presented"`) if no page returned it yet.
pub fn ack_eligible(
    recipient: &WorkerId,
    caller: &WorkerId,
    presented: bool,
    acknowledged: bool,
) -> Result<bool, ErrorCode> {
    if recipient != caller {
        return Err(ErrorCode::NotFound);
    }
    if acknowledged {
        return Ok(false);
    }
    if !presented {
        return Err(ErrorCode::InvalidArgument);
    }
    Ok(true)
}

/// Automatic root continuations: one reserved at a time, at most [`ContinuationBudget::MAX`]
/// per active root task.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ContinuationBudget {
    pub used: u8,
    pub reserved: bool,
}

impl ContinuationBudget {
    pub const MAX: u8 = 3;

    pub fn reserve(&mut self) -> bool {
        if self.reserved || self.used >= Self::MAX {
            return false;
        }
        self.reserved = true;
        true
    }

    /// The reserved continuation was dispatched: it counts.
    pub fn commit(&mut self) {
        if self.reserved {
            self.reserved = false;
            self.used = self.used.saturating_add(1);
        }
    }

    /// The reservation was dropped: nothing is consumed.
    pub fn release(&mut self) {
        self.reserved = false;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn r(s: &str) -> RunId {
        RunId::parse(&format!("r_{s}")).expect("valid")
    }

    fn m(s: &str) -> MessageId {
        MessageId::parse(&format!("m_{s}")).expect("valid")
    }

    fn target(driver: Driver, run: Option<RunId>) -> TargetRuntime {
        TargetRuntime {
            driver,
            live_input: driver != Driver::Agy,
            active_run: run,
            process_alive: true,
            closed: false,
            awaiting_result: false,
        }
    }

    #[test]
    fn plan_delivery_covers_each_provider_and_case() {
        let run = r("aaaaaaaa");
        let codex = target(Driver::Codex, Some(run.clone()));
        let claude = target(Driver::Claude, Some(run.clone()));
        let agy = target(Driver::Agy, Some(run.clone()));
        assert_eq!(
            plan_delivery(&codex, Delivery::ThisRun, None),
            Ok(DeliveryPlan::SteerNow(run.clone()))
        );
        assert_eq!(
            plan_delivery(&claude, Delivery::ThisRun, Some(&run)),
            Ok(DeliveryPlan::WriteLive {
                target: Some(run.clone())
            })
        );
        assert_eq!(
            plan_delivery(&claude, Delivery::Auto, None),
            Ok(DeliveryPlan::WriteLive { target: None })
        );
        assert_eq!(
            plan_delivery(&codex, Delivery::Auto, None),
            Ok(DeliveryPlan::SteerNow(run.clone()))
        );
        assert_eq!(
            plan_delivery(&agy, Delivery::ThisRun, None),
            Err(ErrorCode::UnsupportedDelivery)
        );
        assert_eq!(
            plan_delivery(&agy, Delivery::Auto, None),
            Ok(DeliveryPlan::QueueNextTurn)
        );
        assert_eq!(
            plan_delivery(&codex, Delivery::NextTurn, None),
            Ok(DeliveryPlan::QueueNextTurn)
        );
        // No live run.
        let idle = target(Driver::Codex, None);
        assert_eq!(
            plan_delivery(&idle, Delivery::ThisRun, None),
            Err(ErrorCode::StaleRun)
        );
        assert_eq!(
            plan_delivery(&idle, Delivery::Auto, None),
            Ok(DeliveryPlan::StartTurn)
        );
        assert_eq!(
            plan_delivery(&idle, Delivery::NextTurn, None),
            Ok(DeliveryPlan::StartTurn)
        );
        let mut dead = claude.clone();
        dead.process_alive = false;
        assert_eq!(
            plan_delivery(&dead, Delivery::ThisRun, None),
            Err(ErrorCode::StaleRun)
        );
        // A stale expected run.
        assert_eq!(
            plan_delivery(&codex, Delivery::ThisRun, Some(&r("bbbbbbbb"))),
            Err(ErrorCode::StaleRun)
        );
        // A closed target.
        let mut closed = codex.clone();
        closed.closed = true;
        assert_eq!(
            plan_delivery(&closed, Delivery::Auto, None),
            Err(ErrorCode::WorkerClosed)
        );
        // agy that was written to and has no result yet queues.
        let mut waiting = target(Driver::Agy, None);
        waiting.awaiting_result = true;
        assert_eq!(
            plan_delivery(&waiting, Delivery::Auto, None),
            Ok(DeliveryPlan::QueueNextTurn)
        );
        assert_eq!(
            plan_delivery(&waiting, Delivery::NextTurn, None),
            Ok(DeliveryPlan::QueueNextTurn)
        );
    }

    #[test]
    fn steer_refusals_fall_back_only_for_auto() {
        assert_eq!(
            on_steer_refused(Delivery::Auto),
            Ok(DeliveryState::QueuedNextTurn)
        );
        assert_eq!(
            on_steer_refused(Delivery::ThisRun),
            Err(ErrorCode::StaleRun)
        );
    }

    #[test]
    fn delivery_state_all_is_exhaustive_and_misdelivered_is_terminal() {
        use DeliveryState::*;
        fn next(s: DeliveryState) -> Option<DeliveryState> {
            match s {
                Queued => Some(QueuedNextTurn),
                QueuedNextTurn => Some(DispatchReserved),
                DispatchReserved => Some(Submitted),
                Submitted => Some(ProviderConfirmed),
                ProviderConfirmed => Some(Misdelivered),
                Misdelivered => Some(Failed),
                Failed => Some(Cancelled),
                Cancelled => Some(Unknown),
                Unknown => None,
            }
        }
        let mut walked = vec![Queued];
        while let Some(s) = next(*walked.last().expect("non-empty")) {
            walked.push(s);
        }
        assert_eq!(walked, DeliveryState::ALL);
        assert_eq!(DeliveryState::ALL.len(), 9);
        let json = serde_json::to_value(Misdelivered).expect("ser");
        assert_eq!(json, serde_json::json!("misdelivered"));
        assert_eq!(
            serde_json::from_value::<DeliveryState>(json).expect("de"),
            Misdelivered
        );
        assert!(Misdelivered.is_terminal());
        assert_eq!(delivery_step(Submitted, Misdelivered), Ok(Misdelivered));
        for from in DeliveryState::ALL.into_iter().filter(|s| s.is_terminal()) {
            for to in DeliveryState::ALL {
                assert!(delivery_step(from, to).is_err(), "{from:?} -> {to:?}");
            }
        }
        assert!(delivery_step(Queued, Submitted).is_err());
    }

    #[test]
    fn echo_queue_correlates_by_order_and_text() {
        let a = r("aaaaaaaa");
        let b = r("bbbbbbbb");
        let mut q = EchoQueue::default();
        q.written(m("11111111"), Some(a.clone()), "hello");
        q.written(m("22222222"), Some(a.clone()), "second");
        q.written(m("33333333"), None, "auto");
        assert_eq!(
            q.echoed("hello", &a),
            EchoMatch::Confirmed {
                message: m("11111111"),
                run: a.clone()
            }
        );
        assert_eq!(
            q.echoed("second", &b),
            EchoMatch::Misdelivered {
                message: m("22222222"),
                target: a.clone(),
                actual: b.clone()
            }
        );
        assert_eq!(
            q.echoed("auto", &b),
            EchoMatch::Confirmed {
                message: m("33333333"),
                run: b.clone()
            }
        );
        assert_eq!(q.echoed("anything", &b), EchoMatch::Unexpected);
        q.written(m("44444444"), None, "first");
        q.written(m("55555555"), None, "then");
        assert_eq!(
            q.echoed("then", &a),
            EchoMatch::Mismatch {
                message: m("44444444")
            }
        );
        q.written(m("66666666"), None, "text");
        assert_eq!(
            q.echoed("text!", &a),
            EchoMatch::Mismatch {
                message: m("55555555")
            }
        );
    }

    #[test]
    fn ack_eligibility() {
        let me = WorkerId::parse("w_meeeeeee").expect("valid");
        let other = WorkerId::parse("w_otherrrr").expect("valid");
        assert_eq!(
            ack_eligible(&me, &other, true, false),
            Err(ErrorCode::NotFound)
        );
        assert_eq!(
            ack_eligible(&me, &me, false, false),
            Err(ErrorCode::InvalidArgument)
        );
        assert_eq!(ack_eligible(&me, &me, true, true), Ok(false));
        assert_eq!(ack_eligible(&me, &me, true, false), Ok(true));
    }

    #[test]
    fn continuation_budget_reserves_one_at_a_time_up_to_three() {
        let mut b = ContinuationBudget::default();
        assert!(b.reserve());
        assert!(!b.reserve());
        b.release();
        assert_eq!(b.used, 0);
        for _ in 0..3 {
            assert!(b.reserve());
            b.commit();
        }
        assert_eq!(b.used, 3);
        assert!(!b.reserve());
    }

    /// `MessageKind::ALL` is hand-maintained: `next` has no wildcard, so a new kind does not
    /// compile until it is chained in, and the walk must equal `ALL`.
    #[test]
    fn message_kind_all_lists_every_kind_once_in_order() {
        use MessageKind::*;
        fn next(k: MessageKind) -> Option<MessageKind> {
            match k {
                Instruction => Some(Report),
                Report => Some(Question),
                Question => Some(Reply),
                Reply => None,
            }
        }
        let mut walked = vec![Instruction];
        while let Some(k) = next(*walked.last().expect("non-empty")) {
            walked.push(k);
        }
        assert_eq!(walked, MessageKind::ALL);
    }

    #[test]
    fn kinds_follow_the_direction_rules() {
        let allowed: Vec<(Direction, MessageKind)> =
            [Direction::ParentToChild, Direction::ChildToParent]
                .into_iter()
                .flat_map(|d| MessageKind::ALL.into_iter().map(move |k| (d, k)))
                .filter(|(d, k)| kind_allowed(*d, *k))
                .collect();
        assert_eq!(
            allowed,
            vec![
                (Direction::ParentToChild, MessageKind::Instruction),
                (Direction::ParentToChild, MessageKind::Reply),
                (Direction::ChildToParent, MessageKind::Report),
                (Direction::ChildToParent, MessageKind::Question),
                (Direction::ChildToParent, MessageKind::Reply),
            ]
        );
    }
}
