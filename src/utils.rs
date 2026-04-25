//! Utility functions for Gemini Terminal.

use std::env;
use tracing::{info, warn, debug};

/// Standalone detection logic that can run on a background thread.
/// Takes environment parameters for testability.
pub fn check_gemini_binary(
    path_env: Option<String>,
    home_env: Option<String>,
    shell_env: Option<String>,
) -> bool {
    let current_path = path_env.unwrap_or_default();
    debug!("Detection PATH: {}", current_path);

    // 1. Try which
    debug!("Step 1: Trying 'which gemini'");
    match std::process::Command::new("which").arg("gemini").output() {
        Ok(output) => {
            if output.status.success() {
                let path = String::from_utf8_lossy(&output.stdout).trim().to_string();
                info!("Gemini found via 'which' at: {}", path);
                return true;
            }
        }
        Err(_) => {}
    }

    // 2. Try common absolute paths
    debug!("Step 2: Trying common absolute paths");
    let home = home_env.unwrap_or_default();
    let paths = [
        "/usr/bin/gemini".to_string(),
        "/usr/local/bin/gemini".to_string(),
        format!("{}/.local/bin/gemini", home),
        format!("{}/.npm-global/bin/gemini", home),
        format!("{}/bin/gemini", home),
    ];

    for path in paths {
        if !path.is_empty() && std::path::Path::new(&path).exists() {
            debug!("Gemini found at absolute path: {}", path);
            return true;
        }
    }

    // 3. Try shell command -v (interactive)
    debug!("Step 3: Trying shell command -v gemini");
    let shell = shell_env.unwrap_or_else(|| "/bin/sh".to_string());
    if let Ok(output) = std::process::Command::new(&shell)
        .args(["-ic", "command -v gemini"])
        .output()
    {
        if output.status.success() {
            let path = String::from_utf8_lossy(&output.stdout).trim().to_string();
            info!("Gemini found via shell -ic at: {}", path);
            return true;
        }
    }

    warn!("Gemini binary not found after all checks.");
    false
}

/// Determines the startup command based on whether the gemini binary exists.
pub fn get_startup_command(has_gemini: bool) -> Vec<&'static str> {
    if has_gemini {
        vec!["-ic", "gemini"]
    } else {
        vec!["-ic", "exec $SHELL"]
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs::File;
    use tempfile::tempdir;

    #[test]
    fn test_startup_command_gemini_exists() {
        let cmd = get_startup_command(true);
        assert_eq!(cmd[0], "-ic");
        assert_eq!(cmd[1], "gemini");
    }

    #[test]
    fn test_startup_command_gemini_missing() {
        let cmd = get_startup_command(false);
        assert_eq!(cmd[0], "-ic");
        assert_eq!(cmd[1], "exec $SHELL");
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
        assert!(check_gemini_binary(
            Some("".to_string()),
            Some(home_path),
            Some("/bin/sh".to_string())
        ));
    }

    #[test]
    fn test_check_gemini_missing() {
        let dir = tempdir().unwrap();
        let home_path = dir.path().to_str().unwrap().to_string();
        
        // No binary anywhere
        assert!(!check_gemini_binary(
            Some("".to_string()),
            Some(home_path),
            Some("/bin/sh".to_string())
        ));
    }
}
