//! The environment a Codex thread runs in, and where Codex keeps its rules there (red-team
//! #371, Codex peer #4).
//!
//! A Codex thread gets the app's environment minus `clear_env` (and any inherited approval
//! socket), the user's shell `PATH` when it has been read, then the Codex profile's env file on
//! top (`AgentEnv`). [`child_env`] rebuilds exactly that, and [`resolve`] reads `CODEX_HOME`
//! (else `$HOME/.codex`) from it, so import, export and View Differences use the folder the
//! threads load, and every `codex execpolicy check` runs in the same environment. Nothing here
//! reads the process environment except [`process_env`]: callers pass it in, so tests never see
//! the real one.

use std::path::PathBuf;

/// The variable Codex reads its home from.
pub const VAR: &str = "CODEX_HOME";

/// What a Codex thread's environment is built from.
#[derive(Debug, Clone, Default)]
pub struct Inputs {
    /// The app's own environment ([`process_env`]).
    pub inherited: Vec<(String, String)>,
    /// The user's shell `PATH`, once read (`utils::known_shell_path`).
    pub shell_path: Option<String>,
    /// The Codex profile's env file variables.
    pub env_file: Vec<(String, String)>,
    /// The `clear_env` patterns.
    pub clear: Vec<String>,
}

/// The app's own environment, lossy (a non-UTF-8 variable must not panic here).
pub fn process_env() -> Vec<(String, String)> {
    std::env::vars_os()
        .map(|(k, v)| {
            (
                k.to_string_lossy().into_owned(),
                v.to_string_lossy().into_owned(),
            )
        })
        .collect()
}

/// The environment a Codex thread gets, built as `AgentEnv` builds it: inherited minus
/// `clear_env` and the app's own socket variables, then the shell `PATH`, then the env file.
pub fn child_env(inputs: &Inputs) -> Vec<(String, String)> {
    let mut block: Vec<String> = inputs
        .inherited
        .iter()
        .map(|(k, v)| format!("{k}={v}"))
        .collect();
    crate::utils::strip_env(&mut block, &inputs.clear);
    let own = [
        agent_core::approval::ENV_SOCKET,
        crate::approval_server::ENV_HOOK_BIN,
    ];
    let mut env: Vec<(String, String)> = block
        .into_iter()
        .filter_map(|entry| {
            let (k, v) = entry.split_once('=')?;
            (!own.contains(&k)).then(|| (k.to_owned(), v.to_owned()))
        })
        .collect();
    let mut set = |key: &str, value: &str| {
        env.retain(|(k, _)| k != key);
        env.push((key.to_owned(), value.to_owned()));
    };
    if let Some(path) = &inputs.shell_path {
        set("PATH", path);
    }
    for (k, v) in &inputs.env_file {
        set(k, v);
    }
    env
}

/// The value of `key` in `env` (the last one wins), empty counting as unset.
pub fn get<'a>(env: &'a [(String, String)], key: &str) -> Option<&'a str> {
    env.iter()
        .rev()
        .find(|(k, _)| k == key)
        .map(|(_, v)| v.as_str())
        .filter(|v| !v.is_empty())
}

/// The Codex home in a thread's environment ([`child_env`]): `CODEX_HOME`, else
/// `$HOME/.codex`. `Err` says why it cannot be known (a relative value, which would depend on
/// each thread's folder; no home folder).
pub fn resolve(env: &[(String, String)]) -> Result<PathBuf, String> {
    match get(env, VAR).map(PathBuf::from) {
        Some(path) if path.is_absolute() => Ok(path),
        Some(path) => Err(format!(
            "{VAR} is set to a relative path ({}), which Codex resolves per thread",
            path.display()
        )),
        None => match get(env, "HOME").map(PathBuf::from) {
            Some(home) if home.is_absolute() => Ok(home.join(".codex")),
            _ => Err("Codex's environment has no home folder".to_owned()),
        },
    }
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

    fn home_of(inputs: &Inputs) -> Result<PathBuf, String> {
        resolve(&child_env(inputs))
    }

    #[test]
    fn the_thread_environment_decides_not_the_gui() {
        let gui = env(&[
            ("HOME", "/home/u"),
            (VAR, "/gui/codex"),
            ("PATH", "/gui/bin"),
        ]);
        let plain = Inputs {
            inherited: env(&[("HOME", "/home/u")]),
            ..Inputs::default()
        };
        assert_eq!(home_of(&plain), Ok(PathBuf::from("/home/u/.codex")));
        // The GUI's value reaches the threads unless something overrides it.
        let inherited = Inputs {
            inherited: gui.clone(),
            ..Inputs::default()
        };
        assert_eq!(home_of(&inherited), Ok(PathBuf::from("/gui/codex")));
        // The profile's env file wins, HOME included.
        let profile = Inputs {
            inherited: gui.clone(),
            env_file: env(&[(VAR, "/profile/codex")]),
            ..Inputs::default()
        };
        assert_eq!(home_of(&profile), Ok(PathBuf::from("/profile/codex")));
        let other_home = Inputs {
            inherited: env(&[("HOME", "/home/u")]),
            env_file: env(&[("HOME", "/srv/agent")]),
            ..Inputs::default()
        };
        assert_eq!(home_of(&other_home), Ok(PathBuf::from("/srv/agent/.codex")));
        // clear_env strips the GUI's value: the threads fall back to $HOME/.codex.
        for pattern in ["CODEX_*", VAR] {
            let cleared = Inputs {
                inherited: gui.clone(),
                clear: vec![pattern.to_owned()],
                ..Inputs::default()
            };
            assert_eq!(home_of(&cleared), Ok(PathBuf::from("/home/u/.codex")));
            let child = child_env(&cleared);
            assert_eq!(get(&child, VAR), None, "the check runs without it too");
        }
        // …and the env file is applied after clear_env; the shell PATH replaces the GUI's.
        let both = Inputs {
            inherited: gui,
            env_file: env(&[(VAR, "/profile/codex")]),
            clear: vec![VAR.to_owned()],
            shell_path: Some("/shell/bin".to_owned()),
        };
        let child = child_env(&both);
        assert_eq!(resolve(&child), Ok(PathBuf::from("/profile/codex")));
        assert_eq!(get(&child, "PATH"), Some("/shell/bin"));
        // An inherited approval socket never reaches the child.
        let socket = Inputs {
            inherited: env(&[(agent_core::approval::ENV_SOCKET, "/run/x"), ("HOME", "/h")]),
            ..Inputs::default()
        };
        assert_eq!(
            get(&child_env(&socket), agent_core::approval::ENV_SOCKET),
            None
        );
        // Empty counts as unset.
        let empty = Inputs {
            inherited: env(&[("HOME", "/home/u"), (VAR, "")]),
            ..Inputs::default()
        };
        assert_eq!(home_of(&empty), Ok(PathBuf::from("/home/u/.codex")));
    }

    #[test]
    fn an_unknowable_home_is_an_error_not_a_guess() {
        let relative = Inputs {
            inherited: env(&[("HOME", "/h"), (VAR, "rel/codex")]),
            ..Inputs::default()
        };
        assert!(home_of(&relative).is_err());
        assert!(home_of(&Inputs::default()).is_err());
        let relative_home = Inputs {
            inherited: env(&[("HOME", "h")]),
            ..Inputs::default()
        };
        assert!(home_of(&relative_home).is_err());
    }
}
