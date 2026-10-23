//! Pure agent-terminal logic: no GTK, no process spawning, no file I/O.
//!
//! Everything here is plain data in, plain data out, so it is unit-tested
//! without a display, a repository or a filesystem.
//!
//! Several modules port logic from T3 Code (MIT, Copyright (c) 2026 T3 Tools Inc.); see
//! THIRD_PARTY.md at the repository root.

pub mod adapter;
pub mod agent_command;
pub mod agy;
pub mod approval;
pub mod caps;
pub mod catalog;
pub mod claude;
pub mod codex;
pub mod codex_rules;
pub mod commands;
pub mod delegation;
pub mod event;
pub mod form;
pub mod git_command;
pub mod handoff_budget;
pub mod import;
pub mod model_tags;
pub mod question_extract;
pub mod quota;
pub mod redact;
pub mod serde_name;
pub mod shell_words;
pub mod text;
pub mod tool_title;
pub mod transition;
