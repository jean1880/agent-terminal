//! Pure logic for the diff panel: which two trees to compare, how to read
//! `git diff --numstat -z`, how to colour each line of a unified diff, and how
//! to cap a diff too large to show.
//!
//! Nothing here runs git; `crate::git` does, and the window renders.

/// What the panel compares against.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum DiffBase {
    /// HEAD against the working tree as it is now (untracked files included).
    #[default]
    Uncommitted,
    /// The tab's latest checkpoint against the one before it.
    LastTurn,
    /// Where the tab's first checkpoint started against the working tree now.
    ThisTab,
}

impl DiffBase {
    /// Dropdown order. Guarded by `all_lists_every_base_in_dropdown_order`.
    pub const ALL: [DiffBase; 3] = [DiffBase::Uncommitted, DiffBase::LastTurn, DiffBase::ThisTab];

    pub fn label(self) -> &'static str {
        match self {
            DiffBase::Uncommitted => "Uncommitted",
            DiffBase::LastTurn => "Last turn",
            DiffBase::ThisTab => "This tab",
        }
    }
}

/// A checkpoint as the panel needs it: its commit and where it started.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CheckpointCommit {
    pub commit: String,
    /// `None` for a checkpoint taken before the repository's first commit.
    pub parent: Option<String>,
}

/// Why a base cannot be shown yet.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Unavailable {
    NoCheckpoints,
}

/// The two tree-ishes to diff for `base`.
///
/// `checkpoints` are the tab's, oldest first; `current` is a fresh snapshot
/// tree of the working tree; `empty_tree` stands in for a missing parent (an
/// unborn HEAD), since it differs between SHA-1 and SHA-256 repositories and
/// must be asked of git rather than hard-coded.
pub fn diff_range(
    base: DiffBase,
    head: Option<&str>,
    checkpoints: &[CheckpointCommit],
    current: &str,
    empty_tree: &str,
) -> Result<(String, String), Unavailable> {
    let or_empty = |rev: Option<&str>| rev.unwrap_or(empty_tree).to_string();
    match base {
        DiffBase::Uncommitted => Ok((or_empty(head), current.to_string())),
        DiffBase::LastTurn => {
            let last = checkpoints.last().ok_or(Unavailable::NoCheckpoints)?;
            Ok((or_empty(last.parent.as_deref()), last.commit.clone()))
        }
        DiffBase::ThisTab => {
            let first = checkpoints.first().ok_or(Unavailable::NoCheckpoints)?;
            Ok((or_empty(first.parent.as_deref()), current.to_string()))
        }
    }
}

/// One file's line counts from `git diff --numstat`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileStat {
    pub path: String,
    /// Set for a rename or copy.
    pub old_path: Option<String>,
    /// `None` for a binary file, which git counts as `-`.
    pub added: Option<u64>,
    pub deleted: Option<u64>,
}

/// Parses `git diff --numstat -z`.
///
/// A plain entry is `added\tdeleted\tpath\0`. A rename leaves the path empty
/// and follows with two more NUL-terminated fields: `added\tdeleted\t\0old\0new\0`.
pub fn parse_numstat(bytes: &[u8]) -> Vec<FileStat> {
    let mut fields = bytes.split(|b| *b == 0);
    let mut stats = Vec::new();
    while let Some(field) = fields.next() {
        if field.is_empty() {
            continue;
        }
        let text = String::from_utf8_lossy(field);
        let mut parts = text.splitn(3, '\t');
        let (Some(added), Some(deleted), Some(path)) = (parts.next(), parts.next(), parts.next())
        else {
            continue;
        };
        let count = |n: &str| n.parse::<u64>().ok();
        let (path, old_path) = if path.is_empty() {
            let old = fields
                .next()
                .map(|f| String::from_utf8_lossy(f).to_string());
            let new = fields
                .next()
                .map(|f| String::from_utf8_lossy(f).to_string());
            match (old, new) {
                (Some(old), Some(new)) => (new, Some(old)),
                _ => continue,
            }
        } else {
            (path.to_string(), None)
        };
        stats.push(FileStat {
            path,
            old_path,
            added: count(added),
            deleted: count(deleted),
        });
    }
    stats
}

/// Totals for the panel header: files, lines added, lines deleted.
pub fn totals(stats: &[FileStat]) -> (usize, u64, u64) {
    let added = stats.iter().filter_map(|s| s.added).sum();
    let deleted = stats.iter().filter_map(|s| s.deleted).sum();
    (stats.len(), added, deleted)
}

/// The panel header's one-line summary, e.g. `3 files, +12 −4`.
pub fn summary(stats: &[FileStat]) -> String {
    let (files, added, deleted) = totals(stats);
    let noun = if files == 1 { "file" } else { "files" };
    format!("{files} {noun}, +{added} −{deleted}")
}

/// One row of the file list, e.g. `src/a.rs  +3 −1`, `old → new  +0 −0`, or
/// `logo.png  binary`.
pub fn file_row(stat: &FileStat) -> String {
    let name = match &stat.old_path {
        Some(old) => format!("{old} → {}", stat.path),
        None => stat.path.clone(),
    };
    match (stat.added, stat.deleted) {
        (Some(added), Some(deleted)) => format!("{name}  +{added} −{deleted}"),
        _ => format!("{name}  binary"),
    }
}

/// How a line of a unified diff is coloured.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LineKind {
    /// `diff --git …`, where each file starts.
    File,
    /// `index`, `---`, `+++`, mode and rename lines between a file header and
    /// its first hunk.
    Meta,
    Hunk,
    Added,
    Removed,
    Context,
}

/// Classifies each line of a unified diff.
///
/// Stateful rather than per-line: a removed line whose text starts with `--`
/// reads `--- …`, exactly like a file's old-path line. Only lines before a
/// file's first hunk are meta.
pub fn classify(diff: &str) -> Vec<LineKind> {
    let mut in_header = false;
    diff.lines()
        .map(|line| {
            if line.starts_with("diff --git ") {
                in_header = true;
                LineKind::File
            } else if line.starts_with("@@") {
                in_header = false;
                LineKind::Hunk
            } else if in_header {
                LineKind::Meta
            } else if line.starts_with('+') {
                LineKind::Added
            } else if line.starts_with('-') {
                LineKind::Removed
            } else if line.starts_with('\\') {
                // "\ No newline at end of file"
                LineKind::Meta
            } else {
                LineKind::Context
            }
        })
        .collect()
}

/// At most this much diff text is put in the panel.
pub const MAX_DIFF_BYTES: usize = 1024 * 1024;
/// At most this many lines of diff text are put in the panel.
pub const MAX_DIFF_LINES: usize = 20_000;

/// Cuts `diff` to the first `max_lines` lines and `max_bytes` bytes, never
/// splitting a line or a character. Returns the kept text and how many lines
/// were left out.
pub fn truncate(diff: &str, max_bytes: usize, max_lines: usize) -> (&str, usize) {
    let mut end = 0;
    let mut kept = 0;
    for line in diff.split_inclusive('\n') {
        if kept == max_lines || end + line.len() > max_bytes {
            break;
        }
        end += line.len();
        kept += 1;
    }
    let total = diff.split_inclusive('\n').count();
    (&diff[..end], total - kept)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn all_lists_every_base_in_dropdown_order() {
        // Adding a variant must fail to compile here until ALL is updated.
        for (i, base) in DiffBase::ALL.iter().enumerate() {
            let expected = match base {
                DiffBase::Uncommitted => 0,
                DiffBase::LastTurn => 1,
                DiffBase::ThisTab => 2,
            };
            assert_eq!(i, expected);
        }
    }

    fn cp(commit: &str, parent: Option<&str>) -> CheckpointCommit {
        CheckpointCommit {
            commit: commit.into(),
            parent: parent.map(Into::into),
        }
    }

    #[test]
    fn ranges_follow_the_base() {
        let cps = [cp("c1", Some("head")), cp("c2", Some("c1"))];
        let range = |base, head, cps: &[CheckpointCommit]| diff_range(base, head, cps, "now", "E");
        assert_eq!(
            range(DiffBase::Uncommitted, Some("head"), &cps),
            Ok(("head".into(), "now".into()))
        );
        assert_eq!(
            range(DiffBase::LastTurn, Some("head"), &cps),
            Ok(("c1".into(), "c2".into()))
        );
        assert_eq!(
            range(DiffBase::ThisTab, Some("head"), &cps),
            Ok(("head".into(), "now".into()))
        );
    }

    #[test]
    fn a_missing_parent_or_head_diffs_against_the_empty_tree() {
        let cps = [cp("c1", None)];
        let range = |base, head| diff_range(base, head, &cps, "now", "E");
        assert_eq!(
            range(DiffBase::Uncommitted, None),
            Ok(("E".into(), "now".into()))
        );
        assert_eq!(
            range(DiffBase::LastTurn, None),
            Ok(("E".into(), "c1".into()))
        );
        assert_eq!(
            range(DiffBase::ThisTab, None),
            Ok(("E".into(), "now".into()))
        );
    }

    #[test]
    fn checkpoint_bases_need_a_checkpoint() {
        for base in [DiffBase::LastTurn, DiffBase::ThisTab] {
            assert_eq!(
                diff_range(base, Some("h"), &[], "now", "E"),
                Err(Unavailable::NoCheckpoints)
            );
        }
    }

    #[test]
    fn numstat_reads_plain_binary_and_renamed_entries() {
        let raw = b"3\t1\tsrc/a.rs\0-\t-\tlogo.png\x000\t0\t\0old name.txt\0new name.txt\0";
        assert_eq!(
            parse_numstat(raw),
            vec![
                FileStat {
                    path: "src/a.rs".into(),
                    old_path: None,
                    added: Some(3),
                    deleted: Some(1),
                },
                FileStat {
                    path: "logo.png".into(),
                    old_path: None,
                    added: None,
                    deleted: None,
                },
                FileStat {
                    path: "new name.txt".into(),
                    old_path: Some("old name.txt".into()),
                    added: Some(0),
                    deleted: Some(0),
                },
            ]
        );
        assert_eq!(totals(&parse_numstat(raw)), (3, 3, 1));
        assert!(parse_numstat(b"").is_empty());
    }

    #[test]
    fn summary_and_rows_read_naturally() {
        let raw = b"3\t1\tsrc/a.rs\0-\t-\tlogo.png\x000\t0\t\0old.txt\0new.txt\0";
        let stats = parse_numstat(raw);
        assert_eq!(summary(&stats), "3 files, +3 −1");
        assert_eq!(summary(&stats[..1]), "1 file, +3 −1");
        assert_eq!(file_row(&stats[0]), "src/a.rs  +3 −1");
        assert_eq!(file_row(&stats[1]), "logo.png  binary");
        assert_eq!(file_row(&stats[2]), "old.txt → new.txt  +0 −0");
    }

    #[test]
    fn a_removed_line_that_looks_like_a_header_is_still_removed() {
        let diff = "diff --git a/x b/x\nindex 1..2 100644\n--- a/x\n+++ b/x\n@@ -1,2 +1,2 @@\n--- not a header\n+++ nor this\n ctx\n\\ No newline at end of file\n";
        assert_eq!(
            classify(diff),
            vec![
                LineKind::File,
                LineKind::Meta,
                LineKind::Meta,
                LineKind::Meta,
                LineKind::Hunk,
                LineKind::Removed,
                LineKind::Added,
                LineKind::Context,
                LineKind::Meta,
            ]
        );
    }

    #[test]
    fn truncation_keeps_whole_lines_and_counts_the_rest() {
        let diff = "a\nbb\nccc\n";
        assert_eq!(truncate(diff, 100, 100), (diff, 0));
        assert_eq!(truncate(diff, 100, 2), ("a\nbb\n", 1));
        assert_eq!(truncate(diff, 6, 100), ("a\nbb\n", 1));
        // A multi-byte line that does not fit is dropped whole, never split.
        let wide = "é\n日本語\n";
        assert_eq!(truncate(wide, 5, 100), ("é\n", 1));
    }
}
