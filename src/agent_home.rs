//! The home folder in an agent's launch environment. Profile variables are applied after
//! unsets, so even an empty profile `HOME` shadows the app's own value.

use std::path::PathBuf;

use crate::agent_proc::AgentEnv;

pub fn effective_home(env: &AgentEnv) -> Option<PathBuf> {
    home_in_child(&child_env(env))
}

pub fn child_env(env: &AgentEnv) -> Vec<(String, String)> {
    child_env_from_os(env, std::env::vars_os())
}

fn child_env_from_os(
    env: &AgentEnv,
    inherited: impl IntoIterator<Item = (std::ffi::OsString, std::ffi::OsString)>,
) -> Vec<(String, String)> {
    let inherited = inherited
        .into_iter()
        .filter_map(|(key, value)| {
            // The child inherits the original bytes. A lossy HOME could prove a different folder;
            // leave it unknown, while still retaining redirect-variable names and profile overrides.
            if key == "HOME" && value.to_str().is_none() {
                return None;
            }
            Some((
                key.to_string_lossy().into_owned(),
                value.to_string_lossy().into_owned(),
            ))
        })
        .collect();
    child_env_with(env, inherited)
}

fn child_env_with(env: &AgentEnv, inherited: Vec<(String, String)>) -> Vec<(String, String)> {
    crate::codex_home::child_env(&crate::codex_home::Inputs {
        inherited,
        env_file: env.env.clone(),
        clear: env.unset.clone(),
        ..crate::codex_home::Inputs::default()
    })
}

/// The same path policy as Codex's session grants: ambiguous components cannot prove a home.
pub fn home_in_child(child: &[(String, String)]) -> Option<PathBuf> {
    let home = crate::codex_home::get(child, "HOME").map(PathBuf::from)?;
    agent_core::codex::is_plain_absolute(&home).then_some(home)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn resolve(env: &AgentEnv, inherited: Option<PathBuf>) -> Option<PathBuf> {
        let inherited = inherited
            .map(|home| vec![("HOME".into(), home.to_string_lossy().into_owned())])
            .unwrap_or_default();
        home_in_child(&child_env_with(env, inherited))
    }

    #[test]
    fn profile_home_wins_after_unsets_and_the_last_value_wins() {
        let mut env = AgentEnv {
            env: vec![("HOME".into(), "/profile".into())],
            unset: vec!["HOME".into()],
        };
        assert_eq!(resolve(&env, Some("/app".into())), Some("/profile".into()));
        env.env.push(("HOME".into(), "/last".into()));
        assert_eq!(resolve(&env, Some("/app".into())), Some("/last".into()));
        for value in ["", "relative"] {
            env.env.push(("HOME".into(), value.into()));
            assert_eq!(resolve(&env, Some("/app".into())), None);
        }
    }

    #[test]
    fn inherited_home_is_used_only_when_it_reaches_the_child() {
        let mut env = AgentEnv::default();
        assert_eq!(resolve(&env, Some("/app".into())), Some("/app".into()));
        assert_eq!(resolve(&env, None), None);
        assert_eq!(resolve(&env, Some("relative".into())), None);
        env.unset.push("HOME".into());
        assert_eq!(resolve(&env, Some("/app".into())), None);
    }

    #[test]
    fn home_uses_the_shared_plain_absolute_policy() {
        for value in ["/app/../profile", ".", "..", "relative/home"] {
            let env = AgentEnv {
                env: vec![("HOME".into(), value.into())],
                unset: vec![],
            };
            assert_eq!(resolve(&env, Some("/app".into())), None, "{value}");
        }
        // Path components normalise interior '.', just as Codex's grant policy does.
        let env = AgentEnv {
            env: vec![("HOME".into(), "/app/./profile".into())],
            unset: vec![],
        };
        assert_eq!(resolve(&env, None), Some("/app/./profile".into()));
    }

    #[test]
    fn a_non_unicode_inherited_home_cannot_prove_a_different_folder() {
        use std::os::unix::ffi::OsStringExt;
        let inherited = vec![
            (
                "HOME".into(),
                std::ffi::OsString::from_vec(b"/home/\xff".to_vec()),
            ),
            (
                "XDG_CONFIG_HOME".into(),
                std::ffi::OsString::from_vec(b"/config/\xff".to_vec()),
            ),
        ];
        let child = child_env_from_os(&AgentEnv::default(), inherited.clone());
        assert_eq!(home_in_child(&child), None);
        assert!(
            child.iter().any(|(key, _)| key == "XDG_CONFIG_HOME"),
            "redirect presence is still known"
        );
        let profile = AgentEnv {
            env: vec![("HOME".into(), "/profile/home".into())],
            unset: vec![],
        };
        assert_eq!(
            home_in_child(&child_env_from_os(&profile, inherited)),
            Some("/profile/home".into())
        );
    }
}
