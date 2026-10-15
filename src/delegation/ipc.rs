//! Lane C: the delegation IPC server (§4.1–§4.2): a GIO Unix socket on the main context. Wave 0
//! stub: binding always fails, so delegation stays unavailable.

mod conn;
pub(crate) mod peer;

use std::path::{Path, PathBuf};
use std::rc::Rc;

use agent_core::delegation::{Limits, ToolError};

use super::seam::{CallDispatcher, ConnId, RuntimeDir};

/// The listener on `<rundir>/delegation-<app_pid>.sock` and its connections.
pub(crate) struct IpcServer {
    path: PathBuf,
}

impl IpcServer {
    pub(crate) fn bind(
        _dir: &RuntimeDir,
        _app_pid: u32,
        _limits: Limits,
        _dispatcher: Rc<dyn CallDispatcher>,
    ) -> Result<Rc<Self>, String> {
        Err("delegation is not built yet".to_owned())
    }

    pub(crate) fn socket_path(&self) -> &Path {
        &self.path
    }

    /// Sends `revoked` with `error`, flushes (at most the write deadline), then closes.
    pub(crate) fn revoke(&self, _conn: ConnId, _error: ToolError) {}

    pub(crate) fn close_all(&self, _error: ToolError) {}

    /// (all connections, unauthenticated connections).
    pub(crate) fn counts(&self) -> (u32, u32) {
        (0, 0)
    }
}
