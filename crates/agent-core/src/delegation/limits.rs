//! The contract §9 limits, and admission checks against them.

use super::error::{ErrorCode, ToolError};

/// Defaults the request normaliser fills (contract §3).
pub const DEFAULT_PAGE_EVENTS: u32 = 100;
pub const DEFAULT_PAGE_BYTES: u32 = 64 * 1024;
pub const DEFAULT_WAIT_TIMEOUT_MS: u32 = 10_000;
pub const DEFAULT_LIST_LIMIT: u32 = 50;

/// Every row of contract §9. Time values are milliseconds; sizes are bytes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Limits {
    // Runs, processes, depth, pending starts.
    pub active_runs_per_root: u32,
    pub active_runs_app: u32,
    /// An idle process still counts; only an observed exit frees a slot.
    pub live_processes_app: u32,
    pub max_depth: u8,
    pub pending_starts_per_root: u32,
    pub pending_starts_app: u32,
    // IPC.
    pub ipc_connections: u32,
    /// Unauthenticated connections, inside `ipc_connections`.
    pub ipc_preauth: u32,
    pub max_frame: u32,
    pub max_inflight: u32,
    pub hello_deadline_ms: u64,
    pub frame_body_deadline_ms: u64,
    pub write_deadline_ms: u64,
    pub outbound_queue_frames: u32,
    pub outbound_queue_bytes: u32,
    // Bridge.
    pub bridge_stdio_bytes: u32,
    pub welcome_deadline_ms: u64,
    pub reconnect_deadline_ms: u64,
    // agy and attestation.
    pub agy_hello_window_ms: u64,
    /// Observed calls kept per principal generation (oldest dropped first).
    pub observed_calls: u32,
    pub attestation_wait_ms: u64,
    // Waits.
    pub waits_app: u32,
    pub waits_principal: u32,
    pub wait_timeout_default_ms: u32,
    pub wait_timeout_max_ms: u32,
    // Request sizes.
    pub max_task_bytes: u32,
    pub max_message_bytes: u32,
    pub max_name_chars: u32,
    pub max_model_bytes: u32,
    pub max_effort_bytes: u32,
    pub max_base_bytes: u32,
    // Pages.
    pub page_events_default: u32,
    pub page_events_max: u32,
    pub page_bytes_min: u32,
    pub page_bytes_default: u32,
    pub page_bytes_max: u32,
    pub wait_workers_max: u32,
    pub ack_batch_max: u32,
    pub list_page_default: u32,
    pub list_page_max: u32,
    // Runs and continuations.
    pub run_deadline_default_ms: u64,
    pub run_deadline_max_ms: u64,
    pub continuations_per_task: u8,
    // L2 scan.
    pub l2_scan_depth: u32,
    pub l2_scan_entries: u32,
    // Output bounds.
    pub output_budget: u32,
    pub max_request_id_bytes: u32,
    pub max_summary_bytes: u32,
    pub max_receipt_bytes: u32,
    pub max_error_details_bytes: u32,
    pub max_ack_bytes: u32,
    pub max_warnings: u32,
    // Capabilities catalogue field caps.
    pub capabilities_profiles: u32,
    pub capabilities_models: u32,
    pub capabilities_efforts: u32,
    pub capabilities_blockers: u32,
}

impl Default for Limits {
    fn default() -> Self {
        Self {
            active_runs_per_root: 4,
            active_runs_app: 8,
            live_processes_app: 8,
            max_depth: super::authority::MAX_DEPTH,
            pending_starts_per_root: 32,
            pending_starts_app: 64,
            ipc_connections: 32,
            ipc_preauth: 8,
            max_frame: super::ipc::MAX_FRAME as u32,
            max_inflight: 4,
            hello_deadline_ms: 5_000,
            frame_body_deadline_ms: 5_000,
            write_deadline_ms: 5_000,
            outbound_queue_frames: 16,
            outbound_queue_bytes: 4 * 1024 * 1024,
            bridge_stdio_bytes: 1024 * 1024,
            welcome_deadline_ms: super::ipc::WELCOME_DEADLINE_MS,
            reconnect_deadline_ms: super::ipc::RECONNECT_DEADLINE_MS,
            agy_hello_window_ms: 30_000,
            observed_calls: 256,
            attestation_wait_ms: 2_000,
            waits_app: 64,
            waits_principal: 8,
            wait_timeout_default_ms: DEFAULT_WAIT_TIMEOUT_MS,
            wait_timeout_max_ms: 30_000,
            max_task_bytes: 64 * 1024,
            max_message_bytes: 16 * 1024,
            max_name_chars: 80,
            max_model_bytes: 128,
            max_effort_bytes: 32,
            max_base_bytes: 256,
            page_events_default: DEFAULT_PAGE_EVENTS,
            page_events_max: 500,
            page_bytes_min: 1024,
            page_bytes_default: DEFAULT_PAGE_BYTES,
            page_bytes_max: 256 * 1024,
            wait_workers_max: 32,
            ack_batch_max: 100,
            list_page_default: DEFAULT_LIST_LIMIT,
            list_page_max: 100,
            run_deadline_default_ms: 15 * 60 * 1000,
            run_deadline_max_ms: 120 * 60 * 1000,
            continuations_per_task: super::mailbox::ContinuationBudget::MAX,
            l2_scan_depth: 8,
            l2_scan_entries: 4096,
            output_budget: super::wire::OUTPUT_BUDGET as u32,
            max_request_id_bytes: super::wire::MAX_REQUEST_ID_BYTES as u32,
            max_summary_bytes: super::wire::MAX_SUMMARY_BYTES as u32,
            max_receipt_bytes: super::wire::MAX_RECEIPT_BYTES as u32,
            max_error_details_bytes: super::wire::MAX_ERROR_DETAILS_BYTES as u32,
            max_ack_bytes: super::wire::MAX_ACK_BYTES as u32,
            max_warnings: super::wire::MAX_WARNINGS as u32,
            capabilities_profiles: 32,
            capabilities_models: 64,
            capabilities_efforts: 8,
            capabilities_blockers: 16,
        }
    }
}

/// What is in use now, as the supervisor counts it.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Usage {
    pub active_runs_root: u32,
    pub active_runs_app: u32,
    pub live_processes: u32,
    pub pending_root: u32,
    pub pending_app: u32,
    pub waits_app: u32,
    pub waits_principal: u32,
    pub connections: u32,
    pub preauth_connections: u32,
}

fn capacity(limit: &str) -> ToolError {
    ToolError::new(
        ErrorCode::CapacityReached,
        format!("delegation limit reached: {limit}"),
    )
    .with_detail("limit", limit)
}

/// One more spawn: a pending start and an active run, plus a live process when it needs one.
pub fn admit_spawn(l: &Limits, u: &Usage, needs_process: bool) -> Result<(), ToolError> {
    let checks = [
        (
            u.active_runs_root,
            l.active_runs_per_root,
            "active_runs_per_root",
        ),
        (u.active_runs_app, l.active_runs_app, "active_runs_app"),
        (
            u.pending_root,
            l.pending_starts_per_root,
            "pending_starts_per_root",
        ),
        (u.pending_app, l.pending_starts_app, "pending_starts_app"),
    ];
    for (used, limit, name) in checks {
        if used >= limit {
            return Err(capacity(name));
        }
    }
    if needs_process && u.live_processes >= l.live_processes_app {
        return Err(capacity("live_processes_app"));
    }
    Ok(())
}

/// One more open wait.
pub fn admit_wait(l: &Limits, u: &Usage) -> Result<(), ToolError> {
    if u.waits_app >= l.waits_app {
        return Err(capacity("waits_app"));
    }
    if u.waits_principal >= l.waits_principal {
        return Err(capacity("waits_principal"));
    }
    Ok(())
}

/// One more (unauthenticated) connection; `false` closes it at once.
pub fn admit_connection(l: &Limits, u: &Usage) -> bool {
    u.connections < l.ipc_connections && u.preauth_connections < l.ipc_preauth
}

/// A read or wait page's `(max_events, max_bytes)`: defaults when omitted, `invalid_argument`
/// out of range.
pub fn clamp_page(
    l: &Limits,
    max_events: Option<u32>,
    max_bytes: Option<u32>,
) -> Result<(u32, u32), ToolError> {
    let events = max_events.unwrap_or(l.page_events_default);
    if events < 1 || events > l.page_events_max {
        return Err(ToolError::invalid("max_events", "out_of_range"));
    }
    let bytes = max_bytes.unwrap_or(l.page_bytes_default);
    if bytes < l.page_bytes_min || bytes > l.page_bytes_max {
        return Err(ToolError::invalid("max_bytes", "out_of_range"));
    }
    Ok((events, bytes))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::delegation::ipc::{RECONNECT_DEADLINE_MS, WELCOME_DEADLINE_MS};

    fn limit_of(e: ToolError) -> String {
        assert_eq!(e.code, ErrorCode::CapacityReached);
        assert!(e.retryable);
        e.details
            .and_then(|d| d.get("limit").and_then(|v| v.as_str()).map(str::to_owned))
            .unwrap_or_default()
    }

    #[test]
    fn spawn_admission_boundaries() {
        let l = Limits::default();
        let full = Usage {
            active_runs_root: 3,
            active_runs_app: 7,
            live_processes: 7,
            pending_root: 31,
            pending_app: 63,
            ..Usage::default()
        };
        assert_eq!(admit_spawn(&l, &full, true), Ok(()));
        type Bump = fn(&mut Usage);
        let cases: [(Bump, &str); 5] = [
            (|u| u.active_runs_root += 1, "active_runs_per_root"),
            (|u| u.active_runs_app += 1, "active_runs_app"),
            (|u| u.pending_root += 1, "pending_starts_per_root"),
            (|u| u.pending_app += 1, "pending_starts_app"),
            (|u| u.live_processes += 1, "live_processes_app"),
        ];
        for (bump, name) in cases {
            let mut u = full;
            bump(&mut u);
            assert_eq!(limit_of(admit_spawn(&l, &u, true).expect_err(name)), name);
        }
        let mut no_process = full;
        no_process.live_processes = 8;
        assert_eq!(admit_spawn(&l, &no_process, false), Ok(()));
    }

    #[test]
    fn wait_and_connection_admission_boundaries() {
        let l = Limits::default();
        let at = Usage {
            waits_app: 63,
            waits_principal: 7,
            ..Usage::default()
        };
        assert_eq!(admit_wait(&l, &at), Ok(()));
        let mut app = at;
        app.waits_app = 64;
        assert_eq!(
            limit_of(admit_wait(&l, &app).expect_err("app")),
            "waits_app"
        );
        let mut principal = at;
        principal.waits_principal = 8;
        assert_eq!(
            limit_of(admit_wait(&l, &principal).expect_err("principal")),
            "waits_principal"
        );
        let conn = Usage {
            connections: 31,
            preauth_connections: 7,
            ..Usage::default()
        };
        assert!(admit_connection(&l, &conn));
        assert!(!admit_connection(
            &l,
            &Usage {
                connections: 32,
                ..conn
            }
        ));
        assert!(!admit_connection(
            &l,
            &Usage {
                preauth_connections: 8,
                ..conn
            }
        ));
    }

    #[test]
    fn clamp_page_defaults_and_bounds() {
        let l = Limits::default();
        assert_eq!(clamp_page(&l, None, None), Ok((100, 65_536)));
        assert_eq!(clamp_page(&l, Some(500), Some(262_144)), Ok((500, 262_144)));
        assert_eq!(clamp_page(&l, Some(1), Some(1024)), Ok((1, 1024)));
        for (e, b, field) in [
            (Some(0), None, "max_events"),
            (Some(501), None, "max_events"),
            (None, Some(1023), "max_bytes"),
            (None, Some(262_145), "max_bytes"),
        ] {
            let err = clamp_page(&l, e, b).expect_err(field);
            assert_eq!(err.code, ErrorCode::InvalidArgument);
            assert_eq!(
                err.details.as_ref().and_then(|d| d.get("field")),
                Some(&field.into())
            );
        }
    }

    #[test]
    fn deadlines_are_the_contract_values_and_separate() {
        let l = Limits::default();
        assert_eq!(l.welcome_deadline_ms, 2_000);
        assert_eq!(l.reconnect_deadline_ms, 2_000);
        assert_eq!(WELCOME_DEADLINE_MS, 2_000);
        assert_eq!(RECONNECT_DEADLINE_MS, 2_000);
        // Two constants, each its own deadline: the reconnect counts from the detected loss.
        let mut changed = l;
        changed.reconnect_deadline_ms = 3_000;
        assert_ne!(changed.welcome_deadline_ms, changed.reconnect_deadline_ms);
        assert_eq!(l.hello_deadline_ms, 5_000);
        assert_eq!(l.agy_hello_window_ms, 30_000);
        assert_eq!(l.observed_calls, 256);
        assert_eq!(l.l2_scan_depth, 8);
        assert_eq!(l.l2_scan_entries, 4096);
        assert_eq!(l.output_budget, 786_432);
    }
}
