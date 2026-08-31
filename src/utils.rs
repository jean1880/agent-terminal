//! Utility functions for Agent Terminal.

use crate::config::CliClient;
use std::cell::RefCell;
use std::collections::HashMap;
use tracing::{debug, info, warn};

thread_local! {
    /// Detection results, keyed by the client that was asked for.
    ///
    /// [`detect_cli_binary`] shells out — including `$SHELL -ic`, which sources
    /// the user's rc file — so it can take seconds on a heavy interactive shell.
    /// Caching it here means a second window, or reopening Settings, never pays
    /// that cost again. GTK confines the application to one thread, so a
    /// `thread_local` is process-wide in practice; detection itself runs on a
    /// worker thread and only the result is recorded here, on the main thread.
    static DETECTION_CACHE: RefCell<HashMap<CliClient, Option<String>>> =
        RefCell::new(HashMap::new());
}

/// Returns a previously detected binary for `client`.
///
/// The double `Option` is meaningful: `None` means "never detected", while
/// `Some(None)` means "detected, and nothing is installed" — a cached negative
/// that must not trigger another round of shelling out.
pub fn cached_cli_binary(client: CliClient) -> Option<Option<String>> {
    DETECTION_CACHE.with(|cache| cache.borrow().get(&client).cloned())
}

/// Records a detection result so later lookups can skip the subprocesses.
pub fn cache_cli_binary(client: CliClient, binary: Option<String>) {
    DETECTION_CACHE.with(|cache| {
        cache.borrow_mut().insert(client, binary);
    });
}

/// Forgets cached detection results.
#[cfg(test)]
pub fn clear_detection_cache() {
    DETECTION_CACHE.with(|cache| cache.borrow_mut().clear());
}

/// Standalone detection logic that can run on a background thread.
/// Takes environment parameters for testability.
pub fn detect_cli_binary(
    selected_client: crate::config::CliClient,
    path_env: Option<&str>,
    home_env: Option<&str>,
    shell_env: Option<&str>,
) -> Option<String> {
    match selected_client {
        crate::config::CliClient::Auto => {
            // 1. Try to find claude first (preferred default)
            if check_binary_exists("claude", path_env, home_env, shell_env) {
                return Some("claude".to_string());
            }
            // 2. Try to find agy (the new standard)
            if check_binary_exists("agy", path_env, home_env, shell_env) {
                return Some("agy".to_string());
            }
            // 3. Try to find gemini (for backward compatibility / user preference)
            if check_binary_exists("gemini", path_env, home_env, shell_env) {
                return Some("gemini".to_string());
            }
            None
        }
        crate::config::CliClient::Gemini => {
            if check_binary_exists("gemini", path_env, home_env, shell_env) {
                Some("gemini".to_string())
            } else {
                None
            }
        }
        crate::config::CliClient::Agy => {
            if check_binary_exists("agy", path_env, home_env, shell_env) {
                Some("agy".to_string())
            } else {
                None
            }
        }
        crate::config::CliClient::Claude => {
            if check_binary_exists("claude", path_env, home_env, shell_env) {
                Some("claude".to_string())
            } else {
                None
            }
        }
    }
}

/// Helper function to check if a specific binary exists.
fn check_binary_exists(
    name: &str,
    path_env: Option<&str>,
    home_env: Option<&str>,
    shell_env: Option<&str>,
) -> bool {
    let current_path = path_env.unwrap_or_default();
    let home = home_env.unwrap_or_default();
    debug!("Detection PATH for {}: {}", name, current_path);

    // 1. Try which
    debug!("Step 1: Trying 'which {}'", name);
    let mut cmd = std::process::Command::new("which");
    cmd.arg(name);
    if !current_path.is_empty() {
        cmd.env("PATH", current_path);
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
    let shell = shell_env.unwrap_or("/bin/sh");
    let mut shell_cmd = std::process::Command::new(shell);
    shell_cmd.args(["-ic", &format!("command -v {}", name)]);
    if !current_path.is_empty() {
        shell_cmd.env("PATH", current_path);
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

/// Resolves the working directory to use, handling ~ expansion and fallback to home.
pub fn resolve_working_directory(starting_dir: &str, home_dir: &str) -> String {
    let mut work_dir = starting_dir.trim().to_string();
    if work_dir.is_empty() {
        home_dir.to_string()
    } else {
        if work_dir.starts_with('~') {
            work_dir = work_dir.replacen('~', home_dir, 1);
        }
        if std::path::Path::new(&work_dir).exists() {
            work_dir
        } else {
            warn!(
                "Configured starting directory '{}' does not exist, falling back to home directory",
                work_dir
            );
            home_dir.to_string()
        }
    }
}

/// Determines the startup command for the detected CLI binary.
pub fn get_startup_command(binary: Option<&str>) -> Vec<String> {
    match binary {
        Some(name) => {
            // `exec` is critical: it replaces the wrapping interactive shell with the
            // CLI so the CLI directly owns the controlling terminal (session leader).
            // Without it, `zsh -ic "claude"` runs claude as a *child job* of the shell,
            // and Claude Code comes up degraded — no status line, CLAUDE.md not loaded,
            // settings/folder-trust not persisted.
            //
            // We intentionally do NOT source a startup script here: emitting banner /
            // `clear` output into the TTY immediately before exec disrupts Claude's
            // initial terminal handshake. The working directory (set to $HOME in
            // setup_terminal_ui) provides the workspace as a native, trust-persisted
            // project, so no --add-dir flag is needed either.
            vec!["-ic".to_string(), format!("exec {}", name)]
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
    fn detection_cache_separates_a_miss_from_a_cached_negative() {
        // The distinction is the whole point: a client that was checked and found
        // absent must not be re-checked, because "absent" costs three subprocess
        // spawns including an interactive shell.
        clear_detection_cache();

        assert_eq!(
            cached_cli_binary(CliClient::Claude),
            None,
            "expected a miss"
        );

        cache_cli_binary(CliClient::Claude, None);
        assert_eq!(
            cached_cli_binary(CliClient::Claude),
            Some(None),
            "a cached negative must read back as Some(None), not a miss"
        );

        cache_cli_binary(CliClient::Agy, Some("agy".to_string()));
        assert_eq!(
            cached_cli_binary(CliClient::Agy),
            Some(Some("agy".to_string()))
        );
        // Keys must not collide: Claude's cached negative survives Agy's entry.
        assert_eq!(cached_cli_binary(CliClient::Claude), Some(None));

        clear_detection_cache();
        assert_eq!(cached_cli_binary(CliClient::Agy), None);
    }

    #[test]
    fn test_resolve_working_directory() {
        let dir = tempdir().unwrap();
        let home_dir = dir.path().to_str().unwrap().to_string();

        // Test empty starting directory (defaults to home_dir)
        assert_eq!(resolve_working_directory("", &home_dir), home_dir);
        assert_eq!(resolve_working_directory("  ", &home_dir), home_dir);

        // Test non-existent starting directory (defaults to home_dir)
        assert_eq!(
            resolve_working_directory("/non/existent/path", &home_dir),
            home_dir
        );

        // Test ~ expansion to home_dir
        assert_eq!(resolve_working_directory("~", &home_dir), home_dir);

        // Create a subfolder inside home_dir
        let sub_dir = dir.path().join("projects");
        std::fs::create_dir_all(&sub_dir).unwrap();
        let sub_dir_str = sub_dir.to_str().unwrap();

        // Test existing absolute starting directory
        assert_eq!(
            resolve_working_directory(sub_dir_str, &home_dir),
            sub_dir_str
        );

        // Test existing starting directory starting with ~
        assert_eq!(
            resolve_working_directory("~/projects", &home_dir),
            sub_dir_str
        );
    }

    #[test]
    fn test_startup_command_agy_exists() {
        let cmd = get_startup_command(Some("agy"));
        assert_eq!(cmd[0], "-ic");
        // exec so the CLI owns the TTY; no --add-dir (workspace = spawn cwd $HOME).
        assert!(cmd[1].ends_with("exec agy"));
        assert!(!cmd[1].contains("--add-dir"));
    }

    #[test]
    fn test_startup_command_gemini_exists() {
        let cmd = get_startup_command(Some("gemini"));
        assert_eq!(cmd[0], "-ic");
        assert!(cmd[1].ends_with("exec gemini"));
        assert!(!cmd[1].contains("--include-directories"));
    }

    #[test]
    fn test_startup_command_agy_missing() {
        let cmd = get_startup_command(None);
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
                crate::config::CliClient::Auto,
                Some("/non/existent/path"),
                Some(home_path.as_str()),
                Some("/bin/sh")
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
                crate::config::CliClient::Auto,
                Some("/non/existent/path"),
                Some(home_path.as_str()),
                Some("/bin/sh")
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
            crate::config::CliClient::Auto,
            Some("/non/existent/path"),
            Some(home_path.as_str()),
            Some("/bin/sh")
        )
        .is_none());
    }

    #[test]
    fn test_check_explicit_selections() {
        let dir = tempdir().unwrap();
        let home_path = dir.path().to_str().unwrap().to_string();

        let local_bin = dir.path().join(".local/bin");
        std::fs::create_dir_all(&local_bin).unwrap();
        let agy_path = local_bin.join("agy");
        File::create(&agy_path).unwrap();

        // When Gemini is explicitly selected but missing
        assert_eq!(
            detect_cli_binary(
                crate::config::CliClient::Gemini,
                Some("/non/existent/path"),
                Some(home_path.as_str()),
                Some("/bin/sh")
            ),
            None
        );

        // When Agy is explicitly selected and present
        assert_eq!(
            detect_cli_binary(
                crate::config::CliClient::Agy,
                Some("/non/existent/path"),
                Some(home_path.as_str()),
                Some("/bin/sh")
            ),
            Some("agy".to_string())
        );

        // Test Claude startup command format — `exec claude`, no --add-dir.
        // exec lets claude own the TTY (status line / CLAUDE.md / trust all work);
        // the workspace comes from the spawn cwd ($HOME), not an --add-dir flag
        // (which would re-prompt for folder access on every launch).
        let cmd = get_startup_command(Some("claude"));
        assert_eq!(cmd[0], "-ic");
        assert!(cmd[1].ends_with("exec claude"));
        assert!(!cmd[1].contains("--add-dir"));
    }
}
