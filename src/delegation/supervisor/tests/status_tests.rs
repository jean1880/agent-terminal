//! A worker's reported status and run agree with what the supervisor can do with it: a lost
//! bridge says why, a task that cannot be dispatched ends its run everywhere, a task that never
//! starts is a stall with a reason, and paging errors name what went wrong without using up a
//! key.
use super::*;
use agent_core::delegation::{Cursor, MessageId};

/// The stored dispatch state of an operation, once it has left `accepted`/`dispatch_reserved`.
async fn settled_dispatch(sup: &Rc<Supervisor>, operation: &str) -> (String, Option<String>) {
    let mut out = None;
    pump_until(|| {
        let row = sup
            .services
            .handle
            .reader()
            .delegation_operation(operation)
            .unwrap()
            .unwrap();
        let settled = !matches!(
            row.dispatch_state.as_str(),
            "accepted" | "dispatch_reserved"
        );
        out = Some((row.dispatch_state, row.error_json));
        settled
    })
    .await;
    out.unwrap()
}

/// a bridge connection that closes leaves the worker `rebind_required` with a reason, in
/// memory and in the store, and the registry hears about it.
#[test]
fn a_closed_bridge_says_why_persists_it_and_tells_the_registry() {
    in_context(async {
        let (_dir, sup, root) = harness();
        let thread = sup.state.borrow().roots[&root].row.root_thread_id.clone();
        let receipt = sup
            .spawn_worker(&root, spawn_request("closed-bridge"))
            .await
            .unwrap();
        let worker = spawned(&receipt);
        pump_until(|| sup.state.borrow().workers[&worker].run.is_none()).await;
        // Claude reuses its token on reconnect; the others cannot, so their loss is a rebind.
        sup.state
            .borrow_mut()
            .workers
            .get_mut(&worker)
            .unwrap()
            .driver = Driver::Codex;
        connect(&sup, &worker, ConnId(41)).await;
        let recorder = Rc::new(Recorder::default());
        let erased: Rc<dyn WorkerSubscriber> = recorder.clone();
        sup.subscribe(&thread, Rc::downgrade(&erased));
        let heard = recorder.log.borrow().len();
        sup.closed(ConnId(41));
        {
            let state = sup.state.borrow();
            let w = &state.workers[&worker];
            assert_eq!(w.row.delegation_status, "rebind_required");
            assert!(
                w.row
                    .delegation_reason
                    .as_deref()
                    .is_some_and(|r| !r.is_empty()),
                "a lost bridge must say why"
            );
        }
        pump_until(|| {
            sup.services
                .handle
                .reader()
                .delegation_worker(worker.as_str())
                .unwrap()
                .is_some_and(|w| {
                    w.delegation_status == "rebind_required" && w.delegation_reason.is_some()
                })
        })
        .await;
        assert!(
            recorder.log.borrow().len() > heard,
            "the registry was not told about the lost bridge"
        );
        let summary = sup
            .summaries(&thread)
            .into_iter()
            .find(|s| s.worker_id == worker)
            .unwrap();
        assert!(summary.delegation.reason.is_some());
        stop(&sup).await;
    });
}

/// The live agy failure: a fresh agy child's process starts asynchronously (hook proof, launch
/// guard), so its task must wait for that start, not be refused as `runtime_unavailable`.
#[test]
fn an_agy_child_gets_its_task_once_its_process_has_started() {
    in_context(async {
        let (dir, sup, root) = harness_with("agy", true);
        configure_agy(&sup, dir.path());
        let receipt = sup
            .spawn_worker(&root, spawn_request("agy-task"))
            .await
            .unwrap();
        let worker = spawned(&receipt);
        let (state, error) =
            settled_dispatch(&sup, receipt["operation_id"].as_str().unwrap()).await;
        assert!(
            matches!(state.as_str(), "submitted" | "confirmed"),
            "the agy task was not delivered: {state} {error:?}"
        );
        assert!(sup.state.borrow().workers[&worker].readback.ready);
        assert!(
            std::fs::read_dir(dir.path())
                .unwrap()
                .filter_map(Result::ok)
                .any(|e| e.file_name().to_string_lossy().starts_with("approval-")),
            "child approval socket must be bound in the supervisor's runtime directory"
        );
        stop(&sup).await;
    });
}

/// a task whose dispatch fails ends its run in memory as well as in the store, so `list`
/// and `cancel` agree, and the worker says why it cannot proceed.
#[test]
fn a_failed_dispatch_ends_the_run_so_list_and_cancel_agree() {
    in_context(async {
        let (dir, sup, root) = harness();
        std::fs::write(dir.path().join("hold-bootstrap"), "").unwrap();
        let receipt = sup
            .spawn_worker(&root, spawn_request("failed-dispatch"))
            .await
            .unwrap();
        let worker = spawned(&receipt);
        let run = receipt["run_id"].as_str().unwrap().to_owned();
        pump_until(|| {
            std::fs::read_to_string(dir.path().join("frames.ndjson"))
                .is_ok_and(|log| log.contains("startup check"))
        })
        .await;
        // Delegation is switched off while the startup check runs: the task cannot be sent.
        std::fs::write(dir.path().join("opted-out"), "").unwrap();
        std::fs::remove_file(dir.path().join("hold-bootstrap")).unwrap();
        let (state, _) = settled_dispatch(&sup, receipt["operation_id"].as_str().unwrap()).await;
        assert_eq!(state, "failed");
        pump_until(|| run_state(&sup, &run).is_some_and(|s| s == "failed")).await;
        std::fs::remove_file(dir.path().join("opted-out")).unwrap();
        pump_until(|| !sup.state.borrow().roots[&root].revoking).await;
        connect(&sup, &root, ConnId(42)).await;
        pump_until(|| sup.state.borrow().workers[&worker].run.is_none()).await;
        let listed = tool(&sup, ConnId(42), 1, ToolRequest::List(Default::default())).await;
        let entry = listed["workers"]
            .as_array()
            .unwrap()
            .iter()
            .find(|w| w["worker_id"] == worker.as_str())
            .unwrap()
            .clone();
        assert!(
            entry["run_id"].is_null(),
            "list still names the ended run: {entry}"
        );
        assert_eq!(entry["controls"]["cancel"], false);
        assert_eq!(entry["state"], "failed");
        assert!(
            entry["delegation"]["reason"].is_string(),
            "a worker that cannot proceed says why: {entry}"
        );
        // Cancel agrees: the run it would have named is over, and says so.
        let stale = call_err(
            &sup,
            ConnId(42),
            2,
            ToolRequest::Cancel(CancelRequest {
                expected_run_id: Some(agent_core::delegation::RunId::parse(&run).unwrap()),
                ..cancel_request(&worker, "stale-cancel")
            }),
        )
        .await;
        assert_eq!(stale.code, ErrorCode::StaleRun);
        stop(&sup).await;
    });
}

/// `list` reports the run `cancel` checks: the store's active run, with its stored state.
#[test]
fn list_reports_the_run_cancel_would_target() {
    in_context(async {
        let (dir, sup, root) = harness();
        connect(&sup, &root, ConnId(43)).await;
        std::fs::write(dir.path().join("silent-task"), "").unwrap();
        let receipt = sup
            .spawn_worker(&root, spawn_request("listed-run"))
            .await
            .unwrap();
        let worker = spawned(&receipt);
        pump_until(|| {
            std::fs::read_to_string(dir.path().join("frames.ndjson"))
                .is_ok_and(|log| log.contains("do the task"))
        })
        .await;
        let listed = tool(&sup, ConnId(43), 1, ToolRequest::List(Default::default())).await;
        let entry = &listed["workers"][0];
        let active = sup
            .services
            .handle
            .reader()
            .active_delegation_run(worker.as_str())
            .unwrap()
            .unwrap();
        assert_eq!(entry["run_id"], active.id.as_str());
        assert_eq!(entry["run_state"], active.state.as_str());
        let cancelled = tool(
            &sup,
            ConnId(43),
            2,
            ToolRequest::Cancel(CancelRequest {
                expected_run_id: Some(
                    agent_core::delegation::RunId::parse(entry["run_id"].as_str().unwrap())
                        .unwrap(),
                ),
                ..cancel_request(&worker, "listed-cancel")
            }),
        )
        .await;
        assert_ne!(cancelled["state"], "idle");
        stop(&sup).await;
    });
}

/// a task handed to the agent whose turn never opens is a stall: the worker fails with a
/// reason and its run ends, instead of `starting` for ever.
#[test]
fn a_task_that_never_starts_is_a_stall_with_a_reason() {
    in_context(async {
        let (dir, sup, root) = harness();
        sup.state.borrow_mut().task_start_deadline = Some(Duration::from_millis(300));
        std::fs::write(dir.path().join("silent-task"), "").unwrap();
        let receipt = sup
            .spawn_worker(&root, spawn_request("never-starts"))
            .await
            .unwrap();
        let worker = spawned(&receipt);
        let run = receipt["run_id"].as_str().unwrap().to_owned();
        pump_until(|| sup.state.borrow().workers[&worker].row.state == "failed").await;
        assert!(sup.state.borrow().workers[&worker]
            .row
            .delegation_reason
            .as_deref()
            .is_some_and(|r| r.contains("did not start")));
        assert!(sup.state.borrow().workers[&worker].run.is_none());
        pump_until(|| {
            run_state(&sup, &run).is_some_and(|s| matches!(s.as_str(), "failed" | "interrupted"))
        })
        .await;
        stop(&sup).await;
    });
}

/// A task whose turn opens in time is left alone by that deadline.
#[test]
fn a_task_that_starts_in_time_is_not_a_stall() {
    in_context(async {
        let (_dir, sup, root) = harness();
        sup.state.borrow_mut().task_start_deadline = Some(Duration::from_millis(300));
        let mut request = spawn_request("starts-in-time");
        request.task = "hold".into();
        let receipt = sup.spawn_worker(&root, request).await.unwrap();
        let worker = spawned(&receipt);
        pump_until(|| sup.state.borrow().workers[&worker].row.state == "running").await;
        glib::timeout_future(Duration::from_millis(500)).await;
        assert_eq!(sup.state.borrow().workers[&worker].row.state, "running");
        stop(&sup).await;
    });
}

/// A cursor reused with another filter is refused naming the filter it was issued for; a
/// read refused that way does not use up its idempotency key; and listing a worker with no
/// children is empty by design (`target` names the subtree root, which is never listed).
#[test]
fn a_cursor_error_names_its_filter_and_a_refused_read_keeps_its_key() {
    in_context(async {
        let (_dir, sup, root) = harness();
        connect(&sup, &root, ConnId(44)).await;
        let receipt = sup
            .spawn_worker(&root, spawn_request("cursor-filter"))
            .await
            .unwrap();
        let worker = spawned(&receipt);
        pump_until(|| sup.state.borrow().workers[&worker].run.is_none()).await;
        let waited = tool(
            &sup,
            ConnId(44),
            1,
            ToolRequest::Wait(WaitRequest {
                workers: Some(vec![worker.clone()]),
                timeout_ms: Some(1),
                ..Default::default()
            }),
        )
        .await;
        let cursor = Cursor::parse(waited["next_cursor"].as_str().unwrap()).unwrap();
        let refused = call_err(
            &sup,
            ConnId(44),
            2,
            ToolRequest::Read(ReadRequest {
                after: Some(cursor.clone()),
                ..Default::default()
            }),
        )
        .await;
        assert_eq!(refused.code, ErrorCode::InvalidArgument);
        assert!(
            refused.message.contains(worker.as_str()),
            "the error does not name the filter: {}",
            refused.message
        );
        let details = refused.details.clone().unwrap();
        assert_eq!(details["field"], "after");
        assert_eq!(details["issued_for"]["workers"][0], worker.as_str());

        // The child reports to its parent, so the parent has something to acknowledge.
        connect(&sup, &worker, ConnId(45)).await;
        let report = tool(
            &sup,
            ConnId(45),
            3,
            ToolRequest::Send(report_to_parent("report", "cursor-report")),
        )
        .await;
        let message = MessageId::parse(report["message_id"].as_str().unwrap()).unwrap();
        tool(&sup, ConnId(44), 4, ToolRequest::Read(Default::default())).await;
        let key = IdempotencyKey::parse("ack-after-refusal").unwrap();
        let bad = call_err(
            &sup,
            ConnId(44),
            5,
            ToolRequest::Read(ReadRequest {
                after: Some(cursor),
                acknowledge: Some(vec![message.clone()]),
                idempotency_key: Some(key.clone()),
                ..Default::default()
            }),
        )
        .await;
        assert_eq!(bad.code, ErrorCode::InvalidArgument);
        // The refusal applied nothing and recorded nothing under the key.
        let store = sup.services.handle.reader();
        assert!(store
            .delegation_message(message.as_str())
            .unwrap()
            .unwrap()
            .acknowledged_at
            .is_none());
        for kind in ["ack", "read"] {
            assert!(store
                .delegation_operation_by_key(root.as_str(), kind, key.as_str())
                .unwrap()
                .is_none());
        }
        let fixed = tool(
            &sup,
            ConnId(44),
            6,
            ToolRequest::Read(ReadRequest {
                acknowledge: Some(vec![message.clone()]),
                idempotency_key: Some(key),
                ..Default::default()
            }),
        )
        .await;
        assert_eq!(fixed["ack"]["acknowledged"][0], message.as_str());

        let childless = tool(
            &sup,
            ConnId(44),
            7,
            ToolRequest::List(ListRequest {
                target: Some(worker.clone()),
                ..Default::default()
            }),
        )
        .await;
        assert_eq!(childless["workers"], serde_json::json!([]));
        stop(&sup).await;
    });
}

fn follow_up(worker: &WorkerId, key: &str) -> SendRequest {
    instruction(worker, "follow-up", key)
}

/// A follow-up's run starts with the state its predecessor left (`waiting` after an
/// interruption, `failed` after a failed turn); that state is no evidence that the new turn
/// opened, so a silent follow-up is still a stall.
#[test]
fn a_silent_follow_up_is_a_stall_whatever_its_predecessor_left() {
    for predecessor in ["interrupted", "failed"] {
        in_context(async {
            let (dir, sup, root) = harness();
            let mut request = spawn_request(&format!("follow-{predecessor}"));
            request.task = "hold".into();
            let receipt = sup.spawn_worker(&root, request).await.unwrap();
            let worker = spawned(&receipt);
            pump_until(|| sup.state.borrow().workers[&worker].row.state == "running").await;
            let first = sup.state.borrow().workers[&worker].run.clone().unwrap();
            // A real interruption: the held turn ends interrupted, leaving `waiting`.
            sup.cancel_bound_run(&worker, &first, "Delegated run cancelled");
            pump_until(|| sup.state.borrow().workers[&worker].run.is_none()).await;
            assert_eq!(sup.state.borrow().workers[&worker].row.state, "waiting");
            if predecessor == "failed" {
                // The fixture cannot fail a turn: leave the state a failed turn leaves.
                sup.state
                    .borrow_mut()
                    .workers
                    .get_mut(&worker)
                    .unwrap()
                    .row
                    .state = "failed".into();
            }
            sup.state.borrow_mut().task_start_deadline = Some(Duration::from_millis(300));
            std::fs::write(dir.path().join("silent-task"), "").unwrap();
            sup.send_worker(&root, follow_up(&worker, &format!("silent-{predecessor}")))
                .await
                .unwrap();
            pump_until(|| {
                sup.state.borrow().workers[&worker]
                    .row
                    .delegation_reason
                    .as_deref()
                    .is_some_and(|r| r.contains("did not start"))
            })
            .await;
            assert!(sup.state.borrow().workers[&worker].run.is_none());
            stop(&sup).await;
        });
    }
}

/// A cancel that lands after the task left `pending` but before it was delivered stays a
/// cancel: the run ends interrupted and the worker is not failed (nor its authority revoked
/// a second time).
#[test]
fn a_cancel_between_taking_the_task_and_delivering_it_stays_a_cancel() {
    in_context(async {
        let (dir, sup, root) = harness();
        std::fs::write(dir.path().join("hold-bootstrap"), "").unwrap();
        let receipt = sup
            .spawn_worker(&root, spawn_request("cancel-gap"))
            .await
            .unwrap();
        let worker = spawned(&receipt);
        let run =
            agent_core::delegation::RunId::parse(receipt["run_id"].as_str().unwrap()).unwrap();
        pump_until(|| {
            std::fs::read_to_string(dir.path().join("frames.ndjson"))
                .is_ok_and(|log| log.contains("startup check"))
        })
        .await;
        assert!(sup.state.borrow().workers[&worker].pending.is_some());
        // No await between these: the delivery is scheduled with the task taken, and the cancel
        // lands before it runs.
        {
            // The startup check counts as passed, as it has when a task is taken for delivery.
            let mut state = sup.state.borrow_mut();
            let readback = &mut state.workers.get_mut(&worker).unwrap().readback;
            readback.ready = true;
            readback.bootstrap = false;
        }
        sup.dispatch_pending(&worker);
        assert!(sup.state.borrow().workers[&worker].pending.is_none());
        sup.cancel_bound_run(&worker, &run, "Delegated run cancelled");
        let (state, error) =
            settled_dispatch(&sup, receipt["operation_id"].as_str().unwrap()).await;
        assert_eq!(state, "failed");
        assert!(error.unwrap().contains("stale_run"));
        pump_until(|| {
            run_state(&sup, run.as_str()).is_some_and(|s| s != "reserved" && s != "starting")
        })
        .await;
        let ended = sup
            .services
            .handle
            .reader()
            .delegation_run(run.as_str())
            .unwrap()
            .unwrap();
        assert_eq!(
            ended.state, "interrupted",
            "a cancelled run was saved as failed"
        );
        assert_eq!(ended.terminal_reason.as_deref(), Some("cancelled"));
        let stored = sup
            .services
            .handle
            .reader()
            .delegation_worker(worker.as_str())
            .unwrap()
            .unwrap();
        assert_eq!(stored.state, "waiting");
        {
            let state = sup.state.borrow();
            let w = &state.workers[&worker];
            assert!(w.run.is_none());
            assert_eq!(w.row.state, "waiting", "the cancel became a failure");
            assert!(
                w.row.delegation_reason.is_none(),
                "{:?}",
                w.row.delegation_reason
            );
        }
        std::fs::remove_file(dir.path().join("hold-bootstrap")).unwrap();
        stop(&sup).await;
    });
}
