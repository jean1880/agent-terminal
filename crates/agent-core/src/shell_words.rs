//! Small readings of one shell word, shared by every command-line reader.

/// The last `/`-separated part of `word`: the program a path names.
pub fn basename(word: &str) -> &str {
    word.rsplit('/').next().unwrap_or(word)
}

/// Whether `word` is a shell variable assignment (`NAME=value`): a name of ASCII letters,
/// digits and `_`, not starting with a digit.
pub fn is_assignment(word: &str) -> bool {
    word.split_once('=').is_some_and(|(name, _)| {
        !name.is_empty()
            && !name.starts_with(|c: char| c.is_ascii_digit())
            && name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn basename_takes_the_last_part() {
        assert_eq!(basename("/usr/bin/git"), "git");
        assert_eq!(basename("git"), "git");
        assert_eq!(basename("dir/"), "");
    }

    #[test]
    fn assignments_need_a_valid_name() {
        assert!(is_assignment("A=1"));
        assert!(is_assignment("_x9="));
        assert!(!is_assignment("=1"));
        assert!(!is_assignment("9A=1"));
        assert!(!is_assignment("A-B=1"));
        assert!(!is_assignment("plain"));
    }
}
