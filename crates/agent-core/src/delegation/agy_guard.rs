//! The pure parts of the agy capability-leak mitigation. The app does the I/O
//! (directory scans, `/proc` reads, `SO_PEERCRED`) and passes the results in.

use std::path::{Path, PathBuf};

use super::tools::ToolName;
use super::BRIDGE_ARG;

/// Environment variables that redirect agy's configuration; set even to blank, they withhold
/// delegation.
pub const CONFIG_REDIRECT_VARS: [&str; 3] = [
    "ANTIGRAVITY_APP_DATA_DIR",
    "JETSKI_APP_DATA_DIR",
    "CASCADE_GLOBAL_CONFIG_OVERRIDE",
];
/// Any variable with this prefix withholds delegation too (its effect is unverified).
pub const REDIRECT_VAR_PREFIX: &str = "CASCADE_";
/// agy flags that move its configuration directory (`-`/`_` alike, one or two dashes).
pub const CONFIG_DIR_FLAGS: [&str; 2] = ["gemini_dir", "app_data_dir"];
/// Names agy opened or probed in a workspace (`tests/fixtures/real/delegation/agy/d2-config-probes.txt`).
pub const WORKSPACE_SOURCE_NAMES: [&str; 7] = [
    "mcp_config.json",
    "hooks.json",
    "sidecars.json",
    "sidecars",
    "plugin.json",
    "plugins.json",
    "plugins.txt",
];
/// The 8 exact `permissions.allow` rules a hookless agy needs, one per tool in
/// [`ToolName::ALL`] order.
pub const ALLOW_RULES: [&str; 8] = [
    "mcp(agent_terminal/cross_agent_capabilities)",
    "mcp(agent_terminal/cross_agent_spawn)",
    "mcp(agent_terminal/cross_agent_send)",
    "mcp(agent_terminal/cross_agent_read)",
    "mcp(agent_terminal/cross_agent_wait)",
    "mcp(agent_terminal/cross_agent_list)",
    "mcp(agent_terminal/cross_agent_cancel)",
    "mcp(agent_terminal/cross_agent_close)",
];

/// The allow rule for one tool.
pub fn allow_rule(tool: ToolName) -> String {
    format!("mcp({}/{})", super::SERVER_NAME, tool.as_str())
}

/// Why the file-system guard withheld delegation. Names and relative paths only, never values.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum L2Refusal {
    ArgvRedirect(String),
    EnvRedirect(String),
    HomeMismatch,
    AllowRulesMissing(Vec<String>),
    WorkspaceSource(String),
    Unreadable(String),
    ScanLimit,
    DiscoveryFailed,
}

/// The config-directory flag `arg` names, if any: one or two leading dashes, `-` and `_`
/// alike, the value inline after `=` or in the next argument.
fn config_dir_flag(arg: &str) -> Option<&'static str> {
    let body = arg.strip_prefix("--").or_else(|| arg.strip_prefix('-'))?;
    if body.starts_with('-') {
        return None;
    }
    let name = body.split_once('=').map_or(body, |(n, _)| n);
    let normalised = name.replace('-', "_");
    CONFIG_DIR_FLAGS.into_iter().find(|f| *f == normalised)
}

/// The first config-directory flag in the final argv (after `--` too), as `--<name>`. Same
/// classification as the agy adapter's (PR #44); defence in depth.
pub fn argv_redirect(argv: &[String]) -> Option<String> {
    argv.iter()
        .find_map(|a| config_dir_flag(a))
        .map(|f| format!("--{f}"))
}

/// The first redirecting variable name in the final environment; a name only.
pub fn env_redirect(env: &[(String, String)]) -> Option<String> {
    env.iter()
        .map(|(name, _)| name)
        .find(|name| {
            CONFIG_REDIRECT_VARS.contains(&name.as_str()) || name.starts_with(REDIRECT_VAR_PREFIX)
        })
        .cloned()
}

/// The directories L2 searches. Git: the physical cwd up to and including the toplevel.
/// Non-git (`toplevel` is `None`), or a toplevel that is not an ancestor of the cwd: the cwd
/// and every ancestor up to `/`. Pure path math on physical paths.
pub fn l2_directories(physical_cwd: &Path, toplevel: Option<&Path>) -> Vec<PathBuf> {
    let all: Vec<PathBuf> = physical_cwd.ancestors().map(Path::to_path_buf).collect();
    match toplevel {
        Some(top) => match all.iter().position(|d| d == top) {
            Some(i) => all[..=i].to_vec(),
            None => all,
        },
        None => all,
    }
}

/// The allow rules missing from agy's `settings.json` (`permissions.allow`). Unparsable JSON
/// is a refusal.
pub fn allow_rules_missing(settings_json: &str) -> Result<Vec<&'static str>, L2Refusal> {
    let v: serde_json::Value = serde_json::from_str(settings_json)
        .map_err(|_| L2Refusal::Unreadable("antigravity-cli/settings.json".into()))?;
    let present: Vec<&str> = v
        .get("permissions")
        .and_then(|p| p.get("allow"))
        .and_then(|a| a.as_array())
        .map(|a| a.iter().filter_map(|r| r.as_str()).collect())
        .unwrap_or_default();
    Ok(ALLOW_RULES
        .into_iter()
        .filter(|r| !present.contains(r))
        .collect())
}

/// The ppid in `/proc/<pid>/stat`, parsed after the LAST `)` (comm may hold spaces and
/// parentheses): the second field after it.
pub fn stat_ppid(stat: &str) -> Option<u32> {
    let (_, after) = stat.rsplit_once(')')?;
    let mut fields = after.split_whitespace();
    let _state = fields.next()?;
    fields.next()?.parse().ok()
}

/// `/proc/<pid>/cmdline`: the NUL-separated argv after argv[0] is exactly
/// `["--cross-agent-mcp"]` (a trailing NUL is optional). The approval hook
/// (`--approval-hook`, same executable) is never a bridge.
pub fn is_bridge_cmdline(cmdline: &[u8]) -> bool {
    let trimmed = cmdline.strip_suffix(b"\0").unwrap_or(cmdline);
    let mut fields = trimmed.split(|b| *b == 0);
    let argv0_ok = fields.next().is_some_and(|a| !a.is_empty());
    argv0_ok && fields.next() == Some(BRIDGE_ARG.as_bytes()) && fields.next().is_none()
}

/// Lineage self-check, bridge side: the bridge's parent's ppid (from the parent's
/// `stat`) equals `AGENT_TERMINAL_DELEGATION_APP_PID`.
pub fn lineage_ok(parent_stat: &str, app_pid: &str) -> bool {
    match (stat_ppid(parent_stat), app_pid.trim().parse::<u32>()) {
        (Some(ppid), Ok(app)) => ppid == app,
        _ => false,
    }
}

/// What the supervisor does with an agy hello.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HelloVerdict {
    Accept,
    /// Fails L3 (not agy's direct child, or not a bridge): a notice only; the token stays
    /// unconsumed.
    RefuseNoConsume,
    /// The same pid after its earlier connection closed: a lost IPC → `rebind_required`.
    RefuseLostIpc,
    /// A second bridge that passes L3 → revoke the generation.
    Compromise,
}

/// Pure decision from the peer facts, the token's state and any earlier peer.
pub fn hello_verdict(
    peer_is_direct_child: bool,
    exe_matches: bool,
    bridge_argv: bool,
    token_consumed_by: Option<u32>,
    peer_pid: u32,
    earlier_live: bool,
) -> HelloVerdict {
    if !(peer_is_direct_child && exe_matches && bridge_argv) {
        return HelloVerdict::RefuseNoConsume;
    }
    match token_consumed_by {
        None => HelloVerdict::Accept,
        Some(pid) if pid == peer_pid && !earlier_live => HelloVerdict::RefuseLostIpc,
        // One live connection per token: the same peer saying hello again concurrently is
        // refused without consequence.
        Some(pid) if pid == peer_pid => HelloVerdict::RefuseNoConsume,
        Some(_) => HelloVerdict::Compromise,
    }
}

/// Census: exactly one live (direct child, exe match, bridge argv) process, and it is the
/// connected peer.
pub fn census_ok(bridges: &[u32], peer_pid: u32) -> bool {
    bridges == [peer_pid]
}

#[cfg(test)]
mod tests {
    use super::*;

    fn argv(v: &[&str]) -> Vec<String> {
        v.iter().map(|s| (*s).to_owned()).collect()
    }

    #[test]
    fn allow_rules_match_the_tools() {
        let built: Vec<String> = ToolName::ALL.into_iter().map(allow_rule).collect();
        assert_eq!(built, ALLOW_RULES.to_vec());
        assert!(WORKSPACE_SOURCE_NAMES.contains(&"sidecars.json"));
        assert!(WORKSPACE_SOURCE_NAMES.contains(&"sidecars"));
    }

    #[test]
    fn argv_redirect_matches_the_pr44_classification() {
        for hit in [
            &["agy", "--gemini_dir", "/x"][..],
            &["agy", "--gemini-dir=/x"],
            &["agy", "-gemini_dir", "/x"],
            &["agy", "-app-data-dir=/x"],
            &["agy", "--app_data_dir"],
            &["agy", "--", "--gemini_dir=/x"],
        ] {
            assert!(argv_redirect(&argv(hit)).is_some(), "{hit:?}");
        }
        assert_eq!(
            argv_redirect(&argv(&["agy", "--gemini-dir", "/x"])),
            Some("--gemini_dir".into())
        );
        for miss in [
            &["agy", "--gemini_dirt"][..],
            &["agy", "--sandbox=gemini_dir"],
            &["agy", "gemini_dir"],
            &["agy", "---gemini_dir"],
            &["agy", "--model", "gemini-3"],
        ] {
            assert_eq!(argv_redirect(&argv(miss)), None, "{miss:?}");
        }
    }

    #[test]
    fn env_redirect_reports_names_only() {
        for name in CONFIG_REDIRECT_VARS {
            let env = vec![
                ("HOME".to_owned(), "/h".to_owned()),
                (name.to_owned(), String::new()),
            ];
            assert_eq!(env_redirect(&env), Some(name.to_owned()));
        }
        let env = vec![(
            "CASCADE_MCP_SERVER_INIT".to_owned(),
            "secret-value".to_owned(),
        )];
        let found = env_redirect(&env);
        assert_eq!(found.as_deref(), Some("CASCADE_MCP_SERVER_INIT"));
        assert!(!found.unwrap_or_default().contains("secret-value"));
        assert_eq!(
            env_redirect(&[("PATH".to_owned(), "/bin".to_owned())]),
            None
        );
    }

    #[test]
    fn l2_directories_stop_at_the_toplevel_or_reach_root() {
        let git = l2_directories(Path::new("/r/a/b"), Some(Path::new("/r")));
        assert_eq!(
            git,
            vec![
                PathBuf::from("/r/a/b"),
                PathBuf::from("/r/a"),
                PathBuf::from("/r")
            ]
        );
        let non_git = l2_directories(Path::new("/r/a"), None);
        assert_eq!(
            non_git,
            vec![
                PathBuf::from("/r/a"),
                PathBuf::from("/r"),
                PathBuf::from("/")
            ]
        );
        let foreign_top = l2_directories(Path::new("/r/a"), Some(Path::new("/elsewhere")));
        assert_eq!(foreign_top.last(), Some(&PathBuf::from("/")));
    }

    #[test]
    fn allow_rules_missing_lists_absent_rules() {
        let all = serde_json::json!({"permissions": {"allow": ALLOW_RULES}}).to_string();
        assert_eq!(allow_rules_missing(&all), Ok(vec![]));
        let some = serde_json::json!({"permissions": {"allow": &ALLOW_RULES[..6]}}).to_string();
        assert_eq!(allow_rules_missing(&some), Ok(ALLOW_RULES[6..].to_vec()));
        assert_eq!(allow_rules_missing("{}"), Ok(ALLOW_RULES.to_vec()));
        assert!(matches!(
            allow_rules_missing("{not json"),
            Err(L2Refusal::Unreadable(_))
        ));
    }

    #[test]
    fn stat_ppid_parses_after_the_last_paren() {
        assert_eq!(stat_ppid("1234 (a) b) c) S 42 1234 1234 0 -1"), Some(42));
        assert_eq!(stat_ppid("1 (agy) S 7 1 1"), Some(7));
        assert_eq!(stat_ppid("1234 no paren S 42"), None);
        assert_eq!(stat_ppid("1234 (x) S"), None);
        assert!(lineage_ok("99 (agy (child)) S 4242 1 1", "4242\n"));
        assert!(!lineage_ok("99 (sub-agent) S 5000 1 1", "4242"));
        assert!(!lineage_ok("garbage", "4242"));
        assert!(!lineage_ok("99 (agy) S 4242", "not-a-pid"));
    }

    #[test]
    fn bridge_cmdlines() {
        assert!(is_bridge_cmdline(b"agent-terminal\0--cross-agent-mcp\0"));
        assert!(is_bridge_cmdline(b"agent-terminal\0--cross-agent-mcp"));
        assert!(!is_bridge_cmdline(b"agent-terminal\0--approval-hook\0"));
        assert!(!is_bridge_cmdline(
            b"agent-terminal\0--cross-agent-mcp\0extra\0"
        ));
        assert!(!is_bridge_cmdline(b""));
        assert!(!is_bridge_cmdline(b"\0--cross-agent-mcp\0"));
        assert!(!is_bridge_cmdline(b"agent-terminal\0"));
    }

    #[test]
    fn census_and_hello_verdicts() {
        assert!(census_ok(&[10], 10));
        assert!(!census_ok(&[], 10));
        assert!(!census_ok(&[10, 11], 10));
        assert!(!census_ok(&[11], 10));
        // Not agy's direct child, or not a bridge argv: refused, token unconsumed.
        assert_eq!(
            hello_verdict(false, true, true, None, 10, false),
            HelloVerdict::RefuseNoConsume
        );
        assert_eq!(
            hello_verdict(true, true, false, None, 10, false),
            HelloVerdict::RefuseNoConsume
        );
        assert_eq!(
            hello_verdict(true, false, true, None, 10, false),
            HelloVerdict::RefuseNoConsume
        );
        assert_eq!(
            hello_verdict(true, true, true, None, 10, false),
            HelloVerdict::Accept
        );
        assert_eq!(
            hello_verdict(true, true, true, Some(10), 10, false),
            HelloVerdict::RefuseLostIpc
        );
        assert_eq!(
            hello_verdict(true, true, true, Some(10), 10, true),
            HelloVerdict::RefuseNoConsume
        );
        assert_eq!(
            hello_verdict(true, true, true, Some(10), 11, true),
            HelloVerdict::Compromise
        );
    }
}
