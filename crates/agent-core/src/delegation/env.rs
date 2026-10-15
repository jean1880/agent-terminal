//! Environment variable names of the delegation runtime (contract §5).

/// Absolute socket path. Not secret.
pub const ENV_ENDPOINT: &str = "AGENT_TERMINAL_DELEGATION_ENDPOINT";
/// The 128-bit app-instance nonce, 32 hex. Not secret.
pub const ENV_INSTANCE: &str = "AGENT_TERMINAL_DELEGATION_INSTANCE";
/// The capability token. **Secret.**
pub const ENV_CAP: &str = "AGENT_TERMINAL_DELEGATION_CAP";
/// The app's pid, decimal, for the bridge's lineage self-check (contract §1).
pub const ENV_APP_PID: &str = "AGENT_TERMINAL_DELEGATION_APP_PID";

/// Stripped from every spawn's inherited environment before its own values are installed.
pub const STRIP_ON_SPAWN: [&str; 5] = [
    ENV_ENDPOINT,
    ENV_INSTANCE,
    ENV_CAP,
    ENV_APP_PID,
    "AGENT_TERMINAL_APPROVAL_SOCKET",
];

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strip_list_names_every_delegation_variable_and_the_approval_socket() {
        assert_eq!(STRIP_ON_SPAWN.len(), 5);
        for name in [ENV_ENDPOINT, ENV_INSTANCE, ENV_CAP, ENV_APP_PID] {
            assert!(name.starts_with("AGENT_TERMINAL_DELEGATION_"));
            assert!(STRIP_ON_SPAWN.contains(&name));
        }
        assert!(STRIP_ON_SPAWN.contains(&"AGENT_TERMINAL_APPROVAL_SOCKET"));
    }
}
