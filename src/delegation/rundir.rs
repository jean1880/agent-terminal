//! Lane D: the verified runtime directory (§5.5). Wave 0 stub.

use super::seam::RuntimeDir;

/// `$XDG_RUNTIME_DIR/agent-terminal`, created `0700` if absent, then proven by `lstat`: a
/// directory, not a symlink, owned by this uid, mode `0700`. Blocking: run off the main thread.
pub(crate) fn verified() -> Result<RuntimeDir, String> {
    Err("delegation is not built yet".to_owned())
}
