use serde::{Deserialize, Serialize};
use std::fs;
use std::path::PathBuf;

#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum CliClient {
    Auto,
    Gemini,
    Agy,
    Claude,
}

impl Default for CliClient {
    fn default() -> Self {
        CliClient::Auto
    }
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
}

impl Default for TerminalConfig {
    fn default() -> Self {
        Self {
            startup_script: "~/.config/antigravity-terminal/startup.sh".to_string(),
            scrollback_lines: 10000,
            font_scale: 1.0,
            cli_client: CliClient::default(),
        }
    }
}

impl TerminalConfig {
    pub fn config_dir() -> PathBuf {
        let home = std::env::var("HOME").unwrap_or_else(|_| "/".to_string());
        let dir = PathBuf::from(home).join(".config").join("antigravity-terminal");
        if !dir.exists() {
            let _ = fs::create_dir_all(&dir);
        }
        dir
    }

    pub fn config_path() -> PathBuf {
        Self::config_dir().join("config.json")
    }

    pub fn load() -> Self {
        let path = Self::config_path();
        if path.exists() {
            if let Ok(content) = fs::read_to_string(&path) {
                if let Ok(config) = serde_json::from_str(&content) {
                    return config;
                }
            }
        }
        Self::default()
    }

    pub fn save(&self) {
        let path = Self::config_path();
        if let Ok(content) = serde_json::to_string_pretty(self) {
            let _ = fs::write(path, content);
        }
    }
}
