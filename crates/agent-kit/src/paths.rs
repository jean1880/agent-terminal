//! Path helpers shared by the session and hand-off readers.

/// Expands a leading `~` to `$HOME`.
///
/// Config values are hand-written, so people write `~/...` and expect it to work.
/// Nothing else does this for us: `read_to_string` takes the tilde literally, and
/// a path passed to a shell in quotes is not expanded either.
pub fn expand_tilde(path: &str) -> String {
    match std::env::var("HOME") {
        Ok(home) => expand_tilde_with(path, &home),
        Err(_) => path.trim().to_string(),
    }
}

/// [`expand_tilde`] against an explicit home directory. The one place `~` is
/// interpreted, so the settings dialog, the spawn path and config values all
/// agree on what a path means.
pub fn expand_tilde_with(path: &str, home: &str) -> String {
    let trimmed = path.trim();
    let Some(rest) = trimmed.strip_prefix('~') else {
        return trimmed.to_string();
    };
    // Only a bare `~` or `~/`; `~user` is someone else's home and not ours to guess.
    if !rest.is_empty() && !rest.starts_with('/') {
        return trimmed.to_string();
    }
    format!("{home}{rest}")
}

/// `path` with `home` written as `~`, for showing a path to people. An empty `home` (unknown)
/// leaves every path as it is rather than turning each into `~/…`.
pub fn tilde(path: &std::path::Path, home: &std::path::Path) -> String {
    match path.strip_prefix(home) {
        Ok(rest) if !home.as_os_str().is_empty() => {
            if rest.as_os_str().is_empty() {
                "~".to_owned()
            } else {
                format!("~/{}", rest.display())
            }
        }
        _ => path.display().to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    #[test]
    fn a_path_under_home_is_shown_with_a_tilde_and_an_unknown_home_changes_nothing() {
        let home = Path::new("/home/u");
        assert_eq!(tilde(Path::new("/home/u"), home), "~");
        assert_eq!(tilde(Path::new("/home/u/a/b"), home), "~/a/b");
        assert_eq!(tilde(Path::new("/home/user2/a"), home), "/home/user2/a");
        assert_eq!(tilde(Path::new("/etc/x"), home), "/etc/x");
        assert_eq!(tilde(Path::new("rel/x"), Path::new("")), "rel/x");
    }

    #[test]
    fn tilde_paths_are_expanded_for_config_supplied_files() {
        // People write ~/... in config; nothing else expands it for us, and a
        // path handed to a shell in quotes stays literal.
        let home = std::env::var("HOME").unwrap();
        assert_eq!(
            expand_tilde("~/reports/drift.txt"),
            format!("{home}/reports/drift.txt")
        );
        assert_eq!(expand_tilde("~"), home);
        assert_eq!(expand_tilde("/absolute/path"), "/absolute/path");
        assert_eq!(expand_tilde("  ~/spaced  "), format!("{home}/spaced"));
        // ~otheruser is someone else's home and not ours to guess at.
        assert_eq!(expand_tilde("~root/x"), "~root/x");
    }
}
