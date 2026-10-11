//! Where Codex keeps its rules, as the Codex threads see it (red-team #371).
//!
//! A Codex thread runs with the app's environment minus `clear_env`, plus the Codex profile's
//! env file (`AgentEnv`). Its `CODEX_HOME` therefore comes from, in order: the env file, nothing
//! (when `clear_env` strips it), the app's own environment, else `~/.codex`. Import, export and
//! View Differences use the folder that resolves to, never the GUI process's `$CODEX_HOME`
//! alone, so the rules land where the threads load them.

use std::ffi::OsStr;
use std::path::{Path, PathBuf};

/// The variable Codex reads its home from.
pub const VAR: &str = "CODEX_HOME";

/// The Codex home a thread with this environment uses: `env_file` (the profile's env file
/// variables), `clear` (the `clear_env` patterns), `inherited` (the app's own `CODEX_HOME`) and
/// `home` (the user's home folder). An empty value counts as unset, as Codex treats it. `Err`
/// says why it cannot be known (a relative value, which would depend on each thread's folder;
/// no home folder).
pub fn resolve(
    env_file: &[(String, String)],
    clear: &[String],
    inherited: Option<&OsStr>,
    home: Option<&Path>,
) -> Result<PathBuf, String> {
    let from_file = env_file
        .iter()
        .rev()
        .find(|(k, _)| k == VAR)
        .map(|(_, v)| PathBuf::from(v));
    let stripped = !crate::utils::strip_env(&mut vec![format!("{VAR}=x")], clear).is_empty();
    let chosen = match from_file {
        Some(value) => Some(value),
        None if stripped => None,
        None => inherited.map(PathBuf::from),
    }
    .filter(|p| !p.as_os_str().is_empty());
    match chosen {
        Some(path) if path.is_absolute() => Ok(path),
        Some(path) => Err(format!(
            "{VAR} is set to a relative path ({}), which Codex resolves per thread",
            path.display()
        )),
        None => home
            .map(|h| h.join(".codex"))
            .ok_or_else(|| "there is no home folder".to_owned()),
    }
}

/// The app's own `CODEX_HOME` and home folder; `None` under test, so no test reads the real
/// environment or the user's `~/.codex`.
pub fn process_inputs() -> Option<(Option<std::ffi::OsString>, Option<PathBuf>)> {
    if cfg!(test) {
        return None;
    }
    Some((
        std::env::var_os(VAR),
        std::env::var_os("HOME").map(PathBuf::from),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn env(pairs: &[(&str, &str)]) -> Vec<(String, String)> {
        pairs
            .iter()
            .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
            .collect()
    }

    #[test]
    fn the_profile_environment_decides_not_the_gui() {
        let home = Path::new("/home/u");
        let gui = Some(OsStr::new("/gui/codex"));
        // Nothing set anywhere: the default.
        assert_eq!(
            resolve(&[], &[], None, Some(home)),
            Ok(PathBuf::from("/home/u/.codex"))
        );
        // The GUI's own value reaches the threads unless something overrides it.
        assert_eq!(
            resolve(&[], &[], gui, Some(home)),
            Ok(PathBuf::from("/gui/codex"))
        );
        // The profile's env file wins over the GUI's value.
        assert_eq!(
            resolve(&env(&[(VAR, "/profile/codex")]), &[], gui, Some(home)),
            Ok(PathBuf::from("/profile/codex"))
        );
        // clear_env strips the GUI's value: the threads fall back to the default.
        assert_eq!(
            resolve(&[], &["CODEX_*".to_owned()], gui, Some(home)),
            Ok(PathBuf::from("/home/u/.codex"))
        );
        assert_eq!(
            resolve(&[], &[VAR.to_owned()], gui, Some(home)),
            Ok(PathBuf::from("/home/u/.codex"))
        );
        // …but the env file is applied after clear_env.
        assert_eq!(
            resolve(
                &env(&[(VAR, "/profile/codex")]),
                &[VAR.to_owned()],
                gui,
                Some(home)
            ),
            Ok(PathBuf::from("/profile/codex"))
        );
        // Empty counts as unset.
        assert_eq!(
            resolve(&env(&[(VAR, "")]), &[], None, Some(home)),
            Ok(PathBuf::from("/home/u/.codex"))
        );
    }

    #[test]
    fn an_unknowable_home_is_an_error_not_a_guess() {
        assert!(resolve(
            &env(&[(VAR, "rel/codex")]),
            &[],
            None,
            Some(Path::new("/h"))
        )
        .is_err());
        assert!(resolve(&[], &[], Some(OsStr::new("codex")), Some(Path::new("/h"))).is_err());
        assert!(resolve(&[], &[], None, None).is_err());
        assert_eq!(
            process_inputs(),
            None,
            "tests never read the real environment"
        );
    }
}
