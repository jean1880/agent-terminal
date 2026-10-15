//! Lane D: inherited authority for supervised children (§5.1–§5.3). Wave 0 stub: everything is
//! denied.

use agent_core::adapter::Driver;
use serde_json::Value;

use super::seam::{ApprovalGate, AuthorityParams, Effect, GateVerdict};

/// What a provider tool call does (§5.1 table). Anything not listed is `Unknown`.
pub(crate) fn effect(_driver: Driver, _tool: &str, _input: &Value) -> Effect {
    Effect::Unknown
}

/// The gate a supervised child's approvals pass first.
pub(crate) struct ChildAuthority {
    params: AuthorityParams,
}

impl ChildAuthority {
    pub(crate) fn new(params: AuthorityParams) -> Self {
        Self { params }
    }

    pub(crate) fn params(&self) -> &AuthorityParams {
        &self.params
    }
}

impl ApprovalGate for ChildAuthority {
    fn judge(&self, _tool: &str, _input: &Value, _cwd: Option<&str>) -> GateVerdict {
        GateVerdict::Deny {
            reason: "delegation is not built yet".to_owned(),
        }
    }

    fn rule_may_apply(&self, _action: &crate::always_allow::Action, _cwd: Option<&str>) -> bool {
        false
    }
}
