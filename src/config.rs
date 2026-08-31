use serde::{Deserialize, Serialize};
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use tracing::warn;

#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
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

impl CliClient {
    /// All choices in display order; the index matches the settings dropdown.
    /// Built from this rather than a hand-written index match in both directions,
    /// which is what let the dropdown and the enum drift apart.
    pub const ALL: [CliClient; 4] = [
        CliClient::Auto,
        CliClient::Gemini,
        CliClient::Agy,
        CliClient::Claude,
    ];
}

/// The terminal color scheme.
#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq, Default)]
#[serde(rename_all = "kebab-case")]
pub enum ThemeChoice {
    #[default]
    Antigravity,
    Dracula,
    Nord,
    GruvboxDark,
    SolarizedDark,
    OneDark,
    Monokai,
}

impl std::fmt::Display for ThemeChoice {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let name = match self {
            ThemeChoice::Antigravity => "Antigravity",
            ThemeChoice::Dracula => "Dracula",
            ThemeChoice::Nord => "Nord",
            ThemeChoice::GruvboxDark => "Gruvbox Dark",
            ThemeChoice::SolarizedDark => "Solarized Dark",
            ThemeChoice::OneDark => "One Dark",
            ThemeChoice::Monokai => "Monokai",
        };
        f.write_str(name)
    }
}

impl ThemeChoice {
    /// All choices in display order; the index matches the settings dropdown.
    pub const ALL: [ThemeChoice; 7] = [
        ThemeChoice::Antigravity,
        ThemeChoice::Dracula,
        ThemeChoice::Nord,
        ThemeChoice::GruvboxDark,
        ThemeChoice::SolarizedDark,
        ThemeChoice::OneDark,
        ThemeChoice::Monokai,
    ];
}

#[derive(Serialize, Deserialize, Debug, Clone)]
#[serde(default)]
/// Persisted settings.
///
/// `#[serde(default)]` is a container attribute here, so any field missing from
/// the file falls back to that field's value in [`TerminalConfig::default`] — not
/// to the field type's own default, which would silently turn an absent
/// `scrollback_lines` into 0 rather than 10000. This is what lets settings be
/// added and retired without invalidating existing config files.
pub struct TerminalConfig {
    /// Retained so existing config files keep round-tripping, but no longer
    /// surfaced in Settings: nothing ever consumed it. Sourcing a script into the
    /// TTY before `exec` breaks the CLI's terminal handshake, so when this returns
    /// it will be as a per-profile environment file merged into the spawn
    /// environment rather than fed to the terminal.
    pub startup_script: String,
    pub scrollback_lines: u32,
    pub font_scale: f64,
    pub cli_client: CliClient,
    pub starting_directory: String,
    pub theme: ThemeChoice,
}

impl Default for TerminalConfig {
    fn default() -> Self {
        Self {
            startup_script: "~/.config/antigravity-terminal/startup.sh".to_string(),
            scrollback_lines: 10000,
            font_scale: 1.0,
            cli_client: CliClient::default(),
            starting_directory: String::new(),
            theme: ThemeChoice::default(),
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
    ///
    /// The write goes to a sibling temporary file which is then renamed over the
    /// target. Rename within a directory is atomic on Linux, so an interrupted
    /// save leaves either the previous config or the new one — never the
    /// half-written file a plain `fs::write` would produce.
    fn save_to(&self, path: &Path) {
        let content = match serde_json::to_string_pretty(self) {
            Ok(content) => content,
            Err(e) => {
                warn!("Failed to serialize config: {}", e);
                return;
            }
        };

        let tmp = path.with_extension("json.tmp");
        if let Err(e) = Self::write_all_synced(&tmp, content.as_bytes()) {
            warn!("Failed to write config to {}: {}", tmp.display(), e);
            let _ = fs::remove_file(&tmp);
            return;
        }

        if let Err(e) = fs::rename(&tmp, path) {
            warn!(
                "Failed to replace config at {}: {}; settings not saved",
                path.display(),
                e
            );
            let _ = fs::remove_file(&tmp);
        }
    }

    /// Writes `bytes` to `path`, flushing them to disk before returning. The
    /// `sync_all` matters: without it the rename can land before the contents do,
    /// which on a crash yields an empty config rather than an intact old one.
    fn write_all_synced(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
        let mut file = fs::File::create(path)?;
        file.write_all(bytes)?;
        file.sync_all()
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
            theme: ThemeChoice::Dracula,
        };
        cfg.save_to(&path);

        let loaded = TerminalConfig::load_from(&path);
        assert_eq!(loaded.startup_script, cfg.startup_script);
        assert_eq!(loaded.scrollback_lines, cfg.scrollback_lines);
        assert_eq!(loaded.font_scale, cfg.font_scale);
        assert_eq!(loaded.cli_client, cfg.cli_client);
        assert_eq!(loaded.starting_directory, cfg.starting_directory);
        assert_eq!(loaded.theme, cfg.theme);
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
    fn all_lists_every_client_in_dropdown_order() {
        // Same hazard as ThemeChoice::ALL: the client dropdown is built from this
        // array and read back by index, so a variant missing from it mis-maps the
        // picker. Exhaustive on purpose — a new variant must not compile until ALL
        // is updated.
        fn expected_index(client: CliClient) -> usize {
            match client {
                CliClient::Auto => 0,
                CliClient::Gemini => 1,
                CliClient::Agy => 2,
                CliClient::Claude => 3,
            }
        }

        assert_eq!(CliClient::ALL.len(), 4);
        for client in CliClient::ALL {
            let index = expected_index(client);
            assert_eq!(CliClient::ALL[index], client);
            assert_eq!(
                CliClient::ALL.iter().position(|c| *c == client),
                Some(index),
                "{client} is not at its expected position in ALL"
            );
        }
    }

    #[test]
    fn all_lists_every_theme_in_dropdown_order() {
        // ALL is hand-maintained and the settings dropdown maps it by index in
        // BOTH directions: position() to preselect, ALL.get(index) to read back.
        // A variant missing from ALL therefore mis-maps the picker silently — you
        // choose Nord and get Gruvbox, with no error anywhere.
        //
        // The match is exhaustive deliberately: adding a variant fails to compile
        // here until ALL is updated to match.
        fn expected_index(theme: ThemeChoice) -> usize {
            match theme {
                ThemeChoice::Antigravity => 0,
                ThemeChoice::Dracula => 1,
                ThemeChoice::Nord => 2,
                ThemeChoice::GruvboxDark => 3,
                ThemeChoice::SolarizedDark => 4,
                ThemeChoice::OneDark => 5,
                ThemeChoice::Monokai => 6,
            }
        }

        assert_eq!(ThemeChoice::ALL.len(), 7);
        for theme in ThemeChoice::ALL {
            let index = expected_index(theme);
            assert_eq!(ThemeChoice::ALL[index], theme);
            assert_eq!(
                ThemeChoice::ALL.iter().position(|t| *t == theme),
                Some(index),
                "{theme} is not at its expected position in ALL"
            );
        }
    }

    #[test]
    fn theme_display_names_are_unique_and_non_empty() {
        // The dropdown is built from these strings; duplicates or blanks would
        // leave the user unable to tell two entries apart.
        let mut names: Vec<String> = ThemeChoice::ALL.iter().map(ToString::to_string).collect();
        assert!(names.iter().all(|n| !n.trim().is_empty()));
        names.sort();
        let count = names.len();
        names.dedup();
        assert_eq!(names.len(), count, "duplicate theme display names");
    }

    #[test]
    fn every_theme_round_trips_through_json() {
        for theme in ThemeChoice::ALL {
            let encoded = serde_json::to_string(&theme).unwrap();
            let decoded: ThemeChoice = serde_json::from_str(&encoded).unwrap();
            assert_eq!(decoded, theme, "{theme} did not survive a JSON round trip");
        }
    }

    #[test]
    fn overwrites_in_place_without_leaving_a_temp_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.json");

        let mut cfg = TerminalConfig {
            scrollback_lines: 500,
            ..Default::default()
        };
        cfg.save_to(&path);
        cfg.scrollback_lines = 900;
        cfg.save_to(&path);

        assert_eq!(TerminalConfig::load_from(&path).scrollback_lines, 900);

        // The save goes via config.json.tmp and renames; a leftover temp file
        // would mean the rename never happened.
        let leftovers: Vec<_> = std::fs::read_dir(dir.path())
            .unwrap()
            .filter_map(Result::ok)
            .map(|e| e.file_name().to_string_lossy().to_string())
            .filter(|name| name.ends_with(".tmp"))
            .collect();
        assert!(
            leftovers.is_empty(),
            "temp files left behind: {leftovers:?}"
        );
    }

    #[test]
    fn partial_file_keeps_defaults_for_missing_fields() {
        // Container-level #[serde(default)] must fall back to TerminalConfig's own
        // defaults, not to each field type's default — otherwise a config written
        // by an older build silently loses its scrollback to 0.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.json");
        std::fs::File::create(&path)
            .unwrap()
            .write_all(br#"{"theme":"dracula"}"#)
            .unwrap();

        let loaded = TerminalConfig::load_from(&path);
        assert_eq!(loaded.theme, ThemeChoice::Dracula);
        assert_eq!(
            loaded.scrollback_lines,
            TerminalConfig::default().scrollback_lines
        );
        assert_eq!(loaded.font_scale, TerminalConfig::default().font_scale);
    }

    #[test]
    fn legacy_startup_script_still_parses() {
        // The Settings row is gone, but a config file written by v1.x still
        // carries the key and must not be rejected.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.json");
        std::fs::File::create(&path)
            .unwrap()
            .write_all(br#"{"startup_script":"/tmp/old.sh","scrollback_lines":200}"#)
            .unwrap();

        let loaded = TerminalConfig::load_from(&path);
        assert_eq!(loaded.startup_script, "/tmp/old.sh");
        assert_eq!(loaded.scrollback_lines, 200);
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
