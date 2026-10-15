//! Lane A: what the supervisor needs from the app (§2.1). Wave 0 stub.

use std::path::{Path, PathBuf};
use std::rc::Rc;

use agent_core::adapter::{Adapter, Driver};
use agent_core::delegation::{ErrorCode, ToolError};
use agent_kit::store::Store;
use agent_kit::store_writer::StoreHandle;

use super::seam::{LocalFuture, ResolvedChild};
use crate::config::Profile;
use crate::hook_config::HookTarget;

/// The narrow app services the supervisor holds (built by Lane E from `window::imp::threads`).
pub(crate) struct AppServices {
    pub handle: StoreHandle,
    /// `None`: memory or test store; jobs run in place.
    pub store_path: Option<PathBuf>,
    pub app_pid: u32,
    pub bridge_command: PathBuf,
    pub profiles: Rc<dyn Fn() -> Vec<Profile>>,
    pub resolve: Rc<ResolveChild>,
    pub make_adapter: Rc<MakeAdapter>,
    pub hook_check: Rc<HookCheck>,
    pub tab_open_in: Rc<dyn Fn(&Path) -> bool>,
    pub report: Rc<dyn Fn(&str)>,
}

/// `threads::cached_agent` plus `unset_list`, as a service.
pub(crate) type ResolveChild = dyn Fn(&Profile) -> Option<ResolvedChild>;
/// `threads::make_adapter`, as a service.
pub(crate) type MakeAdapter = dyn Fn(Driver, &str) -> Box<dyn Adapter>;
/// `threads::check_hook`, as a service.
pub(crate) type HookCheck = dyn Fn(HookTarget) -> LocalFuture<Result<(), String>>;

impl AppServices {
    /// A read on its own connection, after the writer's barrier (`threads::store_job`).
    pub(crate) async fn read<T: Send + 'static>(
        &self,
        _job: impl FnOnce(&Store) -> T + Send + 'static,
    ) -> Option<T> {
        None
    }

    /// A durable control write (`StoreWriter::durable_call`). Any failure is
    /// `persistence_failed`, and the caller dispatches nothing.
    pub(crate) async fn durable<T: Send + 'static>(
        &self,
        _what: &'static str,
        _job: impl FnOnce(&Store) -> agent_kit::store::Result<T> + Send + 'static,
    ) -> Result<T, ToolError> {
        Err(ToolError::new(
            ErrorCode::PersistenceFailed,
            "delegation is not built yet",
        ))
    }

    /// An ordinary write (`StoreWriter::write_sized`).
    pub(crate) fn write(
        &self,
        _what: &'static str,
        _thread: Option<&str>,
        _force: bool,
        _job: impl FnOnce(&Store) -> agent_kit::store::Result<()> + Send + 'static,
    ) {
    }
}
