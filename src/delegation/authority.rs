//! Inherited authority for supervised children.
//!
//! `judge` runs before every auto-allow path (Bypass, Accept edits, session and Always rules), and
//! `rule_may_apply` filters which remembered rules may match in a child at all. Both read the
//! supervisor's live view on every call, so a revocation or a parent restriction takes effect at
//! the next judgement.

use std::path::{Path, PathBuf};

use agent_core::adapter::Driver;
use agent_core::delegation::authority::{permits, Capability, TaskMode, WorkspaceScope};
use agent_core::delegation::SERVER_NAME;
use serde_json::Value;

use super::seam::{ApprovalGate, AuthorityParams, Effect, GateVerdict, LiveAuthority};
use crate::always_allow::Action;
use crate::approval_server::{
    canonical_target, classify, edit_targets, path_auto_matchable, read_auto_matchable, ToolClass,
};

/// Argument names agy's `call_mcp_tool` names its server by (as `always_allow::Action::of`).
const AGY_SERVER_KEYS: [&str; 4] = ["ServerName", "server_name", "server", "Server"];

/// `mcp__<server>__<tool>` as an MCP effect; `None` for any other shape.
fn mcp_prefixed(tool: &str) -> Option<Effect> {
    let (server, name) = tool.strip_prefix("mcp__")?.split_once("__")?;
    (!server.is_empty() && !name.is_empty()).then_some(Effect::Mcp {
        own: server == SERVER_NAME,
    })
}

/// agy's `call_mcp_tool`: ours only when every server key it carries names `agent_terminal` and
/// there is at least one.
fn agy_mcp(input: &Value) -> Effect {
    let named: Vec<&str> = AGY_SERVER_KEYS
        .iter()
        .filter_map(|k| input.get(*k))
        .map(|v| v.as_str().unwrap_or(""))
        .collect();
    Effect::Mcp {
        own: !named.is_empty() && named.iter().all(|s| *s == SERVER_NAME),
    }
}

/// What a provider tool call does. Anything not listed is `Unknown`.
pub(crate) fn effect(driver: Driver, tool: &str, input: &Value) -> Effect {
    match driver {
        Driver::Claude => match tool {
            "Read" | "Glob" | "Grep" | "LS" | "TodoWrite" | "AskUserQuestion" => Effect::Read,
            "Edit" | "Write" | "MultiEdit" | "NotebookEdit" => Effect::Edit,
            "Bash" | "BashOutput" | "KillShell" => Effect::Exec,
            "WebFetch" | "WebSearch" => Effect::Network,
            "Task" | "Agent" => Effect::Delegate,
            t => mcp_prefixed(t).unwrap_or(Effect::Unknown),
        },
        Driver::Codex => match tool {
            "file_change" => Effect::Edit,
            t if t == agent_core::codex::PERMISSIONS_TOOL => Effect::Edit,
            "command_execution" => Effect::Exec,
            t => mcp_prefixed(t).unwrap_or(Effect::Unknown),
        },
        Driver::Agy => match tool {
            // Before `classify`, which files these under Exec.
            "run_command" | "send_command_input" => Effect::Exec,
            "read_url_content" | "search_web" => Effect::Network,
            "start_subagent" | "invoke_subagent" | "define_subagent" | "manage_subagents"
            | "browser_subagent" => Effect::Delegate,
            "call_mcp_tool" => agy_mcp(input),
            t => match classify(t) {
                ToolClass::ReadOnly => Effect::Read,
                ToolClass::Edit => Effect::Edit,
                ToolClass::Exec | ToolClass::Unknown => Effect::Unknown,
            },
        },
    }
}

/// Every file an edit names, per provider. Empty: nothing checkable.
fn edit_paths<'a>(driver: Driver, tool: &str, input: &'a Value) -> Vec<&'a str> {
    match driver {
        Driver::Claude => ["file_path", "notebook_path"]
            .iter()
            .filter_map(|k| input.get(*k).and_then(Value::as_str))
            .collect(),
        Driver::Codex if tool == "file_change" => {
            crate::always_allow::codex_change_targets(input).unwrap_or_default()
        }
        // A permissions request names no single file: it always asks.
        Driver::Codex => Vec::new(),
        Driver::Agy => edit_targets(input),
    }
}

/// `target` made absolute: as given when absolute, else against the call's cwd (when it is
/// absolute), else against `root`. `..` is left in place for the containment check to refuse.
fn absolute(target: &str, cwd: Option<&str>, root: &Path) -> PathBuf {
    let t = Path::new(target);
    if t.is_absolute() {
        return t.to_path_buf();
    }
    match cwd.map(Path::new).filter(|c| c.is_absolute()) {
        Some(c) => c.join(t),
        None => root.join(t),
    }
}

/// The target's canonical form (symlinks resolved in its longest existing ancestor) lies in the
/// canonical `root`. Defence in depth on top of the lexical, no-symlink containment check.
fn canonically_inside(target: &Path, root: &Path) -> bool {
    match std::fs::canonicalize(root) {
        Ok(real_root) => canonical_target(target).starts_with(real_root),
        Err(_) => false,
    }
}

/// The gate a supervised child's approvals pass first.
pub(crate) struct ChildAuthority {
    params: AuthorityParams,
}

impl ChildAuthority {
    pub(crate) fn new(params: AuthorityParams) -> Self {
        Self { params }
    }

    fn live(&self) -> LiveAuthority {
        (self.params.live)()
    }

    /// The lease worktree, if this child has one.
    fn worktree(&self) -> Option<PathBuf> {
        match &self.params.inherited.workspace {
            WorkspaceScope::Lease { worktree, .. } => Some(PathBuf::from(worktree)),
            WorkspaceScope::Shared { .. } => None,
        }
    }

    /// Where a read may be matched: the lease worktree, or the shared root.
    fn read_root(&self) -> PathBuf {
        match &self.params.inherited.workspace {
            WorkspaceScope::Lease { worktree, .. } => PathBuf::from(worktree),
            WorkspaceScope::Shared { root } => PathBuf::from(root),
        }
    }

    fn home(&self) -> Option<&Path> {
        self.params.home.as_deref()
    }

    /// Whether the parent's declaration (if any) lets the child use `capability`.
    fn allows(&self, capability: Capability) -> bool {
        self.params
            .capabilities
            .as_ref()
            .is_none_or(|declared| declared.contains(&capability))
    }

    /// What a declaration leaves out of `effect`: `None` when allowed (or nothing was
    /// declared), `Some(None)` for a tool no capability names.
    fn undeclared(&self, effect: Effect) -> Option<Option<Capability>> {
        self.params.capabilities.as_ref()?;
        let needed = match effect {
            Effect::Edit => Capability::Edit,
            Effect::Exec => Capability::Exec,
            Effect::Network => Capability::Network,
            Effect::Unknown => return Some(None),
            Effect::Read | Effect::Delegate | Effect::Mcp { .. } => return None,
        };
        (!self.allows(needed)).then_some(Some(needed))
    }

    /// Whether the live authority still holds for this child: valid, not bootstrapping, and the
    /// child's task mode still inside the parent's live ceiling.
    fn holds(&self, live: &LiveAuthority) -> Result<(), String> {
        if !live.valid {
            return Err("this agent's delegated authority is no longer valid".to_owned());
        }
        if live.bootstrap {
            return Err("tools are not allowed during the startup check".to_owned());
        }
        if !permits(live.parent_mode, self.params.task_mode) {
            return Err(format!(
                "the parent's mode no longer allows a {} agent",
                self.params.task_mode.as_str()
            ));
        }
        Ok(())
    }

    /// An edit target may be auto-matched: inside the lease (no `..`, no symlink or extra hard
    /// link on the way, canonically inside too) and not a sensitive file.
    fn edit_matchable(&self, abs: &Path, worktree: &Path) -> bool {
        path_auto_matchable(abs, worktree, self.home()) && canonically_inside(abs, worktree)
    }

    fn judge_edit(&self, tool: &str, input: &Value, cwd: Option<&str>) -> GateVerdict {
        let Some(worktree) = self.worktree() else {
            return GateVerdict::AskOnly {
                reason: "this agent has no worktree of its own".to_owned(),
            };
        };
        let targets = edit_paths(self.params.driver, tool, input);
        if targets.is_empty() {
            return GateVerdict::AskOnly {
                reason: "the edit names no file that can be checked".to_owned(),
            };
        }
        for t in targets {
            let abs = absolute(t, cwd, &worktree);
            if !self.edit_matchable(&abs, &worktree) {
                let reason = if canonically_inside(&abs, &worktree) {
                    "sensitive file or link in this agent's worktree"
                } else {
                    "outside this agent's worktree"
                };
                return GateVerdict::AskOnly {
                    reason: reason.to_owned(),
                };
            }
        }
        GateVerdict::Defer
    }
}

impl ApprovalGate for ChildAuthority {
    fn judge(&self, tool: &str, input: &Value, cwd: Option<&str>) -> GateVerdict {
        if let Err(reason) = self.holds(&self.live()) {
            return GateVerdict::Deny { reason };
        }
        let effect = effect(self.params.driver, tool, input);
        // A shell command that launches an agent CLI is a nested sub-agent by another route.
        if effect == Effect::Exec
            && agent_core::agent_command::from_tool_input(tool, input).is_some()
        {
            return GateVerdict::Deny {
                reason: "delegated agents cannot start their own sub-agents".to_owned(),
            };
        }
        match effect {
            Effect::Delegate => {
                return GateVerdict::Deny {
                    reason: "delegated agents cannot start their own sub-agents".to_owned(),
                }
            }
            Effect::Mcp { own: false } => {
                return GateVerdict::Deny {
                    reason: "delegated agents use no MCP server but agent-terminal's".to_owned(),
                }
            }
            _ => {}
        }
        // A limit the parent declared is its intent: refused, never asked, so no rule and no
        // dangerous permissive mode can approve past it.
        if let Some(missing) = self.undeclared(effect) {
            return GateVerdict::Deny {
                reason: format!(
                    "this agent's parent did not allow it {}",
                    match missing {
                        Some(Capability::Edit) => "to change files",
                        Some(Capability::Exec) => "to run commands",
                        Some(Capability::Network) => "network access",
                        None => "to use tools it cannot recognise",
                    }
                ),
            };
        }
        if self.params.task_mode == TaskMode::Plan {
            return match effect {
                Effect::Read | Effect::Mcp { own: true } => GateVerdict::Defer,
                // Read-only is the parent's intent unless it declared the network.
                Effect::Network if self.params.capabilities.is_some() => GateVerdict::AskOnly {
                    reason: "network access from a plan agent".to_owned(),
                },
                Effect::Network => GateVerdict::Deny {
                    reason: "a plan agent has no network access unless its parent allows it"
                        .to_owned(),
                },
                _ => GateVerdict::Deny {
                    reason: "a plan agent cannot change files or run commands".to_owned(),
                },
            };
        }
        match effect {
            Effect::Unknown => GateVerdict::AskOnly {
                reason: "an unrecognised tool is never allowed automatically".to_owned(),
            },
            Effect::Edit => self.judge_edit(tool, input, cwd),
            _ => GateVerdict::Defer,
        }
    }

    fn rule_may_apply(&self, action: &Action, cwd: Option<&str>) -> bool {
        let live = self.live();
        if self.holds(&live).is_err() {
            return false;
        }
        match action {
            Action::Edit { path } => {
                let Some(worktree) = self.worktree() else {
                    return false;
                };
                // A plan child, or one a restricted parent no longer allows to write, never gets
                // a rule-allowed edit.
                let task = self.params.task_mode;
                if !task.writes()
                    || !permits(live.parent_mode, task)
                    || !self.allows(Capability::Edit)
                {
                    return false;
                }
                let abs = absolute(path, cwd, &worktree);
                self.edit_matchable(&abs, &worktree)
            }
            // The user's shell rules reach a writing child its parent let run commands (`judge`
            // has already refused a command that starts an agent).
            Action::Shell { .. } => {
                let task = self.params.task_mode;
                task.writes() && permits(live.parent_mode, task) && self.allows(Capability::Exec)
            }
            Action::Read { path } => {
                let root = self.read_root();
                let abs = absolute(path, cwd, &root);
                read_auto_matchable(&abs, &root, self.home()) && canonically_inside(&abs, &root)
            }
            // A child uses no MCP server but agent-terminal's, which no rule covers.
            Action::Mcp { .. } => false,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use agent_core::adapter::Mode;
    use agent_core::delegation::authority::InheritedAuthority;
    use agent_core::delegation::{AuthorityEpoch, WorkerId};
    use serde_json::json;
    use std::cell::Cell;
    use std::rc::Rc;

    const DRIVERS: [Driver; 3] = [Driver::Claude, Driver::Codex, Driver::Agy];

    fn fixed_live(valid: bool, bootstrap: bool, parent_mode: Mode) -> LiveAuthority {
        LiveAuthority {
            valid,
            bootstrap,
            parent_mode,
        }
    }

    #[test]
    fn effect_table_claude() {
        let n = json!({});
        for (tool, want) in [
            ("Read", Effect::Read),
            ("Glob", Effect::Read),
            ("Grep", Effect::Read),
            ("LS", Effect::Read),
            ("TodoWrite", Effect::Read),
            ("AskUserQuestion", Effect::Read),
            ("Edit", Effect::Edit),
            ("Write", Effect::Edit),
            ("MultiEdit", Effect::Edit),
            ("NotebookEdit", Effect::Edit),
            ("Bash", Effect::Exec),
            ("BashOutput", Effect::Exec),
            ("KillShell", Effect::Exec),
            ("WebFetch", Effect::Network),
            ("WebSearch", Effect::Network),
            ("Task", Effect::Delegate),
            ("Agent", Effect::Delegate),
            (
                "mcp__agent_terminal__cross_agent_spawn",
                Effect::Mcp { own: true },
            ),
            ("mcp__github__create_issue", Effect::Mcp { own: false }),
            ("mcp__agent_terminal__", Effect::Unknown),
            ("mcp__", Effect::Unknown),
            ("file_change", Effect::Unknown),
            ("run_command", Effect::Unknown),
            ("SomethingNew", Effect::Unknown),
        ] {
            assert_eq!(effect(Driver::Claude, tool, &n), want, "{tool}");
        }
    }

    #[test]
    fn effect_table_codex() {
        let n = json!({});
        for (tool, want) in [
            ("file_change", Effect::Edit),
            (agent_core::codex::PERMISSIONS_TOOL, Effect::Edit),
            ("command_execution", Effect::Exec),
            (
                "mcp__agent_terminal__cross_agent_read",
                Effect::Mcp { own: true },
            ),
            ("mcp__codex_apps__x", Effect::Mcp { own: false }),
            ("Read", Effect::Unknown),
            ("Bash", Effect::Unknown),
            ("web_search", Effect::Unknown),
        ] {
            assert_eq!(effect(Driver::Codex, tool, &n), want, "{tool}");
        }
    }

    #[test]
    fn effect_table_agy() {
        let n = json!({});
        for (tool, want) in [
            ("view_file", Effect::Read),
            ("list_dir", Effect::Read),
            ("grep_search", Effect::Read),
            ("find_by_name", Effect::Read),
            ("codebase_search", Effect::Read),
            ("view_code_item", Effect::Read),
            ("read_resource", Effect::Read),
            ("list_resources", Effect::Read),
            ("replace_file_content", Effect::Edit),
            ("multi_replace_file_content", Effect::Edit),
            ("write_to_file", Effect::Edit),
            ("sed_file", Effect::Edit),
            ("notebook_edit", Effect::Edit),
            ("run_command", Effect::Exec),
            ("send_command_input", Effect::Exec),
            ("read_url_content", Effect::Network),
            ("search_web", Effect::Network),
            ("start_subagent", Effect::Delegate),
            ("invoke_subagent", Effect::Delegate),
            ("define_subagent", Effect::Delegate),
            ("manage_subagents", Effect::Delegate),
            ("browser_subagent", Effect::Delegate),
            ("Bash", Effect::Unknown),
            ("mcp__agent_terminal__x", Effect::Unknown),
        ] {
            assert_eq!(effect(Driver::Agy, tool, &n), want, "{tool}");
        }
        let own = json!({"ServerName": "agent_terminal", "ToolName": "cross_agent_read"});
        assert_eq!(
            effect(Driver::Agy, "call_mcp_tool", &own),
            Effect::Mcp { own: true }
        );
        for foreign in [
            json!({"ServerName": "github"}),
            json!({}),
            json!({"ServerName": "agent_terminal", "server": "github"}),
            json!({"ServerName": 7}),
        ] {
            assert_eq!(
                effect(Driver::Agy, "call_mcp_tool", &foreign),
                Effect::Mcp { own: false },
                "{foreign}"
            );
        }
    }

    // ---- the gate ----------------------------------------------------------------------------

    struct Fixture {
        _tmp: tempfile::TempDir,
        worktree: PathBuf,
        source: PathBuf,
        live: Rc<Cell<LiveAuthority>>,
    }

    impl Fixture {
        fn new() -> Self {
            let tmp = tempfile::tempdir().expect("tempdir");
            let base = tmp.path().canonicalize().expect("canonical");
            let worktree = base.join("wt");
            let source = base.join("src");
            std::fs::create_dir_all(worktree.join("lib")).expect("mkdir");
            std::fs::create_dir_all(&source).expect("mkdir");
            std::fs::write(worktree.join("lib/a.rs"), "a").expect("write");
            std::fs::write(source.join("b.rs"), "b").expect("write");
            Self {
                _tmp: tmp,
                worktree,
                source,
                live: Rc::new(Cell::new(fixed_live(true, false, Mode::AcceptEdits))),
            }
        }

        fn gate_with(
            &self,
            driver: Driver,
            task_mode: TaskMode,
            capabilities: Option<Vec<Capability>>,
        ) -> ChildAuthority {
            let live = self.live.clone();
            ChildAuthority::new(AuthorityParams {
                inherited: InheritedAuthority {
                    worker: WorkerId::parse("w_child000").expect("id"),
                    root: WorkerId::parse("w_root0000").expect("id"),
                    parent: Some(WorkerId::parse("w_root0000").expect("id")),
                    epoch: AuthorityEpoch(1),
                    policy_revision: 1,
                    mode_ceiling: task_mode.as_mode(),
                    depth: 1,
                    workspace: WorkspaceScope::Lease {
                        source_repo: self.source.to_string_lossy().into_owned(),
                        worktree: self.worktree.to_string_lossy().into_owned(),
                        base_commit: "abc".into(),
                        allowed_relative: Vec::new(),
                    },
                },
                driver,
                task_mode,
                capabilities,
                home: None,
                live: Rc::new(move || live.get()),
            })
        }

        fn gate(&self, driver: Driver, task_mode: TaskMode) -> ChildAuthority {
            self.gate_with(driver, task_mode, None)
        }

        fn inside(&self) -> String {
            self.worktree
                .join("lib/a.rs")
                .to_string_lossy()
                .into_owned()
        }

        fn outside(&self) -> String {
            self.source.join("b.rs").to_string_lossy().into_owned()
        }
    }

    /// One call of each effect for `driver`, editing `file`.
    fn calls(driver: Driver, file: &str) -> Vec<(Effect, &'static str, Value)> {
        match driver {
            Driver::Claude => vec![
                (Effect::Read, "Read", json!({"file_path": file})),
                (Effect::Edit, "Edit", json!({"file_path": file})),
                (Effect::Exec, "Bash", json!({"command": "ls"})),
                (Effect::Network, "WebFetch", json!({"url": "x"})),
                (Effect::Unknown, "Mystery", json!({})),
            ],
            Driver::Codex => vec![
                (
                    Effect::Edit,
                    "file_change",
                    json!({"changes": [{"path": file}]}),
                ),
                (Effect::Exec, "command_execution", json!({"command": "ls"})),
                (Effect::Unknown, "Mystery", json!({})),
            ],
            Driver::Agy => vec![
                (Effect::Read, "view_file", json!({"AbsolutePath": file})),
                (Effect::Edit, "write_to_file", json!({"TargetFile": file})),
                (Effect::Exec, "run_command", json!({"CommandLine": "ls"})),
                (Effect::Network, "search_web", json!({"query": "x"})),
                (Effect::Unknown, "Mystery", json!({})),
            ],
        }
    }

    fn is_deny(v: &GateVerdict) -> bool {
        matches!(v, GateVerdict::Deny { .. })
    }

    fn is_ask_only(v: &GateVerdict) -> bool {
        matches!(v, GateVerdict::AskOnly { .. })
    }

    #[test]
    fn plan_denies_edit_exec_unknown_for_claude_codex_agy() {
        let f = Fixture::new();
        let file = f.inside();
        for driver in DRIVERS {
            let gate = f.gate(driver, TaskMode::Plan);
            for (eff, tool, input) in calls(driver, &file) {
                assert_eq!(effect(driver, tool, &input), eff, "{driver:?} {tool}");
                let v = gate.judge(tool, &input, None);
                match eff {
                    // Read-only is the parent's intent: the network too, unless declared.
                    Effect::Edit | Effect::Exec | Effect::Unknown | Effect::Network => {
                        assert!(is_deny(&v), "{driver:?} {tool}: {v:?}")
                    }
                    Effect::Read => assert_eq!(v, GateVerdict::Defer, "{driver:?} {tool}"),
                    _ => unreachable!(),
                }
            }
            let own = match driver {
                Driver::Agy => ("call_mcp_tool", json!({"ServerName": "agent_terminal"})),
                _ => ("mcp__agent_terminal__cross_agent_list", json!({})),
            };
            assert_eq!(gate.judge(own.0, &own.1, None), GateVerdict::Defer);
        }
    }

    #[test]
    fn unknown_tool_never_auto_in_ask() {
        let f = Fixture::new();
        for driver in DRIVERS {
            for mode in [TaskMode::Ask, TaskMode::AcceptEdits] {
                let v = f.gate(driver, mode).judge("Mystery", &json!({}), None);
                assert!(is_ask_only(&v), "{driver:?} {mode:?}: {v:?}");
            }
        }
    }

    #[test]
    fn a_declared_limit_is_refused_and_no_rule_reaches_past_it() {
        let f = Fixture::new();
        let file = f.inside();
        let shell = Action::Shell {
            command: "ls".into(),
        };
        let edit = Action::Edit { path: file.clone() };
        for driver in DRIVERS {
            // Edits only: commands, the network and unknown tools are refused outright.
            let edits = f.gate_with(driver, TaskMode::AcceptEdits, Some(vec![Capability::Edit]));
            for (eff, tool, input) in calls(driver, &file) {
                let v = edits.judge(tool, &input, None);
                match eff {
                    Effect::Read | Effect::Edit => {
                        assert_eq!(v, GateVerdict::Defer, "{driver:?} {tool}")
                    }
                    _ => assert!(is_deny(&v), "{driver:?} {tool}: {v:?}"),
                }
            }
            assert!(is_deny(&edits.judge("Mystery", &json!({}), None)));
            assert!(edits.rule_may_apply(&edit, None));
            assert!(!edits.rule_may_apply(&shell, None));

            // Commands only: no edit, by hand or by rule; shell rules apply.
            let exec = f.gate_with(driver, TaskMode::Ask, Some(vec![Capability::Exec]));
            for (eff, tool, input) in calls(driver, &file) {
                let v = exec.judge(tool, &input, None);
                match eff {
                    Effect::Edit | Effect::Network | Effect::Unknown => {
                        assert!(is_deny(&v), "{driver:?} {tool}: {v:?}")
                    }
                    _ => assert!(!is_deny(&v), "{driver:?} {tool}: {v:?}"),
                }
            }
            assert!(!exec.rule_may_apply(&edit, None));
            assert!(exec.rule_may_apply(&shell, None));

            // An empty declaration: reading and messaging the parent, nothing else.
            let none = f.gate_with(driver, TaskMode::Ask, Some(vec![]));
            assert!(!none.rule_may_apply(&edit, None) && !none.rule_may_apply(&shell, None));

            // A plan child the parent let reach the network is asked; never given commands.
            let plan = f.gate_with(driver, TaskMode::Plan, Some(vec![Capability::Network]));
            for (eff, tool, input) in calls(driver, &file) {
                let v = plan.judge(tool, &input, None);
                match eff {
                    Effect::Network => assert!(is_ask_only(&v), "{driver:?} {tool}: {v:?}"),
                    Effect::Read => assert_eq!(v, GateVerdict::Defer),
                    _ => assert!(is_deny(&v), "{driver:?} {tool}: {v:?}"),
                }
            }
            assert!(!f.gate(driver, TaskMode::Plan).rule_may_apply(&shell, None));
        }
        // A parent dropping to Plan takes the shell rules away again.
        let gate = f.gate(Driver::Claude, TaskMode::Ask);
        f.live.set(fixed_live(true, false, Mode::Plan));
        assert!(!gate.rule_may_apply(&shell, None));
    }

    #[test]
    fn delegate_and_foreign_mcp_always_denied() {
        let f = Fixture::new();
        for mode in TaskMode::ALL {
            let claude = f.gate(Driver::Claude, mode);
            assert!(is_deny(&claude.judge("Task", &json!({}), None)));
            assert!(is_deny(&claude.judge("mcp__github__x", &json!({}), None)));
            let agy = f.gate(Driver::Agy, mode);
            assert!(is_deny(&agy.judge("invoke_subagent", &json!({}), None)));
            assert!(is_deny(&agy.judge(
                "call_mcp_tool",
                &json!({"ServerName": "github"}),
                None
            )));
            let codex = f.gate(Driver::Codex, mode);
            assert!(is_deny(&codex.judge(
                "mcp__codex_apps__x",
                &json!({}),
                None
            )));
        }
    }

    #[test]
    fn a_shell_command_that_launches_an_agent_is_a_nested_sub_agent() {
        let f = Fixture::new();
        let launches = |driver: Driver| -> Vec<(&'static str, Value)> {
            let (tool, key) = match driver {
                Driver::Claude => ("Bash", "command"),
                Driver::Codex => ("command_execution", "command"),
                Driver::Agy => ("run_command", "CommandLine"),
            };
            [
                "cross-agent claude \"review\"",
                "cd /w && timeout 600 codex exec 'go'",
                "bash -lc 'agy -p x'",
                "FOO=1 claude -p x | tee out",
            ]
            .iter()
            .map(|c| (tool, json!({ key: c })))
            .collect()
        };
        for driver in DRIVERS {
            for mode in TaskMode::ALL {
                let gate = f.gate(driver, mode);
                for (tool, input) in launches(driver) {
                    match gate.judge(tool, &input, None) {
                        GateVerdict::Deny { reason } => assert_eq!(
                            reason, "delegated agents cannot start their own sub-agents",
                            "{driver:?} {mode:?} {input}"
                        ),
                        v => panic!("{driver:?} {mode:?} {input}: {v:?}"),
                    }
                }
            }
        }
        // Codex also sends its command as an argv.
        let gate = f.gate(Driver::Codex, TaskMode::AcceptEdits);
        let argv = json!({"command": ["bash", "-lc", "codex exec x"]});
        assert!(is_deny(&gate.judge("command_execution", &argv, None)));
    }

    #[test]
    fn ordinary_commands_and_mentions_of_agents_still_defer() {
        let f = Fixture::new();
        for driver in DRIVERS {
            let gate = f.gate(driver, TaskMode::AcceptEdits);
            let (tool, key) = match driver {
                Driver::Claude => ("Bash", "command"),
                Driver::Codex => ("command_execution", "command"),
                Driver::Agy => ("run_command", "CommandLine"),
            };
            for command in [
                "ls",
                "echo \"claude -p\"",
                "which codex",
                "claude --version",
            ] {
                assert_eq!(
                    gate.judge(tool, &json!({ key: command }), None),
                    GateVerdict::Defer,
                    "{driver:?} {command}"
                );
            }
        }
    }

    #[test]
    fn bootstrap_and_invalid_epoch_deny_everything() {
        let f = Fixture::new();
        let file = f.inside();
        for live in [
            fixed_live(true, true, Mode::AcceptEdits),
            fixed_live(false, false, Mode::AcceptEdits),
        ] {
            f.live.set(live);
            for driver in DRIVERS {
                let gate = f.gate(driver, TaskMode::AcceptEdits);
                for (_, tool, input) in calls(driver, &file) {
                    assert!(
                        is_deny(&gate.judge(tool, &input, None)),
                        "{driver:?} {tool}"
                    );
                }
                let read = Action::Read { path: file.clone() };
                let edit = Action::Edit { path: file.clone() };
                assert!(!gate.rule_may_apply(&read, None));
                assert!(!gate.rule_may_apply(&edit, None));
            }
        }
    }

    #[test]
    fn the_ceiling_follows_the_parent_mode() {
        let f = Fixture::new();
        let file = f.inside();
        let edit = json!({"file_path": file});
        let gate = f.gate(Driver::Claude, TaskMode::AcceptEdits);
        assert_eq!(gate.judge("Edit", &edit, None), GateVerdict::Defer);
        assert!(gate.rule_may_apply(&Action::Edit { path: file.clone() }, None));
        // The parent drops to Ask: an accept_edits child is outside its ceiling now.
        f.live.set(fixed_live(true, false, Mode::Ask));
        assert!(is_deny(&gate.judge("Edit", &edit, None)));
        assert!(!gate.rule_may_apply(&Action::Edit { path: file.clone() }, None));
        // Bypass never propagates: it is Ask.
        f.live.set(fixed_live(true, false, Mode::Bypass));
        assert!(is_deny(&gate.judge("Edit", &edit, None)));
        // An ask child stays inside Ask's ceiling, but not Plan's.
        let ask = f.gate(Driver::Claude, TaskMode::Ask);
        assert_eq!(ask.judge("Edit", &edit, None), GateVerdict::Defer);
        f.live.set(fixed_live(true, false, Mode::Plan));
        assert!(is_deny(&ask.judge(
            "Read",
            &json!({"file_path": file}),
            None
        )));
        // A plan child stays inside every ceiling.
        let plan = f.gate(Driver::Claude, TaskMode::Plan);
        assert_eq!(
            plan.judge("Read", &json!({"file_path": file}), None),
            GateVerdict::Defer
        );
    }

    #[test]
    fn edits_outside_the_lease_or_sensitive_ask_without_grants() {
        let f = Fixture::new();
        let gate = f.gate(Driver::Claude, TaskMode::AcceptEdits);
        let outside = gate.judge("Write", &json!({"file_path": f.outside()}), None);
        assert!(is_ask_only(&outside), "{outside:?}");
        std::fs::create_dir_all(f.worktree.join(".git")).expect("mkdir");
        let sensitive = f
            .worktree
            .join(".git/config")
            .to_string_lossy()
            .into_owned();
        assert!(is_ask_only(&gate.judge(
            "Edit",
            &json!({"file_path": sensitive}),
            None
        )));
        // A relative path resolves against the call's cwd.
        let wt = f.worktree.to_string_lossy().into_owned();
        assert_eq!(
            gate.judge("Edit", &json!({"file_path": "lib/a.rs"}), Some(&wt)),
            GateVerdict::Defer
        );
        assert!(is_ask_only(&gate.judge(
            "Edit",
            &json!({"file_path": "../src/b.rs"}),
            Some(&wt)
        )));
        // Codex: every target of a patch, a rename's destination included.
        let codex = f.gate(Driver::Codex, TaskMode::AcceptEdits);
        let rename = json!({"changes": [{"path": f.inside(), "kind": {"move_path": f.outside()}}]});
        assert!(is_ask_only(&codex.judge("file_change", &rename, None)));
        assert!(is_ask_only(&codex.judge(
            agent_core::codex::PERMISSIONS_TOOL,
            &json!({}),
            None
        )));
        // agy: every target key is judged.
        let agy = f.gate(Driver::Agy, TaskMode::AcceptEdits);
        let two = json!({"TargetFile": f.inside(), "path": f.outside()});
        assert!(is_ask_only(&agy.judge("write_to_file", &two, None)));
        assert!(is_ask_only(&agy.judge("write_to_file", &json!({}), None)));
    }

    #[test]
    fn rule_may_apply_only_inside_the_lease() {
        let f = Fixture::new();
        for driver in DRIVERS {
            let gate = f.gate(driver, TaskMode::AcceptEdits);
            // Inside the lease: yes.
            assert!(gate.rule_may_apply(&Action::Edit { path: f.inside() }, None));
            assert!(gate.rule_may_apply(&Action::Read { path: f.inside() }, None));
            // The source tree: no.
            assert!(!gate.rule_may_apply(&Action::Edit { path: f.outside() }, None));
            assert!(!gate.rule_may_apply(&Action::Read { path: f.outside() }, None));
            // The user's shell rules reach a writing child; MCP rules never.
            assert!(gate.rule_may_apply(
                &Action::Shell {
                    command: "ls".into()
                },
                None
            ));
            assert!(!gate.rule_may_apply(
                &Action::Mcp {
                    server: "agent_terminal".into(),
                    tool: "cross_agent_read".into()
                },
                None
            ));
        }
    }

    #[test]
    fn rule_may_apply_refuses_a_symlink_escape_and_sensitive_files() {
        let f = Fixture::new();
        let gate = f.gate(Driver::Claude, TaskMode::AcceptEdits);
        // A directory link inside the worktree that points at the source tree.
        let link = f.worktree.join("escape");
        std::os::unix::fs::symlink(&f.source, &link).expect("symlink");
        let through = link.join("b.rs").to_string_lossy().into_owned();
        assert!(!gate.rule_may_apply(
            &Action::Edit {
                path: through.clone()
            },
            None
        ));
        assert!(!gate.rule_may_apply(
            &Action::Read {
                path: through.clone()
            },
            None
        ));
        assert!(is_ask_only(&gate.judge(
            "Edit",
            &json!({"file_path": through}),
            None
        )));
        // A file link inside the worktree, too.
        let file_link = f.worktree.join("lib/b.rs");
        std::os::unix::fs::symlink(f.source.join("b.rs"), &file_link).expect("symlink");
        let p = file_link.to_string_lossy().into_owned();
        assert!(!gate.rule_may_apply(&Action::Edit { path: p }, None));
        // Sensitive files inside the lease: no edit rule.
        for rel in [
            ".git/config",
            ".claude/settings.json",
            "AGENTS.md",
            ".envrc",
        ] {
            let p = f.worktree.join(rel).to_string_lossy().into_owned();
            assert!(
                !gate.rule_may_apply(&Action::Edit { path: p }, None),
                "{rel}"
            );
        }
        // A secret-looking file: no read rule.
        let env = f.worktree.join(".env").to_string_lossy().into_owned();
        assert!(!gate.rule_may_apply(&Action::Read { path: env }, None));
    }

    #[test]
    fn a_child_home_inside_its_lease_keeps_dotfiles_out_of_automatic_grants() {
        let f = Fixture::new();
        let child_home = f.worktree.join("private-home");
        std::fs::create_dir_all(child_home.join(".config/autostart")).expect("home");
        std::fs::create_dir_all(child_home.join("projects")).expect("projects");
        for driver in DRIVERS {
            let mut gate = f.gate(driver, TaskMode::AcceptEdits);
            gate.params.home = Some(child_home.clone());
            for relative in [".bashrc", ".config/autostart/start.desktop"] {
                let path = child_home.join(relative).to_string_lossy().into_owned();
                let tool = match driver {
                    Driver::Claude => "Write",
                    Driver::Codex => "file_change",
                    Driver::Agy => "write_to_file",
                };
                let input = match driver {
                    Driver::Claude => json!({"file_path":path}),
                    Driver::Codex => json!({"changes":[{"path":path}]}),
                    Driver::Agy => json!({"TargetFile":path}),
                };
                assert!(
                    is_ask_only(&gate.judge(tool, &input, None)),
                    "{driver:?}: {relative}"
                );
                assert!(!gate.rule_may_apply(&Action::Edit { path }, None));
            }
            let ordinary = child_home
                .join("projects/example.rs")
                .to_string_lossy()
                .into_owned();
            assert!(gate.rule_may_apply(&Action::Edit { path: ordinary }, None));
        }
    }

    #[test]
    fn restricted_parent_and_plan_child_get_no_edit_rules() {
        let f = Fixture::new();
        let plan = f.gate(Driver::Claude, TaskMode::Plan);
        assert!(!plan.rule_may_apply(&Action::Edit { path: f.inside() }, None));
        assert!(plan.rule_may_apply(&Action::Read { path: f.inside() }, None));
        let ae = f.gate(Driver::Codex, TaskMode::AcceptEdits);
        f.live.set(fixed_live(true, false, Mode::Ask));
        assert!(!ae.rule_may_apply(&Action::Edit { path: f.inside() }, None));
        let ask = f.gate(Driver::Codex, TaskMode::Ask);
        assert!(ask.rule_may_apply(&Action::Edit { path: f.inside() }, None));
    }

    #[test]
    fn a_shared_child_reads_its_root_and_edits_nothing() {
        let f = Fixture::new();
        let live = f.live.clone();
        let gate = ChildAuthority::new(AuthorityParams {
            inherited: InheritedAuthority {
                worker: WorkerId::parse("w_child000").expect("id"),
                root: WorkerId::parse("w_root0000").expect("id"),
                parent: None,
                epoch: AuthorityEpoch(1),
                policy_revision: 1,
                mode_ceiling: Mode::Plan,
                depth: 1,
                workspace: WorkspaceScope::Shared {
                    root: f.source.to_string_lossy().into_owned(),
                },
            },
            driver: Driver::Claude,
            task_mode: TaskMode::Plan,
            capabilities: None,
            home: None,
            live: Rc::new(move || live.get()),
        });
        assert!(gate.rule_may_apply(&Action::Read { path: f.outside() }, None));
        assert!(!gate.rule_may_apply(&Action::Read { path: f.inside() }, None));
        assert!(!gate.rule_may_apply(&Action::Edit { path: f.outside() }, None));
    }
}
