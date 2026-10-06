//! Pure agent-terminal logic: no GTK, no process spawning, no file I/O.
//!
//! Everything here is plain data in, plain data out, so it is unit-tested
//! without a display, a repository or a filesystem.
//!
//! Several modules port logic from T3 Code (MIT, Copyright (c) 2026 T3 Tools Inc.); see
//! THIRD_PARTY.md at the repository root.

pub mod adapter;
pub mod agy;
pub mod approval;
pub mod caps;
pub mod catalog;
pub mod claude;
pub mod codex;
pub mod commands;
pub mod event;
pub mod handoff_budget;
pub mod quota;
pub mod redact;
pub mod transition;
