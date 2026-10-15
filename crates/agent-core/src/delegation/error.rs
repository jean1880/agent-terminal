//! Error codes and the tool error object (contract §3 "Error codes").

use std::fmt;

use serde::{Deserialize, Serialize};
use serde_json::Value;

/// Contract §3: 25 codes, plus `Unknown` for a code from a newer build.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ErrorCode {
    RuntimeUnavailable,
    AuthenticationFailed,
    WorkspaceUntrusted,
    ProtocolIncompatible,
    InvalidArgument,
    NotFound,
    ProviderUnavailable,
    RouteUnverified,
    UnsupportedDelivery,
    UnsupportedEffort,
    UnsupportedWorkspace,
    AuthorityDenied,
    ApprovalRequired,
    QuotaExhausted,
    CapacityReached,
    IdempotencyConflict,
    StaleEpoch,
    StaleRun,
    CursorExpired,
    WorkerClosed,
    DeadlineExceeded,
    PersistenceFailed,
    DeliveryUnknown,
    TransportLost,
    InternalError,
    /// A code from a newer build.
    #[serde(other)]
    Unknown,
}

impl ErrorCode {
    pub const ALL: [ErrorCode; 26] = [
        ErrorCode::RuntimeUnavailable,
        ErrorCode::AuthenticationFailed,
        ErrorCode::WorkspaceUntrusted,
        ErrorCode::ProtocolIncompatible,
        ErrorCode::InvalidArgument,
        ErrorCode::NotFound,
        ErrorCode::ProviderUnavailable,
        ErrorCode::RouteUnverified,
        ErrorCode::UnsupportedDelivery,
        ErrorCode::UnsupportedEffort,
        ErrorCode::UnsupportedWorkspace,
        ErrorCode::AuthorityDenied,
        ErrorCode::ApprovalRequired,
        ErrorCode::QuotaExhausted,
        ErrorCode::CapacityReached,
        ErrorCode::IdempotencyConflict,
        ErrorCode::StaleEpoch,
        ErrorCode::StaleRun,
        ErrorCode::CursorExpired,
        ErrorCode::WorkerClosed,
        ErrorCode::DeadlineExceeded,
        ErrorCode::PersistenceFailed,
        ErrorCode::DeliveryUnknown,
        ErrorCode::TransportLost,
        ErrorCode::InternalError,
        ErrorCode::Unknown,
    ];

    /// The contract §3 table: retry (with the same idempotency key) only for these.
    pub fn retryable(self) -> bool {
        match self {
            ErrorCode::CapacityReached
            | ErrorCode::DeadlineExceeded
            | ErrorCode::PersistenceFailed
            | ErrorCode::TransportLost => true,
            ErrorCode::RuntimeUnavailable
            | ErrorCode::AuthenticationFailed
            | ErrorCode::WorkspaceUntrusted
            | ErrorCode::ProtocolIncompatible
            | ErrorCode::InvalidArgument
            | ErrorCode::NotFound
            | ErrorCode::ProviderUnavailable
            | ErrorCode::RouteUnverified
            | ErrorCode::UnsupportedDelivery
            | ErrorCode::UnsupportedEffort
            | ErrorCode::UnsupportedWorkspace
            | ErrorCode::AuthorityDenied
            | ErrorCode::ApprovalRequired
            | ErrorCode::QuotaExhausted
            | ErrorCode::IdempotencyConflict
            | ErrorCode::StaleEpoch
            | ErrorCode::StaleRun
            | ErrorCode::CursorExpired
            | ErrorCode::WorkerClosed
            | ErrorCode::DeliveryUnknown
            | ErrorCode::InternalError
            | ErrorCode::Unknown => false,
        }
    }

    /// The snake_case wire name.
    pub fn as_str(self) -> &'static str {
        match self {
            ErrorCode::RuntimeUnavailable => "runtime_unavailable",
            ErrorCode::AuthenticationFailed => "authentication_failed",
            ErrorCode::WorkspaceUntrusted => "workspace_untrusted",
            ErrorCode::ProtocolIncompatible => "protocol_incompatible",
            ErrorCode::InvalidArgument => "invalid_argument",
            ErrorCode::NotFound => "not_found",
            ErrorCode::ProviderUnavailable => "provider_unavailable",
            ErrorCode::RouteUnverified => "route_unverified",
            ErrorCode::UnsupportedDelivery => "unsupported_delivery",
            ErrorCode::UnsupportedEffort => "unsupported_effort",
            ErrorCode::UnsupportedWorkspace => "unsupported_workspace",
            ErrorCode::AuthorityDenied => "authority_denied",
            ErrorCode::ApprovalRequired => "approval_required",
            ErrorCode::QuotaExhausted => "quota_exhausted",
            ErrorCode::CapacityReached => "capacity_reached",
            ErrorCode::IdempotencyConflict => "idempotency_conflict",
            ErrorCode::StaleEpoch => "stale_epoch",
            ErrorCode::StaleRun => "stale_run",
            ErrorCode::CursorExpired => "cursor_expired",
            ErrorCode::WorkerClosed => "worker_closed",
            ErrorCode::DeadlineExceeded => "deadline_exceeded",
            ErrorCode::PersistenceFailed => "persistence_failed",
            ErrorCode::DeliveryUnknown => "delivery_unknown",
            ErrorCode::TransportLost => "transport_lost",
            ErrorCode::InternalError => "internal_error",
            ErrorCode::Unknown => "unknown",
        }
    }
}

impl fmt::Display for ErrorCode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Longest `ToolError.message`, in bytes (contract §2).
pub const MAX_ERROR_MESSAGE_BYTES: usize = 1024;

/// The error every tool returns (contract §2 `ToolError`). `message` must already be scrubbed;
/// [`ToolError::new`] bounds it to [`MAX_ERROR_MESSAGE_BYTES`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ToolError {
    pub code: ErrorCode,
    pub message: String,
    pub retryable: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub details: Option<Value>,
}

/// `s` cut to at most `max` bytes at a character boundary.
pub(crate) fn truncate_bytes(s: &str, max: usize) -> &str {
    if s.len() <= max {
        return s;
    }
    let mut end = max;
    while !s.is_char_boundary(end) {
        end -= 1;
    }
    &s[..end]
}

impl ToolError {
    /// `retryable` comes from the code.
    pub fn new(code: ErrorCode, message: impl Into<String>) -> Self {
        let message: String = message.into();
        let message = truncate_bytes(&message, MAX_ERROR_MESSAGE_BYTES).to_owned();
        Self {
            code,
            message,
            retryable: code.retryable(),
            details: None,
        }
    }

    pub fn with_details(mut self, details: Value) -> Self {
        self.details = Some(details);
        self
    }

    /// Sets `details[key] = value`, making `details` an object if it was not one.
    pub fn with_detail(mut self, key: &str, value: impl Into<Value>) -> Self {
        let mut map = match self.details.take() {
            Some(Value::Object(map)) => map,
            _ => serde_json::Map::new(),
        };
        map.insert(key.to_owned(), value.into());
        self.details = Some(Value::Object(map));
        self
    }

    /// `invalid_argument` with `details.field` and `details.reason`.
    pub fn invalid(field: &str, reason: &str) -> Self {
        Self::new(
            ErrorCode::InvalidArgument,
            format!("invalid argument `{field}`: {reason}"),
        )
        .with_detail("field", field)
        .with_detail("reason", reason)
    }
}

impl fmt::Display for ToolError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}: {}", self.code, self.message)
    }
}

impl std::error::Error for ToolError {}

#[cfg(test)]
mod tests {
    use super::*;

    /// `ALL` is hand-maintained: `next` has no wildcard, so a new variant fails to compile
    /// until it is chained in, and the walk must then equal `ALL`.
    #[test]
    fn all_lists_every_code_once_in_order() {
        use ErrorCode::*;
        fn next(c: ErrorCode) -> Option<ErrorCode> {
            match c {
                RuntimeUnavailable => Some(AuthenticationFailed),
                AuthenticationFailed => Some(WorkspaceUntrusted),
                WorkspaceUntrusted => Some(ProtocolIncompatible),
                ProtocolIncompatible => Some(InvalidArgument),
                InvalidArgument => Some(NotFound),
                NotFound => Some(ProviderUnavailable),
                ProviderUnavailable => Some(RouteUnverified),
                RouteUnverified => Some(UnsupportedDelivery),
                UnsupportedDelivery => Some(UnsupportedEffort),
                UnsupportedEffort => Some(UnsupportedWorkspace),
                UnsupportedWorkspace => Some(AuthorityDenied),
                AuthorityDenied => Some(ApprovalRequired),
                ApprovalRequired => Some(QuotaExhausted),
                QuotaExhausted => Some(CapacityReached),
                CapacityReached => Some(IdempotencyConflict),
                IdempotencyConflict => Some(StaleEpoch),
                StaleEpoch => Some(StaleRun),
                StaleRun => Some(CursorExpired),
                CursorExpired => Some(WorkerClosed),
                WorkerClosed => Some(DeadlineExceeded),
                DeadlineExceeded => Some(PersistenceFailed),
                PersistenceFailed => Some(DeliveryUnknown),
                DeliveryUnknown => Some(TransportLost),
                TransportLost => Some(InternalError),
                InternalError => Some(Unknown),
                Unknown => None,
            }
        }
        let mut walked = vec![RuntimeUnavailable];
        while let Some(c) = next(*walked.last().expect("non-empty")) {
            walked.push(c);
        }
        assert_eq!(walked, ErrorCode::ALL);
        assert_eq!(ErrorCode::ALL.len(), 26);
    }

    #[test]
    fn codes_serialise_snake_case_and_unknown_decodes() {
        for code in ErrorCode::ALL {
            let json = serde_json::to_value(code).expect("ser");
            assert_eq!(json, Value::String(code.as_str().to_owned()));
            let back: ErrorCode = serde_json::from_value(json).expect("de");
            assert_eq!(back, code);
        }
        let future: ErrorCode = serde_json::from_str("\"from_the_future\"").expect("de");
        assert_eq!(future, ErrorCode::Unknown);
    }

    #[test]
    fn retryable_matches_the_contract_table() {
        let retryable: Vec<ErrorCode> = ErrorCode::ALL
            .into_iter()
            .filter(|c| c.retryable())
            .collect();
        assert_eq!(
            retryable,
            vec![
                ErrorCode::CapacityReached,
                ErrorCode::DeadlineExceeded,
                ErrorCode::PersistenceFailed,
                ErrorCode::TransportLost
            ]
        );
        assert!(!ErrorCode::InternalError.retryable());
        assert!(!ErrorCode::DeliveryUnknown.retryable());
    }

    #[test]
    fn tool_error_takes_retryable_from_its_code_and_bounds_its_message() {
        let e = ToolError::new(ErrorCode::CapacityReached, "full");
        assert!(e.retryable);
        assert_eq!(e.details, None);
        let json = serde_json::to_value(&e).expect("ser");
        assert_eq!(
            json,
            serde_json::json!({"code": "capacity_reached", "message": "full", "retryable": true})
        );
        let long = ToolError::new(ErrorCode::NotFound, "é".repeat(1000));
        assert!(long.message.len() <= MAX_ERROR_MESSAGE_BYTES);
        let inv = ToolError::invalid("task", "too_long");
        assert_eq!(inv.code, ErrorCode::InvalidArgument);
        assert_eq!(
            inv.details,
            Some(serde_json::json!({"field": "task", "reason": "too_long"}))
        );
    }
}
