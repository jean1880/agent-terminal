//! Startup reconciliation never dispatches or signals an old pid.
use super::services::AppServices;
use super::supervisor::Supervisor;
use agent_core::delegation::authority::WorkspaceScope;
use agent_core::delegation::ToolError;
use agent_core::delegation::{ProcessGeneration, RunId, WorkerId};
use std::path::PathBuf;
/// The capabilities a child's launch record declared: none when it declared nothing (a child
/// started before they existed included). A declaration that no longer reads is kept as the
/// strictest one, nothing beyond reading, rather than dropped.
fn stored_capabilities(
    launch: &serde_json::Value,
) -> Option<Vec<agent_core::delegation::authority::Capability>> {
    match launch.get("capabilities") {
        None | Some(serde_json::Value::Null) => None,
        Some(declared) => Some(serde_json::from_value(declared.clone()).unwrap_or_default()),
    }
}

pub(super) async fn startup(services: &AppServices) -> Result<(), ToolError> {
    services
        .durable("reconcile delegation startup", |s| {
            s.reconcile_delegation_after_restart()
        })
        .await?;
    Ok(())
}

impl Supervisor {
    /// Restores descriptions and native resume ids, never a pending dispatch or process.
    pub(super) async fn restore_children(&self, root: &WorkerId) {
        let rows = {
            let state = self.state.borrow();
            state
                .workers
                .iter()
                .filter(|(_, w)| {
                    &w.root == root && w.row.parent_id.is_some() && w.session.is_none()
                })
                .map(|(id, w)| (id.clone(), w.row.clone()))
                .collect::<Vec<_>>()
        };
        let profiles = (self.services.profiles)();
        for (worker, row) in rows {
            let Some(profile) = profiles.iter().find(|p| p.id == row.profile_id).cloned() else {
                continue;
            };
            let (Some(thread), Some(parent), Some(driver), Some(mode)) = (
                row.backing_thread_id.clone(),
                row.parent(),
                row.driver_kind(),
                row.task_mode_kind(),
            ) else {
                continue;
            };
            let query_thread = thread.clone();
            let query_worker = worker.clone();
            let loaded = self
                .services
                .read(move |s| {
                    let cwd = s
                        .thread_summary(&query_thread)?
                        .map(|t| PathBuf::from(t.cwd));
                    let providers = s.provider_threads(&query_thread)?;
                    let mut after = None;
                    let mut last = None;
                    loop {
                        let events = s.events(&query_thread, after, 256)?;
                        if events.is_empty() {
                            break;
                        }
                        for (seq, _) in events {
                            after = Some(seq);
                            for link in s
                                .delegation_event_links(seq)?
                                .into_iter()
                                .filter(|l| l.worker_id == query_worker.as_str())
                            {
                                if let Some(run) = link.run_id {
                                    if let Some(row) = s.delegation_run(&run)? {
                                        last = Some(row);
                                    }
                                }
                            }
                        }
                    }
                    Ok::<_, agent_kit::store::StoreError>((cwd, providers, last))
                })
                .await;
            let Some(Ok((Some(cwd), providers, last))) = loaded else {
                continue;
            };
            let launch =
                serde_json::from_str::<serde_json::Value>(&row.launch_json).unwrap_or_default();
            let provider = providers
                .into_iter()
                .find(|p| p.driver == driver.info().key);
            let lease = launch
                .get("source_repo")
                .and_then(|p| p.as_str())
                .zip(launch.get("branch").and_then(|p| p.as_str()))
                .zip(launch.get("base_commit").and_then(|p| p.as_str()))
                .map(|((repo, branch), base)| super::workspace::WorktreeLease {
                    source_repo: PathBuf::from(repo),
                    worktree: cwd.clone(),
                    branch: branch.into(),
                    base_commit: base.into(),
                });
            let scope = lease
                .as_ref()
                .map(super::workspace::scope)
                .unwrap_or_else(|| WorkspaceScope::Shared {
                    root: cwd.to_string_lossy().into_owned(),
                });
            let spec = super::child::ChildSpec {
                worker: worker.clone(),
                root: root.clone(),
                parent,
                depth: row.depth as u8,
                profile,
                driver,
                mode,
                capabilities: stored_capabilities(&launch),
                model: launch
                    .get("model")
                    .and_then(|v| v.as_str())
                    .map(str::to_owned),
                effort: launch
                    .get("effort")
                    .and_then(|v| v.as_str())
                    .map(str::to_owned),
                scope,
                cwd: cwd.clone(),
                thread,
                provider: provider.as_ref().map(|p| p.id.clone()).unwrap_or_default(),
                resume: provider.and_then(|p| p.native_id),
                lease,
            };
            let mut state = self.state.borrow_mut();
            if let Some(w) = state.workers.get_mut(&worker) {
                w.cwd = cwd;
                if let Some(run) = last {
                    w.generation = ProcessGeneration::from_stored(run.process_generation);
                    w.last_run = RunId::parse(&run.id).ok();
                }
            }
            state.child_specs.insert(worker, spec);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::stored_capabilities;
    use agent_core::delegation::authority::Capability;
    use serde_json::json;

    #[test]
    fn a_restored_declaration_never_widens() {
        assert_eq!(stored_capabilities(&json!({})), None);
        assert_eq!(stored_capabilities(&json!({"capabilities": null})), None);
        assert_eq!(
            stored_capabilities(&json!({"capabilities": ["exec"]})),
            Some(vec![Capability::Exec])
        );
        for unreadable in [json!(["exec", "everything"]), json!("exec"), json!(3)] {
            assert_eq!(
                stored_capabilities(&json!({ "capabilities": unreadable })),
                Some(vec![])
            );
        }
    }
}
