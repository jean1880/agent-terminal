//! Narrow application services shared by the supervisor and its single store writer.

use std::path::{Path, PathBuf};
use std::rc::Rc;

use agent_core::adapter::{Adapter, Driver};
use agent_core::catalog::CatalogModel;
use agent_core::delegation::{ErrorCode, ToolError};
use agent_kit::store::Store;
use agent_kit::store_writer::StoreHandle;

use super::seam::{LocalFuture, ResolvedChild};
use crate::account_status::Snapshot;
use crate::config::Profile;
use crate::hook_config::HookTarget;
use crate::model_tags_file::TagsState;

/// The narrow app services the supervisor holds (built from `window::imp::threads`).
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
    /// Models, tags and usage for `cross_agent_capabilities`.
    pub catalog: CatalogServices,
}

/// `cross_agent_capabilities` with `refresh` probes an agent only when its last reading or
/// attempt is this old (seconds), so repeated calls cannot storm a failing agent.
pub(crate) const USAGE_REFRESH_FLOOR: i64 = 60;

/// What `cross_agent_capabilities` reports about models and plan usage. The default knows no
/// model, no usage and no tag file (tests that do not exercise it).
#[derive(Clone)]
pub(crate) struct CatalogServices {
    /// The models a ready agent offers (none for an agent that is not ready).
    pub models: Rc<dyn Fn(Driver) -> Vec<CatalogModel>>,
    /// The agent's plan usage as last read; `None` when the app keeps none.
    pub usage: Rc<dyn Fn(Driver) -> Option<Snapshot>>,
    /// The tag file, cached and re-read off the main thread only when it changed.
    pub tags: Rc<dyn Fn() -> LocalFuture<TagsState>>,
    /// Re-reads usage that is at least a minute old and waits up to the given time for it.
    pub refresh_usage: Rc<dyn Fn(std::time::Duration) -> LocalFuture<()>>,
}

impl Default for CatalogServices {
    fn default() -> Self {
        Self {
            models: Rc::new(|_| Vec::new()),
            usage: Rc::new(|_| None),
            tags: Rc::new(|| -> LocalFuture<TagsState> {
                Box::pin(std::future::ready(TagsState {
                    tags: None,
                    status: Some("no model tag file configured".into()),
                }))
            }),
            refresh_usage: Rc::new(|_: std::time::Duration| -> LocalFuture<()> {
                Box::pin(std::future::ready(()))
            }),
        }
    }
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
        job: impl FnOnce(&Store) -> T + Send + 'static,
    ) -> Option<T> {
        let Some(path) = self.store_path.clone() else {
            return Some(job(self.handle.reader()));
        };
        let barrier = self.handle.writer().map(|w| w.barrier());
        gtk4::gio::spawn_blocking(move || {
            if let Some(barrier) = barrier {
                if barrier
                    .recv_timeout(std::time::Duration::from_secs(5))
                    .is_err()
                {
                    return None;
                }
            }
            Store::open(&path).ok().map(|s| job(&s))
        })
        .await
        .ok()
        .flatten()
    }

    /// A durable control write (`StoreWriter::durable_call`). Any failure is
    /// `persistence_failed`, and the caller dispatches nothing.
    pub(crate) async fn durable<T: Send + 'static>(
        &self,
        what: &'static str,
        job: impl FnOnce(&Store) -> agent_kit::store::Result<T> + Send + 'static,
    ) -> Result<T, ToolError> {
        let answer = if let Some(writer) = self.handle.writer() {
            let receiver = writer.durable_call(what, job);
            gtk4::gio::spawn_blocking(move || {
                receiver
                    .recv_timeout(std::time::Duration::from_secs(5))
                    .map_err(|_| agent_kit::store::StoreError::WriterGone)?
            })
            .await
            .unwrap_or(Err(agent_kit::store::StoreError::WriterGone))
        } else {
            self.handle.reader().atomically(job)
        };
        answer.map_err(|e| {
            (self.report)(&format!("Cannot save delegation control state: {e}"));
            ToolError::new(
                ErrorCode::PersistenceFailed,
                "cannot commit delegation control state",
            )
        })
    }

    /// An ordinary write (`StoreWriter::write_sized`).
    pub(crate) fn write(
        &self,
        what: &'static str,
        thread: Option<&str>,
        force: bool,
        job: impl FnOnce(&Store) -> agent_kit::store::Result<()> + Send + 'static,
    ) {
        self.write_sized(what, thread, 4096, force, job);
    }
    pub(crate) fn write_sized(
        &self,
        what: &'static str,
        thread: Option<&str>,
        bytes: usize,
        force: bool,
        job: impl FnOnce(&Store) -> agent_kit::store::Result<()> + Send + 'static,
    ) {
        let answer = if let Some(writer) = self.handle.writer() {
            writer.write_sized(what, thread, bytes, force, job)
        } else {
            self.handle.reader().atomically(job)
        };
        if let Err(e) = answer {
            (self.report)(&format!("Cannot save delegation activity: {e}"));
        }
    }
}
