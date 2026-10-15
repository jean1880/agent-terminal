//! The bridge ↔ supervisor IPC (contract §4).

/// The largest frame body, in bytes.
pub const MAX_FRAME: usize = 1 << 20;
/// Bridge side: connect, hello and welcome within this of process start (§1).
pub const WELCOME_DEADLINE_MS: u64 = 2_000;
/// Bridge side: the one reconnect's own deadline, counted from the detected loss (§4).
pub const RECONNECT_DEADLINE_MS: u64 = 2_000;
