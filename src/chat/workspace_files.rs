//! The app's own file index for the composer's `@` popover.
//!
//! Claude answers `Control::FileSuggestions` itself; agy and Codex do not, so for them (and
//! only them, `Capabilities::file_suggestions == false`) the app lists the thread's folder. The
//! listing is blocking process and file I/O: callers run [`suggest`] on a worker thread
//! (`gio::spawn_blocking`). [`rank`] is pure.

use std::path::Path;

/// What the popover can show: `payload::file_suggestions` keeps this many.
pub const MAX_SUGGESTIONS: usize = 50;
/// The walk fallback (no git) stops here, so a huge folder cannot stall the worker.
const MAX_WALKED: usize = 20_000;
const MAX_DEPTH: usize = 8;
/// Never descended into by the walk fallback; git's own listing already honours .gitignore.
const SKIP_DIRS: &[&str] = &["node_modules", "target", "dist", "build", "__pycache__"];

/// Every file under `root` (relative paths), tracked and untracked-but-not-ignored when `root`
/// is in a git repository, else a bounded walk. Secret-looking paths are never listed.
pub fn list(root: &Path) -> Vec<String> {
    let mut files = match crate::git::git_raw(
        root,
        [
            "ls-files",
            "-z",
            "--cached",
            "--others",
            "--exclude-standard",
        ],
        &[],
        5,
    ) {
        Ok(out) => out
            .split(|b| *b == 0)
            .filter(|e| !e.is_empty() && !e.ends_with(b"/"))
            .map(|e| String::from_utf8_lossy(e).into_owned())
            .collect(),
        Err(_) => walk(root),
    };
    files.retain(|f| !crate::git::is_secret_path(Path::new(f)));
    files
}

fn walk(root: &Path) -> Vec<String> {
    let mut out = Vec::new();
    let mut stack = vec![(root.to_path_buf(), 0usize)];
    while let Some((dir, depth)) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            if out.len() >= MAX_WALKED {
                return out;
            }
            let name = entry.file_name().to_string_lossy().into_owned();
            let Ok(kind) = entry.file_type() else {
                continue;
            };
            if kind.is_dir() {
                // Hidden folders (.git, .cache, ...) and build output are not worth listing.
                if depth < MAX_DEPTH
                    && !name.starts_with('.')
                    && !SKIP_DIRS.contains(&name.as_str())
                {
                    stack.push((entry.path(), depth + 1));
                }
            } else if kind.is_file() {
                if let Ok(relative) = entry.path().strip_prefix(root) {
                    out.push(relative.to_string_lossy().into_owned());
                }
            }
        }
    }
    out
}

/// The paths that match `query`, best first, at most [`MAX_SUGGESTIONS`]. A match is the query's
/// characters in order (case-insensitive) in the path; a file-name match beats a directory
/// match, a prefix beats a substring, and shorter paths come first. An empty query lists the
/// shallowest files.
pub fn rank(files: &[String], query: &str) -> Vec<String> {
    let query = query.to_lowercase();
    let mut scored: Vec<(u32, usize, &String)> = files
        .iter()
        .filter_map(|path| score(path, &query).map(|s| (s, path.len(), path)))
        .collect();
    scored.sort();
    scored
        .into_iter()
        .take(MAX_SUGGESTIONS)
        .map(|(_, _, path)| path.clone())
        .collect()
}

/// Lower is better; `None` is no match.
fn score(path: &str, query: &str) -> Option<u32> {
    if query.is_empty() {
        // Shallow files first.
        return Some(u32::try_from(path.matches('/').count()).unwrap_or(u32::MAX));
    }
    let lower = path.to_lowercase();
    let name = lower.rsplit('/').next().unwrap_or(&lower);
    if name.starts_with(query) {
        return Some(0);
    }
    if lower.starts_with(query) {
        return Some(1);
    }
    if name.contains(query) {
        return Some(2);
    }
    if lower.contains(query) {
        return Some(3);
    }
    in_order(&lower, query).then_some(4)
}

fn in_order(haystack: &str, needle: &str) -> bool {
    let mut rest = haystack.chars();
    needle.chars().all(|c| rest.any(|h| h == c))
}

/// The popover's list for `query` in `root`: [`list`], then [`rank`].
pub fn suggest(root: &Path, query: &str) -> Vec<String> {
    rank(&list(root), query)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn files(names: &[&str]) -> Vec<String> {
        names.iter().map(|n| (*n).to_owned()).collect()
    }

    #[test]
    fn matches_are_ranked_name_prefix_first_then_shorter_paths() {
        let all = files(&[
            "docs/session-notes.md",
            "src/chat/session.rs",
            "session.rs",
            "src/main.rs",
        ]);
        assert_eq!(
            rank(&all, "sess"),
            ["session.rs", "src/chat/session.rs", "docs/session-notes.md"]
        );
        assert_eq!(rank(&all, "SRC/M"), ["src/main.rs"]);
        // Characters in order, anywhere.
        assert_eq!(rank(&all, "smr"), ["src/main.rs"]);
        assert!(rank(&all, "zzz").is_empty());
    }

    #[test]
    fn an_empty_query_lists_the_shallowest_files_and_is_capped() {
        let mut all = files(&["a/b/c.rs", "top.rs", "a/mid.rs"]);
        assert_eq!(rank(&all, ""), ["top.rs", "a/mid.rs", "a/b/c.rs"]);
        all.extend((0..200).map(|i| format!("f{i}.txt")));
        assert_eq!(rank(&all, "").len(), MAX_SUGGESTIONS);
    }

    #[test]
    fn a_folder_lists_its_files_without_secrets_or_hidden_or_build_output() {
        let tmp = tempfile::tempdir().expect("tmp");
        let root = tmp.path();
        std::fs::create_dir_all(root.join("src")).expect("mkdir");
        std::fs::create_dir_all(root.join("target")).expect("mkdir");
        std::fs::create_dir_all(root.join(".hidden")).expect("mkdir");
        for f in [
            "src/lib.rs",
            "README.md",
            "target/out.o",
            ".hidden/x",
            ".env",
        ] {
            std::fs::write(root.join(f), "x").expect("write");
        }
        let mut found = list(root);
        found.sort();
        assert_eq!(found, ["README.md", "src/lib.rs"]);
        assert_eq!(suggest(root, "lib"), ["src/lib.rs"]);
    }
}
