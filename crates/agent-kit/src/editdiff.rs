//! Pure unified-diff text for file edits: a line differ, and readers that turn an agent's tool
//! input (Claude `Edit`/`Write`/`MultiEdit`, agy `replace_file_content`/`write_to_file`, Codex
//! `fileChange`) into the diff it proposes.
//!
//! Nothing here touches git, the disk or GTK. It serves two callers: the per-file "View diff"
//! (the working file against the pre-turn checkpoint, both sides read by [`crate::filediff`]),
//! and the fallback and approval preview, which are computed from the request alone.
//!
//! Ceiling: the differ is Myers' O(ND) with the edit distance capped at [`MAX_EDIT_DISTANCE`];
//! a change past it is shown as one block replaced, which is correct but unrefined. Upgrade path:
//! the `similar` crate's patience differ.
//!
//! The agy and Codex input shapes read here are the documented parameter names; an input in a
//! shape nobody has seen yields no preview (callers then show the raw JSON), never a wrong one.

use serde_json::Value;

/// Lines of context around a change when diffing two whole files.
const CONTEXT: usize = 3;

/// Past this many differing lines the differ stops refining (memory is O(D²)).
pub const MAX_EDIT_DISTANCE: usize = 1_000;

/// At most this many bytes of either side are diffed: a bigger `Write` is shown cut, not read in
/// full.
const MAX_SIDE_BYTES: usize = 512 * 1024;

/// One file's diff as a ready unified-diff text.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileEdit {
    pub path: String,
    /// `--- a/…`, `+++ b/…`, then hunks.
    pub diff: String,
    pub added: usize,
    pub removed: usize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Op {
    Keep,
    Del,
    Ins,
}

/// A line of text: its content and whether a newline ended it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Line<'a> {
    text: &'a str,
    newline: bool,
}

fn lines(text: &str) -> Vec<Line<'_>> {
    text.split_inclusive('\n')
        .map(|l| match l.strip_suffix('\n') {
            Some(text) => Line {
                text,
                newline: true,
            },
            None => Line {
                text: l,
                newline: false,
            },
        })
        .collect()
}

/// Myers' diff of `a` against `b`: one op per line of the merged sequence. `None` when the edit
/// distance passes `max_d`.
fn myers(a: &[Line<'_>], b: &[Line<'_>], max_d: usize) -> Option<Vec<Op>> {
    let (n, m) = (a.len() as isize, b.len() as isize);
    let limit = (a.len() + b.len()).min(max_d) as isize;
    let offset = limit + 1;
    let mut v = vec![0isize; (2 * limit + 3) as usize];
    let mut trace: Vec<Vec<isize>> = Vec::new();
    for d in 0..=limit {
        trace.push(v.clone());
        let mut k = -d;
        while k <= d {
            let idx = (k + offset) as usize;
            let mut x = if k == -d || (k != d && v[idx - 1] < v[idx + 1]) {
                v[idx + 1]
            } else {
                v[idx - 1] + 1
            };
            let mut y = x - k;
            while x < n && y < m && a[x as usize] == b[y as usize] {
                x += 1;
                y += 1;
            }
            v[idx] = x;
            if x >= n && y >= m {
                return Some(backtrack(&trace, n, m, d, offset));
            }
            k += 2;
        }
    }
    None
}

fn backtrack(trace: &[Vec<isize>], n: isize, m: isize, last: isize, offset: isize) -> Vec<Op> {
    let at = |v: &[isize], k: isize| v[(k + offset) as usize];
    let mut ops = Vec::new();
    let (mut x, mut y) = (n, m);
    for d in (1..=last).rev() {
        let v = &trace[d as usize];
        let k = x - y;
        let prev_k = if k == -d || (k != d && at(v, k - 1) < at(v, k + 1)) {
            k + 1
        } else {
            k - 1
        };
        let prev_x = at(v, prev_k);
        let prev_y = prev_x - prev_k;
        while x > prev_x && y > prev_y {
            ops.push(Op::Keep);
            x -= 1;
            y -= 1;
        }
        ops.push(if x == prev_x { Op::Ins } else { Op::Del });
        x = prev_x;
        y = prev_y;
    }
    while x > 0 && y > 0 {
        ops.push(Op::Keep);
        x -= 1;
        y -= 1;
    }
    ops.reverse();
    ops
}

/// The ops turning `a` into `b`: shared head and tail are peeled off first (most edits are small
/// against a big file), the middle goes to Myers, and a middle past the cap is one replaced block.
fn diff_ops(a: &[Line<'_>], b: &[Line<'_>]) -> Vec<Op> {
    let head = a.iter().zip(b).take_while(|(x, y)| x == y).count();
    let tail = a[head..]
        .iter()
        .rev()
        .zip(b[head..].iter().rev())
        .take_while(|(x, y)| x == y)
        .count();
    let (am, bm) = (&a[head..a.len() - tail], &b[head..b.len() - tail]);
    let middle = myers(am, bm, MAX_EDIT_DISTANCE).unwrap_or_else(|| {
        std::iter::repeat_n(Op::Del, am.len())
            .chain(std::iter::repeat_n(Op::Ins, bm.len()))
            .collect()
    });
    let mut ops = vec![Op::Keep; head];
    ops.extend(middle);
    ops.extend(std::iter::repeat_n(Op::Keep, tail));
    ops
}

/// How the hunk headers number lines.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Numbering {
    /// Real line numbers: both sides are whole files.
    Real,
    /// A fragment of a file whose position is not known (an `old_string`): `@@ edit @@`.
    Fragment,
}

/// `text` cut to at most [`MAX_SIDE_BYTES`] at a line boundary.
fn cut(text: &str) -> &str {
    if text.len() <= MAX_SIDE_BYTES {
        return text;
    }
    let mut end = MAX_SIDE_BYTES;
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    match text[..end].rfind('\n') {
        Some(nl) => &text[..=nl],
        None => &text[..end],
    }
}

/// The path as a diff header shows it: an absolute one loses its leading `/`, so `a/` and `b/`
/// stay prefixes rather than making `a//w/…`.
fn shown(path: &str) -> &str {
    path.trim_start_matches('/')
}

/// One merged line of a diff with where it sits on each side (0-based).
struct Step<'a> {
    op: Op,
    line: Line<'a>,
    old: usize,
    new: usize,
}

/// A unified diff of `old` → `new` (headers included) with its added/removed line counts.
/// `old: None` is a new file. Equal inputs give a diff with headers and no hunks.
pub fn unified(path: &str, old: Option<&str>, new: &str, numbering: Numbering) -> FileEdit {
    // A fragment (`old_string`) rarely ends in a newline and says nothing about the file's own
    // ending, so the marker would only be noise: both sides are given one.
    let terminated = |text: &str| {
        let text = cut(text);
        if numbering == Numbering::Fragment && !text.is_empty() && !text.ends_with('\n') {
            format!("{text}\n")
        } else {
            text.to_owned()
        }
    };
    let (old_text, new_text) = (terminated(old.unwrap_or("")), terminated(new));
    let old_lines = lines(&old_text);
    let new_lines = lines(&new_text);
    let ops = diff_ops(&old_lines, &new_lines);

    let mut steps: Vec<Step<'_>> = Vec::with_capacity(ops.len());
    let (mut o, mut n) = (0usize, 0usize);
    for op in ops {
        let line = match op {
            Op::Keep | Op::Del => old_lines[o],
            Op::Ins => new_lines[n],
        };
        steps.push(Step {
            op,
            line,
            old: o,
            new: n,
        });
        match op {
            Op::Keep => {
                o += 1;
                n += 1;
            }
            Op::Del => o += 1,
            Op::Ins => n += 1,
        }
    }

    let p = shown(path);
    let mut out = String::new();
    match old {
        Some(_) => out.push_str(&format!("--- a/{p}\n")),
        None => out.push_str("--- /dev/null\n"),
    }
    out.push_str(&format!("+++ b/{p}\n"));

    // A fragment is already its own context: it is shown whole, as one hunk.
    let context = match numbering {
        Numbering::Real => CONTEXT,
        Numbering::Fragment => usize::MAX,
    };
    let changed: Vec<usize> = steps
        .iter()
        .enumerate()
        .filter(|(_, s)| s.op != Op::Keep)
        .map(|(i, _)| i)
        .collect();
    let (mut added, mut removed) = (0, 0);
    let mut i = 0;
    while i < changed.len() {
        let first = changed[i];
        let mut last = first;
        while i + 1 < changed.len() && changed[i + 1] - last <= context.saturating_mul(2) {
            i += 1;
            last = changed[i];
        }
        i += 1;
        let start = first.saturating_sub(context);
        let end = last.saturating_add(context).min(steps.len() - 1);
        let hunk = &steps[start..=end];
        let old_count = hunk.iter().filter(|s| s.op != Op::Ins).count();
        let new_count = hunk.iter().filter(|s| s.op != Op::Del).count();
        match numbering {
            Numbering::Real => {
                let old_start = hunk[0].old + usize::from(old_count > 0);
                let new_start = hunk[0].new + usize::from(new_count > 0);
                out.push_str(&format!(
                    "@@ -{old_start},{old_count} +{new_start},{new_count} @@\n"
                ));
            }
            Numbering::Fragment => out.push_str("@@ edit @@\n"),
        }
        for s in hunk {
            let sign = match s.op {
                Op::Keep => ' ',
                Op::Del => {
                    removed += 1;
                    '-'
                }
                Op::Ins => {
                    added += 1;
                    '+'
                }
            };
            out.push(sign);
            out.push_str(s.line.text);
            out.push('\n');
            if !s.line.newline {
                out.push_str("\\ No newline at end of file\n");
            }
        }
    }
    FileEdit {
        path: path.to_owned(),
        diff: out,
        added,
        removed,
    }
}

/// Whole-file diff of `old` (absent: the file is new) against `new`.
pub fn file_diff(path: &str, old: Option<&str>, new: &str) -> FileEdit {
    unified(path, old, new, Numbering::Real)
}

// ---------------------------------------------------------------------------------------------
// Reading an agent's tool input
// ---------------------------------------------------------------------------------------------

fn str_of<'a>(v: &'a Value, keys: &[&str]) -> Option<&'a str> {
    keys.iter().find_map(|k| v.get(*k).and_then(Value::as_str))
}

fn path_of(v: &Value) -> Option<&str> {
    str_of(
        v,
        &[
            "file_path",
            "TargetFile",
            "path",
            "AbsolutePath",
            "notebook_path",
        ],
    )
    .filter(|p| !p.trim().is_empty())
}

/// Replacement pairs: Claude `old_string`/`new_string`, agy `TargetContent`/`ReplacementContent`.
fn pair_of(v: &Value) -> Option<(&str, &str)> {
    let old = str_of(v, &["old_string", "TargetContent"])?;
    let new = str_of(v, &["new_string", "ReplacementContent"])?;
    Some((old, new))
}

/// What the input proposes, one entry per file. Empty when the input is not a file edit this
/// module understands.
///
/// | Input | Preview |
/// |---|---|
/// | `old_string`/`new_string`, `TargetContent`/`ReplacementContent` | the pair as one `@@ edit @@` hunk |
/// | `edits` (MultiEdit), `ReplacementChunks` | one hunk per pair |
/// | `content` with a path (Write), `CodeContent`, `new_source` | the whole text added |
/// | a list of `{path, kind, diff}` (Codex `fileChange`), or `{changes: [...]}` | the diff as given |
pub fn preview_from_input(input: &Value) -> Vec<FileEdit> {
    let changes = match input {
        Value::Array(list) => Some(list.as_slice()),
        other => other
            .get("changes")
            .and_then(Value::as_array)
            .map(Vec::as_slice),
    };
    if let Some(changes) = changes {
        return changes.iter().filter_map(codex_change).collect();
    }
    let Some(path) = path_of(input) else {
        return Vec::new();
    };
    let chunks = ["edits", "ReplacementChunks"]
        .iter()
        .find_map(|k| input.get(*k).and_then(Value::as_array));
    if let Some(chunks) = chunks {
        let pairs: Vec<(&str, &str)> = chunks.iter().filter_map(pair_of).collect();
        return if pairs.is_empty() {
            Vec::new()
        } else {
            vec![fragments(path, &pairs)]
        };
    }
    if let Some(pair) = pair_of(input) {
        return vec![fragments(path, &[pair])];
    }
    // A whole-file write: what is written is shown as added (what it replaces is not known from
    // the request alone; the per-file view compares against the checkpoint).
    if let Some(text) = str_of(input, &["content", "CodeContent", "new_source"]) {
        return vec![unified(path, None, text, Numbering::Real)];
    }
    Vec::new()
}

/// Several replacement pairs of one file as a single diff, one hunk each.
fn fragments(path: &str, pairs: &[(&str, &str)]) -> FileEdit {
    let p = shown(path);
    let mut merged = FileEdit {
        path: path.to_owned(),
        diff: format!("--- a/{p}\n+++ b/{p}\n"),
        added: 0,
        removed: 0,
    };
    for (old, new) in pairs {
        let part = unified(path, Some(old), new, Numbering::Fragment);
        // Drop the part's two header lines; keep its hunk.
        let body: String = part.diff.lines().skip(2).fold(String::new(), |mut acc, l| {
            acc.push_str(l);
            acc.push('\n');
            acc
        });
        merged.diff.push_str(&body);
        merged.added += part.added;
        merged.removed += part.removed;
    }
    merged
}

/// One entry of Codex's `changes`: `{path, kind: {type}, diff}`. A `diff` with hunks is used as
/// given; for an added or deleted file whose `diff` is the bare content, the whole text is shown
/// as added or removed.
fn codex_change(change: &Value) -> Option<FileEdit> {
    let path = path_of(change)?;
    let p = shown(path);
    let diff = change.get("diff").and_then(Value::as_str).unwrap_or("");
    let kind = change
        .get("kind")
        .and_then(|k| k.get("type").and_then(Value::as_str).or_else(|| k.as_str()))
        .unwrap_or("update");
    if diff.lines().any(|l| l.starts_with("@@")) {
        let mut text = String::new();
        if !diff.starts_with("--- ") {
            text.push_str(&format!("--- a/{p}\n+++ b/{p}\n"));
        }
        text.push_str(diff);
        if !text.ends_with('\n') {
            text.push('\n');
        }
        let (mut added, mut removed) = (0, 0);
        for l in diff.lines() {
            if l.starts_with('+') && !l.starts_with("+++") {
                added += 1;
            } else if l.starts_with('-') && !l.starts_with("---") {
                removed += 1;
            }
        }
        return Some(FileEdit {
            path: path.to_owned(),
            diff: text,
            added,
            removed,
        });
    }
    match kind {
        "add" => Some(unified(
            path,
            None,
            strip_plus(diff).as_str(),
            Numbering::Real,
        )),
        "delete" => {
            let mut edit = unified(path, Some(strip_minus(diff).as_str()), "", Numbering::Real);
            edit.path = path.to_owned();
            Some(edit)
        }
        _ if diff.is_empty() => None,
        // Unknown shape: shown as it came.
        _ => Some(FileEdit {
            path: path.to_owned(),
            diff: format!("--- a/{p}\n+++ b/{p}\n{diff}\n"),
            added: 0,
            removed: 0,
        }),
    }
}

/// `diff` with a leading `+` removed from each line, when every non-empty line has one (an added
/// file's diff body); otherwise unchanged.
fn strip_plus(diff: &str) -> String {
    strip_marker(diff, '+')
}

fn strip_minus(diff: &str) -> String {
    strip_marker(diff, '-')
}

fn strip_marker(diff: &str, marker: char) -> String {
    let all = diff
        .lines()
        .filter(|l| !l.is_empty())
        .all(|l| l.starts_with(marker));
    if !all {
        return diff.to_owned();
    }
    diff.lines()
        .map(|l| l.strip_prefix(marker).unwrap_or(l))
        .fold(String::new(), |mut acc, l| {
            acc.push_str(l);
            acc.push('\n');
            acc
        })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn a_changed_line_shows_with_context_and_real_numbers() {
        let old = "a\nb\nc\nd\ne\nf\ng\nh\n";
        let new = "a\nb\nc\nD\ne\nf\ng\nh\n";
        let d = file_diff("x.txt", Some(old), new);
        assert_eq!(
            d.diff,
            "--- a/x.txt\n+++ b/x.txt\n@@ -1,7 +1,7 @@\n a\n b\n c\n-d\n+D\n e\n f\n g\n"
        );
        assert_eq!((d.added, d.removed), (1, 1));
    }

    #[test]
    fn distant_changes_make_separate_hunks() {
        let old: String = (1..=30).map(|i| format!("{i}\n")).collect();
        let new = old
            .replace("\n2\n", "\ntwo\n")
            .replace("\n29\n", "\ntwenty-nine\n");
        let d = file_diff("n", Some(&old), &new);
        assert_eq!(d.diff.matches("@@ ").count(), 2, "{}", d.diff);
        assert!(d.diff.contains("@@ -1,5 +1,5 @@"), "{}", d.diff);
        assert!(d.diff.contains("@@ -26,5 +26,5 @@"), "{}", d.diff);
        assert_eq!((d.added, d.removed), (2, 2));
    }

    #[test]
    fn a_new_file_is_all_added_and_equal_text_has_no_hunks() {
        let d = file_diff("new.rs", None, "fn main() {}\n");
        assert_eq!(
            d.diff,
            "--- /dev/null\n+++ b/new.rs\n@@ -0,0 +1,1 @@\n+fn main() {}\n"
        );
        let same = file_diff("s", Some("x\n"), "x\n");
        assert_eq!(same.diff, "--- a/s\n+++ b/s\n");
        assert_eq!((same.added, same.removed), (0, 0));
    }

    #[test]
    fn a_missing_final_newline_is_a_change_and_is_marked() {
        let d = file_diff("f", Some("a\nb"), "a\nb\n");
        assert_eq!(
            d.diff,
            "--- a/f\n+++ b/f\n@@ -1,2 +1,2 @@\n a\n-b\n\\ No newline at end of file\n+b\n"
        );
    }

    #[test]
    fn the_differ_finds_the_minimal_edit_in_a_shuffled_middle() {
        let old = "keep\nx\ny\nz\nkeep2\n";
        let new = "keep\nz\nx\nkeep2\n";
        let d = file_diff("p", Some(old), new);
        // y is removed; x and z swap order costs one more pair.
        assert!(d.removed <= 2 && d.added <= 1, "{}", d.diff);
        assert!(d.diff.contains(" keep\n") && d.diff.contains(" keep2\n"));
    }

    #[test]
    fn a_change_past_the_edit_cap_is_one_replaced_block() {
        let old: String = (0..1200).map(|i| format!("old {i}\n")).collect();
        let new: String = (0..1200).map(|i| format!("new {i}\n")).collect();
        let d = file_diff("big", Some(&old), &new);
        assert_eq!((d.added, d.removed), (1200, 1200));
    }

    #[test]
    fn claude_edit_is_one_fragment_hunk() {
        let edits = preview_from_input(&json!({
            "file_path": "/w/src/a.rs",
            "old_string": "let x = 1;\nlet y = 2;",
            "new_string": "let x = 1;\nlet y = 3;",
        }));
        assert_eq!(edits.len(), 1);
        assert_eq!(edits[0].path, "/w/src/a.rs");
        assert_eq!(
            edits[0].diff,
            "--- a/w/src/a.rs\n+++ b/w/src/a.rs\n@@ edit @@\n let x = 1;\n-let y = 2;\n+let y = 3;\n"
        );
        assert_eq!((edits[0].added, edits[0].removed), (1, 1));
    }

    #[test]
    fn claude_multiedit_has_a_hunk_per_pair() {
        let edits = preview_from_input(&json!({
            "file_path": "a.py",
            "edits": [
                {"old_string": "one\n", "new_string": "uno\n"},
                {"old_string": "two\n", "new_string": "dos\n"},
            ],
        }));
        assert_eq!(edits.len(), 1);
        assert_eq!(edits[0].diff.matches("@@ edit @@").count(), 2);
        assert_eq!((edits[0].added, edits[0].removed), (2, 2));
    }

    #[test]
    fn claude_write_and_agy_write_show_the_text_as_added() {
        for input in [
            json!({"file_path": "n.txt", "content": "hello\nworld\n"}),
            json!({"TargetFile": "n.txt", "CodeContent": "hello\nworld\n"}),
        ] {
            let edits = preview_from_input(&input);
            assert_eq!(edits.len(), 1, "{input}");
            assert_eq!(edits[0].added, 2);
            assert!(edits[0].diff.starts_with("--- /dev/null\n+++ b/n.txt\n"));
        }
    }

    #[test]
    fn agy_replace_and_multi_replace_read_their_chunks() {
        let one = preview_from_input(&json!({
            "TargetFile": "/w/calc.py",
            "TargetContent": "return a - b\n",
            "ReplacementContent": "return a + b\n",
        }));
        assert_eq!((one[0].added, one[0].removed), (1, 1));
        let many = preview_from_input(&json!({
            "TargetFile": "/w/calc.py",
            "ReplacementChunks": [
                {"TargetContent": "a\n", "ReplacementContent": "b\n"},
                {"TargetContent": "c\n", "ReplacementContent": "d\n"},
            ],
        }));
        assert_eq!(many[0].diff.matches("@@ edit @@").count(), 2);
    }

    #[test]
    fn codex_changes_use_their_diff_or_the_whole_added_text() {
        let edits = preview_from_input(&json!([
            {"path": "a.rs", "kind": {"type": "update"}, "diff": "@@ -1 +1 @@\n-old\n+new\n"},
            {"path": "b.rs", "kind": {"type": "add"}, "diff": "+x\n+y\n"},
            {"path": "c.rs", "kind": {"type": "delete"}, "diff": "-gone\n"},
        ]));
        assert_eq!(edits.len(), 3);
        assert!(edits[0]
            .diff
            .starts_with("--- a/a.rs\n+++ b/a.rs\n@@ -1 +1 @@\n"));
        assert_eq!((edits[0].added, edits[0].removed), (1, 1));
        assert_eq!(
            edits[1].diff,
            "--- /dev/null\n+++ b/b.rs\n@@ -0,0 +1,2 @@\n+x\n+y\n"
        );
        assert_eq!((edits[2].added, edits[2].removed), (0, 1));
        // The same under a `changes` key (an approval's params).
        let wrapped = preview_from_input(&json!({"changes": [
            {"path": "a.rs", "kind": {"type": "update"}, "diff": "@@ -1 +1 @@\n-o\n+n\n"}
        ]}));
        assert_eq!(wrapped.len(), 1);
    }

    #[test]
    fn inputs_that_are_not_edits_give_nothing() {
        assert!(preview_from_input(&json!({"command": "ls"})).is_empty());
        assert!(preview_from_input(&json!({"file_path": "a"})).is_empty());
        assert!(preview_from_input(&Value::Null).is_empty());
        assert!(preview_from_input(&json!([{"nope": 1}])).is_empty());
    }
}
