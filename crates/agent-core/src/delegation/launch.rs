//! The per-provider launch mapping (contract §5): bridge env, the Claude overlay and
//! arguments, the Codex thread config, and the child read-back checks.
//!
//! The token appears only under a server's `env`, never in argv, and a
//! [`DelegationBinding`] is never serialised.

use serde_json::{json, Map, Value};

use super::authority::TaskMode;
use super::env::{ENV_APP_PID, ENV_CAP, ENV_ENDPOINT, ENV_INSTANCE};
use super::ids::{ProcessGeneration, WorkerId};
use super::token::{CapabilityToken, InstanceNonce};
use super::{BRIDGE_ARG, SERVER_NAME};

/// What one process generation of a principal needs to reach the supervisor. `Debug` hides
/// the token; there is deliberately no `Serialize`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DelegationBinding {
    /// The absolute installed path of `agent-terminal`.
    pub command: String,
    /// The absolute socket path.
    pub endpoint: String,
    pub instance: InstanceNonce,
    pub token: CapabilityToken,
}

/// Codex's tool timeout for our server, above both bridge timeouts (contract §4).
pub const CODEX_TOOL_TIMEOUT_SEC: u64 = 60;

/// Built-in Codex servers seen in every fixture; a child disables them.
pub const CODEX_BUILTIN_SERVERS: &[&str] = &["codex_apps"];

/// Claude child `--tools` for `plan`.
pub const CLAUDE_CHILD_TOOLS_PLAN: &[&str] = &["Read", "Glob", "Grep"];
/// Claude child `--tools` for `ask` and `accept_edits` (G-C1 decides `Bash`).
pub const CLAUDE_CHILD_TOOLS_WRITE: &[&str] = &[
    "Read",
    "Glob",
    "Grep",
    "Edit",
    "Write",
    "NotebookEdit",
    "Bash",
];

/// The prefix of our tools as Claude names them.
const CLAUDE_OWN_TOOL_PREFIX: &str = "mcp__agent_terminal__";

impl DelegationBinding {
    /// `ENDPOINT`, `INSTANCE`, `CAP` (the secret) and `APP_PID`, in that order.
    pub fn env(&self, app_pid: u32) -> [(&'static str, String); 4] {
        [
            (ENV_ENDPOINT, self.endpoint.clone()),
            (ENV_INSTANCE, self.instance.as_str().to_owned()),
            (ENV_CAP, self.token.expose_secret().to_owned()),
            (ENV_APP_PID, app_pid.to_string()),
        ]
    }

    fn env_object(&self, app_pid: u32) -> Value {
        Value::Object(
            self.env(app_pid)
                .into_iter()
                .map(|(k, v)| (k.to_owned(), Value::String(v)))
                .collect(),
        )
    }

    /// The Claude `--mcp-config` overlay:
    /// `{"mcpServers":{"agent_terminal":{"type":"stdio","command":…,"args":["--cross-agent-mcp"],"env":{…}}}}`.
    /// The bridge reads `APP_PID` from this `env` (the provider's own env never has it), so
    /// it takes the app pid like [`DelegationBinding::env`].
    pub fn claude_overlay(&self, app_pid: u32) -> Value {
        json!({
            "mcpServers": {
                SERVER_NAME: {
                    "type": "stdio",
                    "command": self.command,
                    "args": [BRIDGE_ARG],
                    "env": self.env_object(app_pid)
                }
            }
        })
    }

    /// The Codex `thread/start` / `thread/resume` config: exactly the dotted keys
    /// `mcp_servers.agent_terminal.{command,args,env,default_tools_approval_mode,tool_timeout_sec}`.
    pub fn codex_thread_config(&self, app_pid: u32) -> Map<String, Value> {
        let key = |field: &str| format!("mcp_servers.{SERVER_NAME}.{field}");
        let mut m = Map::new();
        m.insert(key("command"), Value::String(self.command.clone()));
        m.insert(key("args"), json!([BRIDGE_ARG]));
        m.insert(key("env"), self.env_object(app_pid));
        m.insert(key("default_tools_approval_mode"), json!("approve"));
        m.insert(key("tool_timeout_sec"), json!(CODEX_TOOL_TIMEOUT_SEC));
        m
    }

    /// A child: the above plus `mcp_servers.<name>.enabled = false` for every name in
    /// `disable` (G-X1's first mechanism). Our own server is never disabled.
    pub fn codex_child_config(&self, app_pid: u32, disable: &[String]) -> Map<String, Value> {
        let mut m = self.codex_thread_config(app_pid);
        for name in disable.iter().filter(|n| n.as_str() != SERVER_NAME) {
            m.insert(format!("mcp_servers.{name}.enabled"), Value::Bool(false));
        }
        m
    }
}

fn child_tools(mode: TaskMode) -> &'static [&'static str] {
    match mode {
        TaskMode::Plan => CLAUDE_CHILD_TOOLS_PLAN,
        TaskMode::Ask | TaskMode::AcceptEdits => CLAUDE_CHILD_TOOLS_WRITE,
    }
}

/// `--restricted --strict-mcp-config --permission-prompt-tool stdio --permission-mode default
/// --mcp-config <overlay> --tools <list>`.
pub fn claude_child_args(mode: TaskMode, overlay_path: &str) -> Vec<String> {
    [
        "--restricted",
        "--strict-mcp-config",
        "--permission-prompt-tool",
        "stdio",
        "--permission-mode",
        "default",
        "--mcp-config",
        overlay_path,
        "--tools",
        &child_tools(mode).join(","),
    ]
    .iter()
    .map(|s| (*s).to_owned())
    .collect()
}

/// A root: only `--mcp-config <overlay>` (no `--strict-mcp-config`).
pub fn claude_parent_args(overlay_path: &str) -> Vec<String> {
    vec!["--mcp-config".to_owned(), overlay_path.to_owned()]
}

/// Claude asks `can_use_tool` for our server with `source: "dynamic"`: the host allows it and
/// the supervisor is the gate.
pub fn is_own_bridge_call(server_name: &str, source: Option<&str>) -> bool {
    server_name == SERVER_NAME && source == Some("dynamic")
}

/// Claude child read-back: `system/init.tools` is exactly the `--tools` list plus our own
/// `mcp__agent_terminal__*`, and `mcp_servers` exactly `[agent_terminal]`. `false` → stop
/// the child before its first turn.
pub fn claude_child_init_ok(
    mode: TaskMode,
    init_tools: &[String],
    init_mcp_servers: &[String],
) -> bool {
    let expected = child_tools(mode);
    let tools_ok = init_tools
        .iter()
        .all(|t| expected.contains(&t.as_str()) || t.starts_with(CLAUDE_OWN_TOOL_PREFIX))
        && expected.iter().all(|e| init_tools.iter().any(|t| t == e));
    let servers_ok = init_mcp_servers.len() == 1 && init_mcp_servers[0] == SERVER_NAME;
    tools_ok && servers_ok
}

/// Codex child read-back: the started servers are exactly `[agent_terminal]`.
pub fn codex_child_servers_ok(started: &[String]) -> bool {
    started.len() == 1 && started[0] == SERVER_NAME
}

/// `mcp-<app-pid>-<worker>-<gen>.json`.
pub fn overlay_file_name(app_pid: u32, worker: &WorkerId, generation: ProcessGeneration) -> String {
    format!("mcp-{app_pid}-{worker}-{}.json", generation.0)
}

/// The app pid of an overlay file name, or `None` for any other name.
pub fn overlay_sweep_candidate(name: &str) -> Option<u32> {
    let rest = name.strip_prefix("mcp-")?.strip_suffix(".json")?;
    let (pid, rest) = rest.split_once('-')?;
    if pid.is_empty() || !pid.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    let (worker, generation) = rest.rsplit_once('-')?;
    WorkerId::parse(worker).ok()?;
    if generation.is_empty() || !generation.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    generation.parse::<u64>().ok()?;
    pid.parse().ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn binding() -> DelegationBinding {
        DelegationBinding {
            command: "/usr/bin/agent-terminal".into(),
            endpoint: "/run/user/1000/agent-terminal/delegation-42.sock".into(),
            instance: InstanceNonce::from_bytes([0xab; 16]),
            token: CapabilityToken::from_random([5; 32]),
        }
    }

    fn secret() -> String {
        binding().token.expose_secret().to_owned()
    }

    /// Where in `v` the token appears, as JSON pointer-ish paths.
    fn token_paths(v: &Value, path: &str, out: &mut Vec<String>) {
        match v {
            Value::String(s) if s.contains(&secret()) => out.push(path.to_owned()),
            Value::Object(m) => {
                for (k, child) in m {
                    if k.contains(&secret()) {
                        out.push(format!("{path}/<key>"));
                    }
                    token_paths(child, &format!("{path}/{k}"), out);
                }
            }
            Value::Array(items) => {
                for (i, child) in items.iter().enumerate() {
                    token_paths(child, &format!("{path}/{i}"), out);
                }
            }
            _ => {}
        }
    }

    #[test]
    fn env_has_four_pairs_and_the_strip_list_five_names() {
        let env = binding().env(4242);
        assert_eq!(env.len(), 4);
        assert_eq!(env[2], (ENV_CAP, secret()));
        assert_eq!(env[3], (ENV_APP_PID, "4242".to_owned()));
        assert_eq!(crate::delegation::env::STRIP_ON_SPAWN.len(), 5);
        assert!(crate::delegation::env::STRIP_ON_SPAWN.contains(&ENV_APP_PID));
    }

    #[test]
    fn claude_overlay_puts_the_token_only_under_env() {
        let o = binding().claude_overlay(7);
        let server = &o["mcpServers"]["agent_terminal"];
        assert_eq!(server["type"], "stdio");
        assert_eq!(server["args"], json!(["--cross-agent-mcp"]));
        assert_eq!(server["env"][ENV_APP_PID], "7");
        let mut paths = Vec::new();
        token_paths(&o, "", &mut paths);
        assert_eq!(
            paths,
            vec![format!("/mcpServers/agent_terminal/env/{ENV_CAP}")]
        );
    }

    #[test]
    fn codex_config_has_exactly_the_documented_keys() {
        let c = binding().codex_thread_config(7);
        let keys: Vec<&str> = c.keys().map(String::as_str).collect();
        let mut expected = vec![
            "mcp_servers.agent_terminal.args",
            "mcp_servers.agent_terminal.command",
            "mcp_servers.agent_terminal.default_tools_approval_mode",
            "mcp_servers.agent_terminal.env",
            "mcp_servers.agent_terminal.tool_timeout_sec",
        ];
        let mut sorted = keys.clone();
        sorted.sort_unstable();
        expected.sort_unstable();
        assert_eq!(sorted, expected);
        assert_eq!(
            c["mcp_servers.agent_terminal.default_tools_approval_mode"],
            "approve"
        );
        assert_eq!(c["mcp_servers.agent_terminal.tool_timeout_sec"], 60);
        let mut paths = Vec::new();
        token_paths(&Value::Object(c.clone()), "", &mut paths);
        assert_eq!(
            paths,
            vec![format!("/mcp_servers.agent_terminal.env/{ENV_CAP}")]
        );
        let child = binding().codex_child_config(7, &["x".into(), "codex_apps".into()]);
        assert_eq!(child.len(), c.len() + 2);
        assert_eq!(child["mcp_servers.x.enabled"], false);
        assert_eq!(child["mcp_servers.codex_apps.enabled"], false);
        let own = binding().codex_child_config(7, &["agent_terminal".into()]);
        assert_eq!(own.len(), c.len());
    }

    #[test]
    fn binding_debug_is_redacted() {
        let dbg = format!("{:?}", binding());
        assert!(!dbg.contains(&secret()[5..]));
        assert!(dbg.contains("atdc_****"));
    }

    #[test]
    fn claude_child_args_follow_the_mode() {
        let plan = claude_child_args(TaskMode::Plan, "/run/o.json");
        assert!(plan.contains(&"--restricted".to_owned()));
        assert!(plan.contains(&"--strict-mcp-config".to_owned()));
        let tools = plan.last().cloned().unwrap_or_default();
        assert_eq!(tools, "Read,Glob,Grep");
        for forbidden in ["Edit", "Write", "Bash"] {
            assert!(!tools.split(',').any(|t| t == forbidden));
        }
        let write = claude_child_args(TaskMode::AcceptEdits, "/run/o.json");
        assert!(write.last().is_some_and(|t| t.contains("Bash")));
        assert_eq!(
            claude_parent_args("/run/o.json"),
            vec!["--mcp-config".to_owned(), "/run/o.json".to_owned()]
        );
        for args in [plan, write] {
            assert!(!args.iter().any(|a| a.contains("atdc_")));
            assert!(!args
                .iter()
                .any(|a| a.contains("Task") || a.contains("Agent")));
        }
        assert!(is_own_bridge_call("agent_terminal", Some("dynamic")));
        assert!(!is_own_bridge_call("agent_terminal", None));
        assert!(!is_own_bridge_call("other", Some("dynamic")));
    }

    #[test]
    fn child_read_backs_refuse_extras() {
        let s = |v: &[&str]| v.iter().map(|x| (*x).to_owned()).collect::<Vec<_>>();
        let mut tools = s(CLAUDE_CHILD_TOOLS_PLAN);
        tools.push("mcp__agent_terminal__cross_agent_read".into());
        let servers = s(&["agent_terminal"]);
        assert!(claude_child_init_ok(TaskMode::Plan, &tools, &servers));
        for extra in ["Task", "Agent", "Bash"] {
            let mut t = tools.clone();
            t.push(extra.into());
            assert!(
                !claude_child_init_ok(TaskMode::Plan, &t, &servers),
                "{extra}"
            );
        }
        assert!(!claude_child_init_ok(
            TaskMode::Plan,
            &tools,
            &s(&["agent_terminal", "other"])
        ));
        assert!(!claude_child_init_ok(TaskMode::Plan, &tools[1..], &servers));
        assert!(!claude_child_init_ok(TaskMode::Plan, &tools, &[]));
        assert!(codex_child_servers_ok(&s(&["agent_terminal"])));
        assert!(!codex_child_servers_ok(&s(&[
            "agent_terminal",
            "codex_apps"
        ])));
        assert!(!codex_child_servers_ok(&s(&["codex_apps"])));
        assert!(!codex_child_servers_ok(&[]));
    }

    #[test]
    fn overlay_names_round_trip() {
        let w = WorkerId::parse("w_abcdefgh").expect("valid");
        let name = overlay_file_name(4242, &w, ProcessGeneration(3));
        assert_eq!(name, "mcp-4242-w_abcdefgh-3.json");
        assert_eq!(overlay_sweep_candidate(&name), Some(4242));
        for foreign in [
            "mcp.json",
            "mcp-x-w_abcdefgh-3.json",
            "mcp-4242-w_abc-3.json",
            "mcp-4242-w_abcdefgh-.json",
            "mcp-4242-w_abcdefgh-3.txt",
            "other-4242-w_abcdefgh-3.json",
        ] {
            assert_eq!(overlay_sweep_candidate(foreign), None, "{foreign}");
        }
    }
}
