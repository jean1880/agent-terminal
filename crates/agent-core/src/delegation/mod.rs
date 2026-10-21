//! Pure types of the cross-agent delegation runtime.
//!
//! No I/O, no clock and no randomness: the app supplies the bytes, times, ids, file listings
//! and `/proc` text. Nothing here exposes a tool or changes an adapter.

pub mod agy_guard;
pub mod authority;
pub mod correlation;
pub mod env;
pub mod error;
pub mod identity;
pub mod ids;
pub mod ipc;
pub mod launch;
pub mod lifecycle;
pub mod limits;
pub mod mailbox;
pub mod operation;
pub mod page;
pub mod route;
pub mod scope;
pub mod token;
pub mod tools;
pub mod wire;

/// The control contract version (IPC hello negotiates `min..=max`; v1 accepts exactly 1).
pub const API_VERSION: u32 = 1;
/// The MCP server name, for every provider.
pub const SERVER_NAME: &str = "agent_terminal";
/// The bridge's only argument after the executable.
pub const BRIDGE_ARG: &str = "--cross-agent-mcp";

pub use authority::TaskMode;
pub use correlation::{attestation_arguments, invocation_from_meta, InvocationKey, ObservedCalls};
pub use error::{ErrorCode, ToolError};
pub use ids::{
    AuthorityEpoch, Cursor, IdError, IdempotencyKey, MessageId, OperationId, ProcessGeneration,
    ProfileId, RunId, WorkerId,
};
pub use launch::DelegationBinding;
pub use limits::Limits;
pub use route::{admit_route, DelegationCaps, RouteStatus};
pub use token::{CapabilityToken, InstanceNonce};
pub use tools::ToolName;
pub use wire::{parse_request, ToolRequest};
