use serde::{Deserialize, Serialize};
use std::fs;
use std::path::{Path, PathBuf};
use tracing::warn;

#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq, Default)]
#[serde(rename_all = "lowercase")]
pub enum CliClient {
    #[default]
    Auto,
    Gemini,
    Agy,
    Claude,
}

impl std::fmt::Display for CliClient {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            CliClient::Auto => write!(f, "Auto-detect"),
            CliClient::Gemini => write!(f, "Gemini"),
            CliClient::Agy => write!(f, "Agy"),
            CliClient::Claude => write!(f, "Claude"),
        }
    }
}

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct TerminalConfig {
    pub startup_script: String,
    pub scrollback_lines: u32,
    pub font_scale: f64,
    #[serde(default)]
    pub cli_client: CliClient,
    #[serde(default)]
    pub starting_directory: String,
}

impl Default for TerminalConfig {
    fn default() -> Self {
        Self {
            startup_script: "~/.config/antigravity-terminal/startup.sh".to_string(),
            scrollback_lines: 10000,
            font_scale: 1.0,
            cli_client: CliClient::default(),
            starting_directory: String::new(),
        }
    }
}

impl TerminalConfig {
    pub fn config_dir() -> PathBuf {
        let home = std::env::var("HOME").unwrap_or_else(|_| "/".to_string());
        let dir = PathBuf::from(home)
            .join(".config")
            .join("antigravity-terminal");
        if !dir.exists() {
            let _ = fs::create_dir_all(&dir);
        }
        dir
    }

    pub fn config_path() -> PathBuf {
        Self::config_dir().join("config.json")
    }

    pub fn load() -> Self {
        Self::load_from(&Self::config_path())
    }

    /// Loads a config from an explicit path, falling back to defaults. A missing
    /// file is expected (first run); a present-but-invalid file is logged so the
    /// user knows their settings were ignored rather than silently discarded.
    fn load_from(path: &Path) -> Self {
        let content = match fs::read_to_string(path) {
            Ok(content) => content,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Self::default(),
            Err(e) => {
                warn!(
                    "Failed to read config at {}: {}; using defaults",
                    path.display(),
                    e
                );
                return Self::default();
            }
        };
        match serde_json::from_str(&content) {
            Ok(config) => config,
            Err(e) => {
                warn!(
                    "Failed to parse config at {}: {}; using defaults",
                    path.display(),
                    e
                );
                Self::default()
            }
        }
    }

    pub fn save(&self) {
        self.save_to(&Self::config_path());
    }

    /// Serializes and writes the config to an explicit path, logging on failure.
    fn save_to(&self, path: &Path) {
        match serde_json::to_string_pretty(self) {
            Ok(content) => {
                if let Err(e) = fs::write(path, content) {
                    warn!("Failed to write config to {}: {}", path.display(), e);
                }
            }
            Err(e) => warn!("Failed to serialize config: {}", e),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    #[test]
    fn round_trips_through_disk() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.json");
        let cfg = TerminalConfig {
            startup_script: "/tmp/startup.sh".to_string(),
            scrollback_lines: 500,
            font_scale: 1.5,
            cli_client: CliClient::Claude,
            starting_directory: "/tmp/project".to_string(),
        };
        cfg.save_to(&path);

        let loaded = TerminalConfig::load_from(&path);
        assert_eq!(loaded.startup_script, cfg.startup_script);
        assert_eq!(loaded.scrollback_lines, cfg.scrollback_lines);
        assert_eq!(loaded.font_scale, cfg.font_scale);
        assert_eq!(loaded.cli_client, cfg.cli_client);
        assert_eq!(loaded.starting_directory, cfg.starting_directory);
    }

    #[test]
    fn missing_file_yields_default() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("does-not-exist.json");
        assert_eq!(
            TerminalConfig::load_from(&path).cli_client,
            CliClient::default()
        );
    }

    #[test]
    fn malformed_file_yields_default() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.json");
        std::fs::File::create(&path)
            .unwrap()
            .write_all(b"{ not valid json ]")
            .unwrap();
        assert_eq!(
            TerminalConfig::load_from(&path).scrollback_lines,
            TerminalConfig::default().scrollback_lines
        );
    }
}
