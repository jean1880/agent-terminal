//! Lane D: worktree leases for `isolated_worktree` children (§6). Wave 0 stub. Every function
//! here blocks (git): run it in `gio::spawn_blocking`.

use std::path::{Path, PathBuf};

use agent_core::delegation::authority::WorkspaceScope;
use agent_core::delegation::{ErrorCode, ToolError, WorkerId};

/// What `plan_lease` resolved at acceptance.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct LeasePlan {
    pub source_repo: PathBuf,
    pub base_commit: String,
    pub branch: String,
    pub path: PathBuf,
    /// The caller's tree had uncommitted changes (receipt warning `parent_changes_absent`).
    pub dirty_parent: bool,
}

/// A created worktree, canonical and checked.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct WorktreeLease {
    pub source_repo: PathBuf,
    pub worktree: PathBuf,
    pub branch: String,
    pub base_commit: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Cleanup {
    Removed,
    Retained(String),
}

fn not_built() -> ToolError {
    ToolError::new(ErrorCode::InternalError, "delegation is not built yet")
}

pub(crate) fn plan_lease(
    _caller_tree: &Path,
    _base: &str,
    _worker: &WorkerId,
    _root: Option<&Path>,
) -> Result<LeasePlan, ToolError> {
    Err(not_built())
}

pub(crate) fn create_lease(_plan: &LeasePlan) -> Result<WorktreeLease, ToolError> {
    Err(not_built())
}

pub(crate) fn verify_lease(_lease: &WorktreeLease) -> Result<(), ToolError> {
    Err(not_built())
}

pub(crate) fn cleanup_if_clean(
    _lease: &WorktreeLease,
    _tab_open_in: bool,
) -> Result<Cleanup, String> {
    Ok(Cleanup::Retained("delegation is not built yet".to_owned()))
}

/// The lease as the authority scope it grants.
pub(crate) fn scope(lease: &WorktreeLease) -> WorkspaceScope {
    WorkspaceScope::Lease {
        source_repo: lease.source_repo.to_string_lossy().into_owned(),
        worktree: lease.worktree.to_string_lossy().into_owned(),
        base_commit: lease.base_commit.clone(),
        allowed_relative: Vec::new(),
    }
}
