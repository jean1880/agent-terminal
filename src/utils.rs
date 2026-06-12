//! Utility functions for Antigravity Terminal.

use tracing::{debug, info, warn};

/// Standalone detection logic that can run on a background thread.
/// Takes environment parameters for testability.
pub fn detect_cli_binary(
    path_env: Option<String>,
    home_env: Option<String>,
    shell_env: Option<String>,
) -> Option<String> {
    // 1. Try to find agy first (the new standard)
    if check_binary_exists("agy", path_env.clone(), home_env.clone(), shell_env.clone()) {
        return Some("agy".to_string());
    }
    // 2. Try to find gemini (for backward compatibility / user preference)
    if check_binary_exists("gemini", path_env, home_env, shell_env) {
        return Some("gemini".to_string());
    }
    None
}

/// Helper function to check if a specific binary exists.
fn check_binary_exists(
    name: &str,
    path_env: Option<String>,
    home_env: Option<String>,
    shell_env: Option<String>,
) -> bool {
    let current_path = path_env.unwrap_or_default();
    let home = home_env.unwrap_or_default();
    debug!("Detection PATH for {}: {}", name, current_path);

    // 1. Try which
    debug!("Step 1: Trying 'which {}'", name);
    let mut cmd = std::process::Command::new("which");
    cmd.arg(name);
    if !current_path.is_empty() {
        cmd.env("PATH", &current_path);
    }
    if let Ok(output) = cmd.output() {
        if output.status.success() {
            let path = String::from_utf8_lossy(&output.stdout).trim().to_string();
            info!("{} found via 'which' at: {}", name, path);
            return true;
        }
    }

    // 2. Try common absolute paths
    debug!("Step 2: Trying common absolute paths for {}", name);
    let paths = [
        format!("/usr/bin/{}", name),
        format!("/usr/local/bin/{}", name),
        format!("{}/.local/bin/{}", home, name),
        format!("{}/.npm-global/bin/{}", home, name),
        format!("{}/bin/{}", home, name),
    ];

    for path in paths {
        if !path.is_empty() && std::path::Path::new(&path).exists() {
            debug!("{} found at absolute path: {}", name, path);
            return true;
        }
    }

    // 3. Try shell command -v (interactive)
    debug!("Step 3: Trying shell command -v {}", name);
    let shell = shell_env.unwrap_or_else(|| "/bin/sh".to_string());
    let mut shell_cmd = std::process::Command::new(&shell);
    shell_cmd.args(["-ic", &format!("command -v {}", name)]);
    if !current_path.is_empty() {
        shell_cmd.env("PATH", &current_path);
    }
    if let Ok(output) = shell_cmd.output() {
        if output.status.success() {
            let path = String::from_utf8_lossy(&output.stdout).trim().to_string();
            info!("{} found via shell -ic at: {}", name, path);
            return true;
        }
    }

    warn!("{} binary not found after all checks.", name);
    false
}

/// Determines the startup command based on whether the agy binary exists.
pub fn get_startup_command(binary: Option<&str>, config: &crate::config::TerminalConfig) -> Vec<String> {
    let mut script_cmd = String::new();
    let script_path = config.startup_script.replace("~", &std::env::var("HOME").unwrap_or_default());
    if !script_path.is_empty() {
        script_cmd = format!("if [ -f \"{0}\" ]; then source \"{0}\"; fi; ", script_path);
    }

    match binary {
        Some(name) => {
            let dir_flag = if name == "agy" {
                "--add-dir"
            } else {
                "--include-directories"
            };
            vec![
                "-ic".to_string(),
                format!("{}{} {} ~/git", script_cmd, name, dir_flag),
            ]
        }
        None => vec!["-ic".to_string(), "exec $SHELL".to_string()],
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs::File;
    use tempfile::tempdir;

    #[test]
    fn test_startup_command_agy_exists() {
        let config = crate::config::TerminalConfig::default();
        let cmd = get_startup_command(Some("agy"), &config);
        assert_eq!(cmd[0], "-ic");
        assert!(cmd[1].ends_with("agy --add-dir ~/git"));
    }

    #[test]
    fn test_startup_command_gemini_exists() {
        let config = crate::config::TerminalConfig::default();
        let cmd = get_startup_command(Some("gemini"), &config);
        assert_eq!(cmd[0], "-ic");
        assert!(cmd[1].ends_with("gemini --include-directories ~/git"));
    }

    #[test]
    fn test_startup_command_agy_missing() {
        let config = crate::config::TerminalConfig::default();
        let cmd = get_startup_command(None, &config);
        assert_eq!(cmd[0], "-ic");
        assert_eq!(cmd[1], "exec $SHELL");
    }

    #[test]
    fn test_check_agy_absolute_path() {
        let dir = tempdir().unwrap();
        let home_path = dir.path().to_str().unwrap().to_string();

        let local_bin = dir.path().join(".local/bin");
        std::fs::create_dir_all(&local_bin).unwrap();
        let agy_path = local_bin.join("agy");
        File::create(&agy_path).unwrap();

        // Should find it in Phase 2
        assert_eq!(
            detect_cli_binary(
                Some("/non/existent/path".to_string()),
                Some(home_path),
                Some("/bin/sh".to_string())
            ),
            Some("agy".to_string())
        );
    }

    #[test]
    fn test_check_gemini_absolute_path() {
        let dir = tempdir().unwrap();
        let home_path = dir.path().to_str().unwrap().to_string();

        let local_bin = dir.path().join(".local/bin");
        std::fs::create_dir_all(&local_bin).unwrap();
        let gemini_path = local_bin.join("gemini");
        File::create(&gemini_path).unwrap();

        // Should find it in Phase 2
        assert_eq!(
            detect_cli_binary(
                Some("/non/existent/path".to_string()),
                Some(home_path),
                Some("/bin/sh".to_string())
            ),
            Some("gemini".to_string())
        );
    }

    #[test]
    fn test_check_agy_missing() {
        let dir = tempdir().unwrap();
        let home_path = dir.path().to_str().unwrap().to_string();

        // No binary anywhere, and empty PATH
        // We use a non-existent path for PATH to ensure 'which' and 'shell -ic' fail
        assert!(detect_cli_binary(
            Some("/non/existent/path".to_string()),
            Some(home_path),
            Some("/bin/sh".to_string())
        )
        .is_none());
    }
}
