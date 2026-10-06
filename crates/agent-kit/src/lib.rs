//! GTK-free git, checkpoint and session logic for agent-terminal.
//!
//! Blocking process and file I/O live here, never GTK: callers run the slow
//! parts through `gio::spawn_blocking`. Redaction is in `agent-core`.

pub mod diff;
pub mod difftool;
pub mod editdiff;
pub mod exec;
pub mod filediff;
pub mod fsutil;
pub mod git;
pub mod handoff;
pub mod paths;
pub mod restore;
pub mod sessions;
pub mod store;
pub mod worktree;
