//! Lane D: per-generation launch bindings and Claude overlay files (§5.5). Wave 0 stub.

use std::path::Path;

use super::seam::{BindingRequest, PreparedBinding, RuntimeDir};

/// Mints the token and, for Claude, writes the overlay (`O_EXCL|O_NOFOLLOW`, `0600`). Blocking:
/// run in `gio::spawn_blocking`.
pub(crate) fn prepare_binding(_req: &BindingRequest) -> Result<PreparedBinding, String> {
    Err("delegation is not built yet".to_owned())
}

/// Removes an overlay after its generation's observed exit.
pub(crate) fn unlink_overlay(_path: &Path) {}

/// Startup: removes every `mcp-<pid>-*.json` whose app pid is not a live agent-terminal.
pub(crate) fn sweep_overlays(_dir: &RuntimeDir, _live_app_pid: &dyn Fn(u32) -> bool) {}
