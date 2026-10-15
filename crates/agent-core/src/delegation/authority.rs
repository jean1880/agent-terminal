//! Authority (contract §6): task modes, the live mode ceiling, depth, workspace rules,
//! inherited authority and the revocation table.

use serde::{Deserialize, Serialize};

use super::error::ErrorCode;
use super::ids::{AuthorityEpoch, WorkerId};
use super::mailbox::{kind_allowed, Direction, MessageKind};
use super::wire::WorkspaceRequest;
use crate::adapter::Mode;

/// A supervised child's mode (contract §2 `TaskMode`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TaskMode {
    Ask,
    AcceptEdits,
    Plan,
}

impl TaskMode {
    pub const ALL: [TaskMode; 3] = [TaskMode::Ask, TaskMode::AcceptEdits, TaskMode::Plan];

    /// The session mode the child runs in.
    pub fn as_mode(self) -> Mode {
        match self {
            TaskMode::Ask => Mode::Ask,
            TaskMode::AcceptEdits => Mode::AcceptEdits,
            TaskMode::Plan => Mode::Plan,
        }
    }

    /// Whether the mode can change files or run commands (and so needs an isolated worktree).
    pub fn writes(self) -> bool {
        match self {
            TaskMode::Ask | TaskMode::AcceptEdits => true,
            TaskMode::Plan => false,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            TaskMode::Ask => "ask",
            TaskMode::AcceptEdits => "accept_edits",
            TaskMode::Plan => "plan",
        }
    }
}

const CEILING_ACCEPT_EDITS: [TaskMode; 3] = [TaskMode::AcceptEdits, TaskMode::Ask, TaskMode::Plan];
const CEILING_ASK: [TaskMode; 2] = [TaskMode::Ask, TaskMode::Plan];
const CEILING_PLAN: [TaskMode; 1] = [TaskMode::Plan];

/// The contract §6 ceiling table, applied to the principal's mode after
/// [`Mode::session_safe`] (Bypass never propagates: it counts as Ask).
pub fn ceiling(principal_mode: Mode) -> &'static [TaskMode] {
    match principal_mode.session_safe() {
        Mode::AcceptEdits => &CEILING_ACCEPT_EDITS,
        Mode::Ask | Mode::Bypass => &CEILING_ASK,
        Mode::Plan => &CEILING_PLAN,
    }
}

/// Whether `requested` is within the ceiling of `principal_mode`.
pub fn permits(principal_mode: Mode, requested: TaskMode) -> bool {
    ceiling(principal_mode).contains(&requested)
}

/// The root is depth 0; the deepest worker is depth 2.
pub const MAX_DEPTH: u8 = 2;

/// A worker at `depth` may spawn only below [`MAX_DEPTH`].
pub fn can_spawn(depth: u8) -> bool {
    depth < MAX_DEPTH
}

/// Writers (`ask`, `accept_edits`) need an isolated worktree; a shared read-only tree needs
/// `plan` (contract §3 spawn rules). Whether the provider's read-only enforcement is proven is
/// the route's question, not this one.
pub fn workspace_allowed(mode: TaskMode, ws: &WorkspaceRequest) -> Result<(), ErrorCode> {
    match ws {
        WorkspaceRequest::SharedReadOnly if mode.writes() => Err(ErrorCode::UnsupportedWorkspace),
        WorkspaceRequest::SharedReadOnly | WorkspaceRequest::IsolatedWorktree { .. } => Ok(()),
    }
}

/// Every parent→child delivery, any kind: the recipient's `task_mode` must be within the
/// sender's live ceiling (an unknown recipient mode is refused). Child→parent: kind rules only.
/// Called at acceptance and again at dispatch (E7).
pub fn delivery_allowed(
    dir: Direction,
    kind: MessageKind,
    sender_mode: Mode,
    recipient_mode: Option<TaskMode>,
) -> Result<(), ErrorCode> {
    if !kind_allowed(dir, kind) {
        return Err(ErrorCode::InvalidArgument);
    }
    match dir {
        Direction::ChildToParent => Ok(()),
        Direction::ParentToChild => match recipient_mode {
            Some(m) if permits(sender_mode, m) => Ok(()),
            _ => Err(ErrorCode::AuthorityDenied),
        },
    }
}

/// What a worker's own path into the filesystem is.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WorkspaceScope {
    Shared {
        root: String,
    },
    Lease {
        source_repo: String,
        worktree: String,
        base_commit: String,
        allowed_relative: Vec<String>,
    },
}

/// The authority a worker holds, inherited down the tree (contract §6, E5).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InheritedAuthority {
    pub worker: WorkerId,
    pub root: WorkerId,
    pub parent: Option<WorkerId>,
    pub epoch: AuthorityEpoch,
    pub policy_revision: u64,
    /// The worker's effective mode, which is the ceiling for what it starts.
    pub mode_ceiling: Mode,
    pub depth: u8,
    pub workspace: WorkspaceScope,
}

/// How permissive a mode is: Plan < Ask < AcceptEdits (Bypass counts as Ask).
fn rank(mode: Mode) -> u8 {
    match mode.session_safe() {
        Mode::Plan => 0,
        Mode::Ask | Mode::Bypass => 1,
        Mode::AcceptEdits => 2,
    }
}

impl InheritedAuthority {
    /// The child's root = `self.root`, parent = `Some(self.worker)`, depth = `self.depth + 1`
    /// (`AuthorityDenied` past [`MAX_DEPTH`]), mode within `ceiling(self.mode_ceiling)`
    /// (`AuthorityDenied` otherwise), epoch = `self.epoch`.
    pub fn child(
        &self,
        child: WorkerId,
        mode: TaskMode,
        scope: WorkspaceScope,
    ) -> Result<InheritedAuthority, ErrorCode> {
        if !can_spawn(self.depth) {
            return Err(ErrorCode::AuthorityDenied);
        }
        if !permits(self.mode_ceiling, mode) {
            return Err(ErrorCode::AuthorityDenied);
        }
        Ok(InheritedAuthority {
            worker: child,
            root: self.root.clone(),
            parent: Some(self.worker.clone()),
            epoch: self.epoch,
            policy_revision: self.policy_revision,
            mode_ceiling: mode.as_mode(),
            depth: self.depth + 1,
            workspace: scope,
        })
    }

    /// The same authority with its ceiling narrowed to `mode` if that is narrower. Never
    /// widens, and never yields Bypass.
    pub fn narrow(&self, mode: Mode) -> InheritedAuthority {
        let current = self.mode_ceiling.session_safe();
        let proposed = mode.session_safe();
        let mut out = self.clone();
        out.mode_ceiling = if rank(proposed) < rank(current) {
            proposed
        } else {
            current
        };
        out
    }
}

/// An event that may revoke credentials (contract §6 revocation table).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RevocationEvent {
    /// The user stopped the root.
    RootStop,
    /// The user narrowed the root's mode.
    RootRestriction {
        new_mode: Mode,
    },
    /// The user widened the root's mode: no bump.
    RootWidening,
    Close(WorkerId),
    /// The observed exit of a worker's process generation.
    GenerationExited(WorkerId),
    AppShutdown,
}

/// Which part of the tree a bump covers.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RevocationScope {
    Root,
    Subtree(WorkerId),
    None,
}

/// What a revocation does. The caller applies it (connections get `revoked`, then close).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RevocationEffect {
    /// Bump the root's authority epoch once.
    pub bump: bool,
    pub scope: RevocationScope,
    /// Workers whose current token dies.
    pub kill_tokens: Vec<WorkerId>,
    /// Workers whose runs are interrupted and processes stopped.
    pub stop_processes: Vec<WorkerId>,
    /// Workers marked `restricted` (no automatic rebind; sends refused; still readable).
    pub mark_restricted: Vec<WorkerId>,
    /// The root's own token stays valid.
    pub keep_root_token: bool,
}

/// The root of a tree snapshot: the first entry with no parent.
fn tree_root(tree: &[(WorkerId, Option<WorkerId>, Option<TaskMode>)]) -> Option<&WorkerId> {
    tree.iter().find(|(_, p, _)| p.is_none()).map(|(w, _, _)| w)
}

/// `of` and every descendant, in tree order (breadth first). Bounded by the snapshot's size.
fn subtree(
    tree: &[(WorkerId, Option<WorkerId>, Option<TaskMode>)],
    of: &WorkerId,
) -> Vec<WorkerId> {
    let mut out = vec![of.clone()];
    let mut i = 0;
    while i < out.len() && out.len() <= tree.len() {
        let current = out[i].clone();
        for (w, p, _) in tree {
            if p.as_ref() == Some(&current) && !out.contains(w) {
                out.push(w.clone());
            }
        }
        i += 1;
    }
    out
}

/// The contract §6 revocation table over a caller-supplied tree snapshot:
/// `(worker, parent, task_mode)`, the root having no parent.
pub fn revocation(
    event: &RevocationEvent,
    tree: &[(WorkerId, Option<WorkerId>, Option<TaskMode>)],
) -> RevocationEffect {
    let root = tree_root(tree);
    let descendants: Vec<WorkerId> = match root {
        Some(r) => subtree(tree, r).into_iter().skip(1).collect(),
        None => Vec::new(),
    };
    let nothing = RevocationEffect {
        bump: false,
        scope: RevocationScope::None,
        kill_tokens: Vec::new(),
        stop_processes: Vec::new(),
        mark_restricted: Vec::new(),
        keep_root_token: true,
    };
    match event {
        RevocationEvent::RootStop => RevocationEffect {
            bump: true,
            scope: RevocationScope::Root,
            kill_tokens: descendants.clone(),
            stop_processes: descendants,
            mark_restricted: Vec::new(),
            keep_root_token: true,
        },
        RevocationEvent::RootRestriction { new_mode } => {
            let out: Vec<WorkerId> = descendants
                .into_iter()
                .filter(|w| {
                    let mode = tree
                        .iter()
                        .find(|(x, _, _)| x == w)
                        .and_then(|(_, _, m)| *m);
                    !matches!(mode, Some(m) if permits(*new_mode, m))
                })
                .collect();
            RevocationEffect {
                bump: true,
                scope: RevocationScope::Root,
                kill_tokens: out.clone(),
                stop_processes: out.clone(),
                mark_restricted: out,
                keep_root_token: true,
            }
        }
        RevocationEvent::RootWidening => nothing,
        RevocationEvent::Close(w) => {
            let all = subtree(tree, w);
            RevocationEffect {
                bump: true,
                scope: RevocationScope::Subtree(w.clone()),
                kill_tokens: all.clone(),
                stop_processes: all,
                mark_restricted: Vec::new(),
                keep_root_token: root != Some(w),
            }
        }
        RevocationEvent::GenerationExited(w) => RevocationEffect {
            bump: true,
            scope: RevocationScope::Subtree(w.clone()),
            kill_tokens: vec![w.clone()],
            stop_processes: Vec::new(),
            mark_restricted: Vec::new(),
            keep_root_token: root != Some(w),
        },
        RevocationEvent::AppShutdown => {
            let all: Vec<WorkerId> = tree.iter().map(|(w, _, _)| w.clone()).collect();
            RevocationEffect {
                bump: true,
                scope: RevocationScope::Root,
                kill_tokens: all.clone(),
                stop_processes: all,
                mark_restricted: Vec::new(),
                keep_root_token: false,
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const MODES: [Mode; 4] = [Mode::Ask, Mode::AcceptEdits, Mode::Plan, Mode::Bypass];

    fn w(s: &str) -> WorkerId {
        WorkerId::parse(&format!("w_{s}")).expect("valid id")
    }

    fn root_authority(mode: Mode) -> InheritedAuthority {
        InheritedAuthority {
            worker: w("root0000"),
            root: w("root0000"),
            parent: None,
            epoch: AuthorityEpoch(3),
            policy_revision: 1,
            mode_ceiling: mode,
            depth: 0,
            workspace: WorkspaceScope::Shared {
                root: "/repo".into(),
            },
        }
    }

    fn lease() -> WorkspaceScope {
        WorkspaceScope::Lease {
            source_repo: "/repo".into(),
            worktree: "/wt".into(),
            base_commit: "abc".into(),
            allowed_relative: vec![],
        }
    }

    #[test]
    fn task_mode_all_is_exhaustive() {
        fn next(m: TaskMode) -> Option<TaskMode> {
            match m {
                TaskMode::Ask => Some(TaskMode::AcceptEdits),
                TaskMode::AcceptEdits => Some(TaskMode::Plan),
                TaskMode::Plan => None,
            }
        }
        let mut walked = vec![TaskMode::Ask];
        while let Some(m) = next(*walked.last().expect("non-empty")) {
            walked.push(m);
        }
        assert_eq!(walked, TaskMode::ALL);
        for m in TaskMode::ALL {
            assert_eq!(
                serde_json::to_value(m).expect("ser"),
                serde_json::json!(m.as_str())
            );
            assert_eq!(m.writes(), m != TaskMode::Plan);
        }
    }

    #[test]
    fn ceiling_table_covers_every_mode() {
        assert_eq!(
            ceiling(Mode::AcceptEdits),
            &[TaskMode::AcceptEdits, TaskMode::Ask, TaskMode::Plan]
        );
        assert_eq!(ceiling(Mode::Ask), &[TaskMode::Ask, TaskMode::Plan]);
        assert_eq!(ceiling(Mode::Bypass), ceiling(Mode::Ask));
        assert_eq!(ceiling(Mode::Plan), &[TaskMode::Plan]);
    }

    #[test]
    fn permits_covers_every_pair() {
        for m in MODES {
            for t in TaskMode::ALL {
                let expected = match (m, t) {
                    (Mode::AcceptEdits, _) => true,
                    (Mode::Ask | Mode::Bypass, TaskMode::AcceptEdits) => false,
                    (Mode::Ask | Mode::Bypass, _) => true,
                    (Mode::Plan, TaskMode::Plan) => true,
                    (Mode::Plan, _) => false,
                };
                assert_eq!(permits(m, t), expected, "{m:?} {t:?}");
            }
        }
    }

    #[test]
    fn depth_and_workspace_rules() {
        assert!(can_spawn(0));
        assert!(can_spawn(1));
        assert!(!can_spawn(2));
        let shared = WorkspaceRequest::SharedReadOnly;
        let wt = WorkspaceRequest::IsolatedWorktree { base: None };
        assert_eq!(
            workspace_allowed(TaskMode::Ask, &shared),
            Err(ErrorCode::UnsupportedWorkspace)
        );
        assert_eq!(
            workspace_allowed(TaskMode::AcceptEdits, &shared),
            Err(ErrorCode::UnsupportedWorkspace)
        );
        assert_eq!(workspace_allowed(TaskMode::Plan, &shared), Ok(()));
        for m in TaskMode::ALL {
            assert_eq!(workspace_allowed(m, &wt), Ok(()));
        }
    }

    #[test]
    fn delivery_allowed_applies_the_ceiling_to_every_parent_to_child_kind() {
        // A Plan sender to an accept_edits child: refused for instruction and reply alike.
        for kind in [MessageKind::Instruction, MessageKind::Reply] {
            assert_eq!(
                delivery_allowed(
                    Direction::ParentToChild,
                    kind,
                    Mode::Plan,
                    Some(TaskMode::AcceptEdits)
                ),
                Err(ErrorCode::AuthorityDenied)
            );
        }
        let kinds = [
            MessageKind::Instruction,
            MessageKind::Report,
            MessageKind::Question,
            MessageKind::Reply,
        ];
        for dir in [Direction::ParentToChild, Direction::ChildToParent] {
            for kind in kinds {
                let r = delivery_allowed(dir, kind, Mode::AcceptEdits, Some(TaskMode::Ask));
                let expected = if kind_allowed(dir, kind) {
                    Ok(())
                } else {
                    Err(ErrorCode::InvalidArgument)
                };
                assert_eq!(r, expected, "{dir:?} {kind:?}");
            }
        }
        // A child→parent report is judged by kind only.
        assert_eq!(
            delivery_allowed(
                Direction::ChildToParent,
                MessageKind::Report,
                Mode::Plan,
                None
            ),
            Ok(())
        );
        // An unknown recipient mode is refused.
        assert_eq!(
            delivery_allowed(
                Direction::ParentToChild,
                MessageKind::Instruction,
                Mode::AcceptEdits,
                None
            ),
            Err(ErrorCode::AuthorityDenied)
        );
    }

    #[test]
    fn child_and_narrow_never_widen() {
        for parent_mode in MODES {
            let root = root_authority(parent_mode);
            for t in TaskMode::ALL {
                match root.child(w("child000"), t, lease()) {
                    Ok(c) => {
                        assert!(permits(parent_mode, t));
                        assert!(rank(c.mode_ceiling) <= rank(parent_mode));
                        assert_ne!(c.mode_ceiling, Mode::Bypass);
                    }
                    Err(e) => {
                        assert_eq!(e, ErrorCode::AuthorityDenied);
                        assert!(!permits(parent_mode, t));
                    }
                }
            }
            for m in MODES {
                let n = root.narrow(m);
                assert!(rank(n.mode_ceiling) <= rank(parent_mode));
                assert!(rank(n.mode_ceiling) <= rank(m));
                assert_ne!(n.mode_ceiling, Mode::Bypass);
            }
        }
    }

    #[test]
    fn lineage_follows_the_tree_and_stops_at_depth_two() {
        let root = root_authority(Mode::AcceptEdits);
        let a = root
            .child(w("aaaaaaaa"), TaskMode::Ask, lease())
            .expect("a");
        assert_eq!(a.parent, Some(root.worker.clone()));
        assert_eq!(a.root, root.worker);
        assert_eq!(a.depth, 1);
        assert_eq!(a.epoch, root.epoch);
        let b = a.child(w("bbbbbbbb"), TaskMode::Plan, lease()).expect("b");
        assert_eq!(b.parent, Some(a.worker.clone()));
        assert_eq!(b.root, root.worker);
        assert_eq!(b.depth, 2);
        assert_eq!(
            b.child(w("cccccccc"), TaskMode::Plan, lease()),
            Err(ErrorCode::AuthorityDenied)
        );
    }

    fn tree() -> Vec<(WorkerId, Option<WorkerId>, Option<TaskMode>)> {
        vec![
            (w("root0000"), None, None),
            (
                w("aaaaaaaa"),
                Some(w("root0000")),
                Some(TaskMode::AcceptEdits),
            ),
            (w("bbbbbbbb"), Some(w("root0000")), Some(TaskMode::Plan)),
            (w("a1a1a1a1"), Some(w("aaaaaaaa")), Some(TaskMode::Plan)),
            (w("a2a2a2a2"), Some(w("aaaaaaaa")), Some(TaskMode::Ask)),
        ]
    }

    #[test]
    fn revocation_follows_the_contract_table() {
        let t = tree();
        let stop = revocation(&RevocationEvent::RootStop, &t);
        assert!(stop.bump);
        assert_eq!(stop.scope, RevocationScope::Root);
        assert!(stop.keep_root_token);
        assert_eq!(stop.kill_tokens.len(), 4);
        assert!(!stop.kill_tokens.contains(&w("root0000")));
        assert_eq!(stop.stop_processes, stop.kill_tokens);

        let restrict = revocation(
            &RevocationEvent::RootRestriction {
                new_mode: Mode::Ask,
            },
            &t,
        );
        assert!(restrict.bump);
        assert!(restrict.keep_root_token);
        assert_eq!(restrict.kill_tokens, vec![w("aaaaaaaa")]);
        assert_eq!(restrict.mark_restricted, vec![w("aaaaaaaa")]);
        assert_eq!(restrict.stop_processes, vec![w("aaaaaaaa")]);
        let to_plan = revocation(
            &RevocationEvent::RootRestriction {
                new_mode: Mode::Plan,
            },
            &t,
        );
        assert_eq!(to_plan.mark_restricted, vec![w("aaaaaaaa"), w("a2a2a2a2")]);

        let widen = revocation(&RevocationEvent::RootWidening, &t);
        assert!(!widen.bump);
        assert_eq!(widen.scope, RevocationScope::None);
        assert!(widen.kill_tokens.is_empty());

        let close = revocation(&RevocationEvent::Close(w("aaaaaaaa")), &t);
        assert!(close.bump);
        assert_eq!(close.scope, RevocationScope::Subtree(w("aaaaaaaa")));
        assert_eq!(
            close.kill_tokens,
            vec![w("aaaaaaaa"), w("a1a1a1a1"), w("a2a2a2a2")]
        );
        assert_eq!(close.stop_processes, close.kill_tokens);
        assert!(close.keep_root_token);

        let exited = revocation(&RevocationEvent::GenerationExited(w("aaaaaaaa")), &t);
        assert!(exited.bump);
        assert_eq!(exited.kill_tokens, vec![w("aaaaaaaa")]);
        assert!(exited.stop_processes.is_empty());
        let root_exit = revocation(&RevocationEvent::GenerationExited(w("root0000")), &t);
        assert!(!root_exit.keep_root_token);

        let shutdown = revocation(&RevocationEvent::AppShutdown, &t);
        assert!(shutdown.bump);
        assert_eq!(shutdown.kill_tokens.len(), 5);
        assert!(!shutdown.keep_root_token);
    }
}
