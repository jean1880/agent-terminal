//! Pure agent-terminal logic: no GTK, no process spawning, no file I/O.
//!
//! Everything here is plain data in, plain data out, so it is unit-tested
//! without a display, a repository or a filesystem.

pub mod redact;
