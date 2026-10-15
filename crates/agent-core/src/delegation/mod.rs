//! Pure types of the cross-agent delegation runtime (`docs/delegation/contract-v1.md`,
//! built as specified in `docs/delegation/p1-types.md`).
//!
//! No I/O, no clock and no randomness: the app supplies the bytes, times, ids, file listings
//! and `/proc` text. Nothing here exposes a tool or changes an adapter.

pub mod env;
pub mod error;
pub mod ids;
pub mod token;

/// The control contract version (IPC hello negotiates `min..=max`; v1 accepts exactly 1).
pub const API_VERSION: u32 = 1;
/// The MCP server name, for every provider.
pub const SERVER_NAME: &str = "agent_terminal";
/// The bridge's only argument after the executable.
pub const BRIDGE_ARG: &str = "--cross-agent-mcp";

pub use error::{ErrorCode, ToolError};
pub use ids::{
    AuthorityEpoch, Cursor, IdError, IdempotencyKey, MessageId, OperationId, ProcessGeneration,
    ProfileId, RunId, WorkerId,
};
pub use token::{CapabilityToken, InstanceNonce};
