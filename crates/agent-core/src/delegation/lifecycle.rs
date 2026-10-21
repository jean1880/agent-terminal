//! Worker and run reducers, and cancel completion.
//!
//! Invariants: `Closed` is absorbing; a worker goes `Stopping → Stopped` only on an observed
//! exit; `Stopping` plus an interrupted turn with the process alive gives `Waiting` (run
//! `Interrupted`); `RestartReconciled` maps every non-terminal run to `Unknown`, never to
//! `Running`.

use serde::{Deserialize, Serialize};

use super::ids::RunId;
use crate::event::{TurnState, WorkerState};

/// What happened to a worker or its run.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WorkerInput {
    ProcessStarted,
    TurnStarted(RunId),
    /// The run is blocked on an approval or a question.
    InputRequired,
    InputResolved,
    TurnEnded {
        run: RunId,
        state: TurnState,
    },
    /// A cancel was dispatched; `bound` is the target's run bound at acceptance, if any.
    CancelRequested {
        bound: Option<RunId>,
    },
    ProcessExited {
        expected: bool,
    },
    CloseRequested,
    Revoked,
    /// The app restarted and reconciled its stored state.
    RestartReconciled,
}

impl WorkerInput {
    pub fn name(&self) -> &'static str {
        match self {
            WorkerInput::ProcessStarted => "process_started",
            WorkerInput::TurnStarted(_) => "turn_started",
            WorkerInput::InputRequired => "input_required",
            WorkerInput::InputResolved => "input_resolved",
            WorkerInput::TurnEnded { .. } => "turn_ended",
            WorkerInput::CancelRequested { .. } => "cancel_requested",
            WorkerInput::ProcessExited { .. } => "process_exited",
            WorkerInput::CloseRequested => "close_requested",
            WorkerInput::Revoked => "revoked",
            WorkerInput::RestartReconciled => "restart_reconciled",
        }
    }
}

/// Why a reducer refused an input.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LifecycleError {
    /// The input names a run that is not the active one.
    StaleRun,
    /// The worker is closed.
    Closed,
    /// Not a legal transition. For the run, operation and delivery reducers `from` is
    /// `WorkerState::Unknown` and `input` names the reducer and input.
    Illegal {
        from: WorkerState,
        input: &'static str,
    },
}

fn illegal(from: WorkerState, input: &WorkerInput) -> LifecycleError {
    LifecycleError::Illegal {
        from,
        input: input.name(),
    }
}

/// One worker transition. `active` is the worker's current run, if any.
pub fn worker_step(
    state: WorkerState,
    active: Option<&RunId>,
    input: &WorkerInput,
) -> Result<WorkerState, LifecycleError> {
    use WorkerState as S;
    if state == S::Closed {
        return match input {
            WorkerInput::ProcessStarted
            | WorkerInput::TurnStarted(_)
            | WorkerInput::InputRequired
            | WorkerInput::InputResolved
            | WorkerInput::CancelRequested { .. } => Err(LifecycleError::Closed),
            WorkerInput::TurnEnded { .. }
            | WorkerInput::ProcessExited { .. }
            | WorkerInput::CloseRequested
            | WorkerInput::Revoked
            | WorkerInput::RestartReconciled => Ok(S::Closed),
        };
    }
    match input {
        WorkerInput::ProcessStarted => match state {
            S::Stopped | S::Unknown | S::Completed | S::Failed | S::Starting => Ok(S::Starting),
            S::Running | S::Waiting | S::Stopping | S::Closed => Err(illegal(state, input)),
        },
        WorkerInput::TurnStarted(_) => match state {
            S::Starting | S::Waiting | S::Completed | S::Failed if active.is_none() => {
                Ok(S::Running)
            }
            _ => Err(illegal(state, input)),
        },
        WorkerInput::InputRequired => match state {
            S::Running if active.is_some() => Ok(S::Waiting),
            _ => Err(illegal(state, input)),
        },
        WorkerInput::InputResolved => match state {
            S::Waiting if active.is_some() => Ok(S::Running),
            _ => Err(illegal(state, input)),
        },
        WorkerInput::TurnEnded { run, state: turn } => {
            if active != Some(run) {
                return Err(LifecycleError::StaleRun);
            }
            match state {
                S::Running | S::Waiting | S::Stopping => Ok(match turn {
                    TurnState::Completed => S::Completed,
                    TurnState::Failed => S::Failed,
                    // The process is alive (an exit is a separate input): idle, not stopped.
                    TurnState::Interrupted => S::Waiting,
                }),
                _ => Err(illegal(state, input)),
            }
        }
        WorkerInput::CancelRequested { bound } => match bound {
            Some(run) if active != Some(run) => Err(LifecycleError::StaleRun),
            Some(_) => match state {
                S::Running | S::Waiting | S::Starting | S::Stopping => Ok(S::Stopping),
                _ => Err(illegal(state, input)),
            },
            // An idle target: only its descendants are interrupted.
            None => Ok(state),
        },
        WorkerInput::ProcessExited { expected } => match state {
            S::Stopping | S::Stopped => Ok(S::Stopped),
            S::Completed | S::Failed => Ok(S::Stopped),
            _ if *expected => Ok(S::Stopped),
            _ => Ok(S::Failed),
        },
        WorkerInput::CloseRequested => Ok(S::Closed),
        WorkerInput::Revoked => match state {
            S::Starting | S::Running | S::Waiting => Ok(S::Stopping),
            other => Ok(other),
        },
        WorkerInput::RestartReconciled => match state {
            S::Starting | S::Running | S::Waiting | S::Stopping => Ok(S::Unknown),
            other => Ok(other),
        },
    }
}

/// A run's state.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RunState {
    Reserved,
    Starting,
    Running,
    Stopping,
    Completed,
    Interrupted,
    Failed,
    #[serde(other)]
    Unknown,
}

impl RunState {
    pub const ALL: [RunState; 8] = [
        RunState::Reserved,
        RunState::Starting,
        RunState::Running,
        RunState::Stopping,
        RunState::Completed,
        RunState::Interrupted,
        RunState::Failed,
        RunState::Unknown,
    ];

    /// Completed, interrupted or failed. `Unknown` (after a restart) is not terminal.
    pub fn is_terminal(self) -> bool {
        match self {
            RunState::Completed | RunState::Interrupted | RunState::Failed => true,
            RunState::Reserved
            | RunState::Starting
            | RunState::Running
            | RunState::Stopping
            | RunState::Unknown => false,
        }
    }
}

fn run_illegal(input: &WorkerInput) -> LifecycleError {
    LifecycleError::Illegal {
        from: WorkerState::Unknown,
        input: input.name(),
    }
}

/// One run transition.
pub fn run_step(state: RunState, input: &WorkerInput) -> Result<RunState, LifecycleError> {
    use RunState as R;
    if state.is_terminal() {
        // A bound run that has already ended is a no-op for a cancel, a close or a revocation.
        return match input {
            WorkerInput::CancelRequested { .. }
            | WorkerInput::CloseRequested
            | WorkerInput::Revoked
            | WorkerInput::ProcessExited { .. }
            | WorkerInput::RestartReconciled => Ok(state),
            _ => Err(run_illegal(input)),
        };
    }
    match input {
        WorkerInput::RestartReconciled => Ok(R::Unknown),
        _ if state == R::Unknown => Err(run_illegal(input)),
        WorkerInput::ProcessStarted => match state {
            R::Reserved => Ok(R::Starting),
            _ => Err(run_illegal(input)),
        },
        WorkerInput::TurnStarted(_) => match state {
            R::Reserved | R::Starting => Ok(R::Running),
            _ => Err(run_illegal(input)),
        },
        WorkerInput::InputRequired | WorkerInput::InputResolved => match state {
            R::Running | R::Stopping => Ok(state),
            _ => Err(run_illegal(input)),
        },
        WorkerInput::TurnEnded { state: turn, .. } => match state {
            R::Starting | R::Running | R::Stopping => Ok(match turn {
                TurnState::Completed => R::Completed,
                TurnState::Interrupted => R::Interrupted,
                TurnState::Failed => R::Failed,
            }),
            _ => Err(run_illegal(input)),
        },
        WorkerInput::CancelRequested { .. }
        | WorkerInput::CloseRequested
        | WorkerInput::Revoked => match state {
            // Never started: nothing to interrupt.
            R::Reserved => Ok(R::Interrupted),
            _ => Ok(R::Stopping),
        },
        WorkerInput::ProcessExited { expected } => match state {
            R::Stopping => Ok(R::Interrupted),
            _ if *expected => Ok(R::Interrupted),
            _ => Ok(R::Failed),
        },
    }
}

/// A cancel operation is `Confirmed` when every run bound at acceptance is terminal.
pub fn cancel_complete(bound: &[(RunId, RunState)]) -> bool {
    bound.iter().all(|(_, s)| s.is_terminal())
}

/// The bound set: the target's current run (if any) plus every active descendant run. Empty
/// means `idle`, with no dispatch; an idle target with active descendants is non-empty.
pub fn cancel_bound_set(target_run: Option<&RunId>, active_descendants: &[RunId]) -> Vec<RunId> {
    let mut out: Vec<RunId> = target_run.cloned().into_iter().collect();
    for r in active_descendants {
        if !out.contains(r) {
            out.push(r.clone());
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use WorkerState as S;

    fn r(s: &str) -> RunId {
        RunId::parse(&format!("r_{s}")).expect("valid")
    }

    const WORKER_STATES: [WorkerState; 9] = [
        S::Starting,
        S::Running,
        S::Waiting,
        S::Completed,
        S::Failed,
        S::Stopped,
        S::Closed,
        S::Stopping,
        S::Unknown,
    ];

    fn inputs(run: &RunId) -> Vec<WorkerInput> {
        vec![
            WorkerInput::ProcessStarted,
            WorkerInput::TurnStarted(run.clone()),
            WorkerInput::InputRequired,
            WorkerInput::InputResolved,
            WorkerInput::TurnEnded {
                run: run.clone(),
                state: TurnState::Completed,
            },
            WorkerInput::TurnEnded {
                run: run.clone(),
                state: TurnState::Interrupted,
            },
            WorkerInput::CancelRequested {
                bound: Some(run.clone()),
            },
            WorkerInput::CancelRequested { bound: None },
            WorkerInput::ProcessExited { expected: true },
            WorkerInput::ProcessExited { expected: false },
            WorkerInput::CloseRequested,
            WorkerInput::Revoked,
            WorkerInput::RestartReconciled,
        ]
    }

    #[test]
    fn a_worker_runs_a_turn_end_to_end() {
        let run = r("aaaaaaaa");
        let s = worker_step(S::Unknown, None, &WorkerInput::ProcessStarted).expect("start");
        assert_eq!(s, S::Starting);
        let s = worker_step(s, None, &WorkerInput::TurnStarted(run.clone())).expect("turn");
        assert_eq!(s, S::Running);
        let s = worker_step(s, Some(&run), &WorkerInput::InputRequired).expect("ask");
        assert_eq!(s, S::Waiting);
        let s = worker_step(s, Some(&run), &WorkerInput::InputResolved).expect("answer");
        assert_eq!(s, S::Running);
        let end = WorkerInput::TurnEnded {
            run: run.clone(),
            state: TurnState::Completed,
        };
        assert_eq!(worker_step(s, Some(&run), &end), Ok(S::Completed));
        let failed = WorkerInput::TurnEnded {
            run: run.clone(),
            state: TurnState::Failed,
        };
        assert_eq!(worker_step(s, Some(&run), &failed), Ok(S::Failed));
        // A completed worker takes its next turn.
        assert_eq!(
            worker_step(S::Completed, None, &WorkerInput::TurnStarted(r("bbbbbbbb"))),
            Ok(S::Running)
        );
        assert_eq!(
            worker_step(
                S::Running,
                Some(&run),
                &WorkerInput::ProcessExited { expected: false }
            ),
            Ok(S::Failed)
        );
        assert_eq!(
            worker_step(S::Waiting, None, &WorkerInput::Revoked),
            Ok(S::Stopping)
        );
    }

    #[test]
    fn illegal_worker_transitions_are_refused() {
        let run = r("aaaaaaaa");
        assert!(matches!(
            worker_step(S::Running, Some(&run), &WorkerInput::ProcessStarted),
            Err(LifecycleError::Illegal {
                from: S::Running,
                ..
            })
        ));
        assert!(matches!(
            worker_step(
                S::Running,
                Some(&run),
                &WorkerInput::TurnStarted(r("bbbbbbbb"))
            ),
            Err(LifecycleError::Illegal { .. })
        ));
        assert!(matches!(
            worker_step(S::Stopped, None, &WorkerInput::InputRequired),
            Err(LifecycleError::Illegal { .. })
        ));
        assert!(matches!(
            worker_step(S::Unknown, None, &WorkerInput::TurnStarted(run.clone())),
            Err(LifecycleError::Illegal { .. })
        ));
        assert!(matches!(
            worker_step(
                S::Completed,
                Some(&run),
                &WorkerInput::TurnEnded {
                    run: run.clone(),
                    state: TurnState::Completed
                }
            ),
            Err(LifecycleError::Illegal { .. })
        ));
    }

    #[test]
    fn stale_runs_are_refused() {
        let run = r("aaaaaaaa");
        let other = r("bbbbbbbb");
        assert_eq!(
            worker_step(
                S::Running,
                Some(&run),
                &WorkerInput::CancelRequested {
                    bound: Some(other.clone())
                }
            ),
            Err(LifecycleError::StaleRun)
        );
        assert_eq!(
            worker_step(
                S::Running,
                Some(&run),
                &WorkerInput::TurnEnded {
                    run: other,
                    state: TurnState::Completed
                }
            ),
            Err(LifecycleError::StaleRun)
        );
        assert_eq!(
            worker_step(
                S::Waiting,
                None,
                &WorkerInput::CancelRequested { bound: Some(run) }
            ),
            Err(LifecycleError::StaleRun)
        );
    }

    #[test]
    fn closed_is_absorbing() {
        let run = r("aaaaaaaa");
        for input in inputs(&run) {
            match worker_step(S::Closed, Some(&run), &input) {
                Ok(s) => assert_eq!(s, S::Closed, "{input:?}"),
                Err(e) => assert_eq!(e, LifecycleError::Closed, "{input:?}"),
            }
        }
        for s in WORKER_STATES {
            assert_eq!(
                worker_step(s, None, &WorkerInput::CloseRequested),
                Ok(S::Closed)
            );
        }
    }

    #[test]
    fn restart_never_yields_running() {
        for s in WORKER_STATES {
            let next = worker_step(s, None, &WorkerInput::RestartReconciled).expect("total");
            assert_ne!(next, S::Running);
        }
        for s in RunState::ALL {
            let next = run_step(s, &WorkerInput::RestartReconciled).expect("total");
            assert_ne!(next, RunState::Running);
            if !s.is_terminal() {
                assert_eq!(next, RunState::Unknown);
            } else {
                assert_eq!(next, s);
            }
        }
    }

    #[test]
    fn stopping_ends_only_on_an_observed_exit_and_interruption_keeps_the_process() {
        let run = r("aaaaaaaa");
        let s = worker_step(
            S::Running,
            Some(&run),
            &WorkerInput::CancelRequested {
                bound: Some(run.clone()),
            },
        )
        .expect("cancel");
        assert_eq!(s, S::Stopping);
        for input in inputs(&run) {
            if let Ok(next) = worker_step(S::Stopping, Some(&run), &input) {
                if next == S::Stopped {
                    assert!(
                        matches!(input, WorkerInput::ProcessExited { .. }),
                        "{input:?}"
                    );
                }
            }
        }
        let interrupted = WorkerInput::TurnEnded {
            run: run.clone(),
            state: TurnState::Interrupted,
        };
        assert_eq!(
            worker_step(S::Stopping, Some(&run), &interrupted),
            Ok(S::Waiting)
        );
        assert_eq!(
            run_step(RunState::Stopping, &interrupted),
            Ok(RunState::Interrupted)
        );
        assert_eq!(
            worker_step(
                S::Stopping,
                None,
                &WorkerInput::ProcessExited { expected: false }
            ),
            Ok(S::Stopped)
        );
    }

    #[test]
    fn run_transitions_cover_the_legal_paths() {
        let run = r("aaaaaaaa");
        let mut s = RunState::Reserved;
        s = run_step(s, &WorkerInput::ProcessStarted).expect("start");
        assert_eq!(s, RunState::Starting);
        s = run_step(s, &WorkerInput::TurnStarted(run.clone())).expect("turn");
        assert_eq!(s, RunState::Running);
        assert_eq!(
            run_step(s, &WorkerInput::InputRequired),
            Ok(RunState::Running)
        );
        assert_eq!(
            run_step(s, &WorkerInput::CancelRequested { bound: None }),
            Ok(RunState::Stopping)
        );
        assert_eq!(
            run_step(
                RunState::Reserved,
                &WorkerInput::CancelRequested { bound: None }
            ),
            Ok(RunState::Interrupted)
        );
        assert_eq!(
            run_step(
                s,
                &WorkerInput::TurnEnded {
                    run: run.clone(),
                    state: TurnState::Failed
                }
            ),
            Ok(RunState::Failed)
        );
        assert_eq!(
            run_step(s, &WorkerInput::ProcessExited { expected: false }),
            Ok(RunState::Failed)
        );
        // A cancel of a run that already ended is a no-op; a new turn on it is refused.
        assert_eq!(
            run_step(
                RunState::Completed,
                &WorkerInput::CancelRequested { bound: None }
            ),
            Ok(RunState::Completed)
        );
        assert!(run_step(RunState::Completed, &WorkerInput::TurnStarted(run.clone())).is_err());
        assert!(run_step(RunState::Running, &WorkerInput::ProcessStarted).is_err());
        assert!(run_step(RunState::Unknown, &WorkerInput::TurnStarted(run)).is_err());
    }

    #[test]
    fn run_state_all_is_exhaustive_and_round_trips() {
        use RunState::*;
        fn next(s: RunState) -> Option<RunState> {
            match s {
                Reserved => Some(Starting),
                Starting => Some(Running),
                Running => Some(Stopping),
                Stopping => Some(Completed),
                Completed => Some(Interrupted),
                Interrupted => Some(Failed),
                Failed => Some(Unknown),
                Unknown => None,
            }
        }
        let mut walked = vec![Reserved];
        while let Some(s) = next(*walked.last().expect("non-empty")) {
            walked.push(s);
        }
        assert_eq!(walked, RunState::ALL);
        for s in RunState::ALL {
            let json = serde_json::to_value(s).expect("ser");
            assert_eq!(serde_json::from_value::<RunState>(json).expect("de"), s);
        }
    }

    #[test]
    fn cancel_completes_only_when_every_bound_run_is_terminal() {
        assert!(!cancel_complete(&[
            (r("aaaaaaaa"), RunState::Interrupted),
            (r("bbbbbbbb"), RunState::Stopping)
        ]));
        assert!(cancel_complete(&[
            (r("aaaaaaaa"), RunState::Interrupted),
            (r("bbbbbbbb"), RunState::Completed)
        ]));
        assert!(!cancel_complete(&[(r("aaaaaaaa"), RunState::Unknown)]));
        assert!(cancel_complete(&[]));
    }

    #[test]
    fn cancel_bound_set_includes_active_descendants() {
        let d = [r("dddddddd"), r("eeeeeeee")];
        assert_eq!(cancel_bound_set(None, &d), d.to_vec());
        assert!(cancel_bound_set(None, &[]).is_empty());
        let t = r("tttttttt");
        assert_eq!(
            cancel_bound_set(Some(&t), &d),
            vec![t.clone(), d[0].clone(), d[1].clone()]
        );
    }
}
