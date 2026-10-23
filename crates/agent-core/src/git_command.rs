//! What a `git` or `gh` command line does, in words, for an approval card (pure: no I/O).
//!
//! [`describe`] reads an argv (already split by the app's fail-closed tokenizer, wrapper
//! unwrapped) and names the operation: what kind it is, whether it stays in this repository or
//! reaches a remote, the details worth reading (message, branch, remote, files) and any warning
//! for an operation that loses work or rewrites history.
//!
//! It is a reading aid, never a gate: the card shows the command line as sent beneath it, and
//! nothing here allows or refuses anything. Its ceilings are honest ones. An option it does not
//! know marks the reading [`GitAction::uncertain`] (an unknown option may take the next word, so
//! the positionals after it may be misread), and anything that is not plain text (a control,
//! bidi or zero-width character) gives no reading at all.

use crate::shell_words::basename;

/// The program the command runs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Tool {
    Git,
    /// The GitHub CLI.
    Gh,
}

impl Tool {
    pub fn name(self) -> &'static str {
        match self {
            Self::Git => "git",
            Self::Gh => "gh",
        }
    }
}

/// The family of operation, for the card's icon and badge.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    /// Reads only: status, log, diff, show, `gh pr view`.
    Inspect,
    Stage,
    Commit,
    Branch,
    Switch,
    Merge,
    Rebase,
    Reset,
    Restore,
    Remove,
    Clean,
    Stash,
    Tag,
    CherryPick,
    Revert,
    Fetch,
    Pull,
    Push,
    Clone,
    Remote,
    Config,
    PullRequest,
    Issue,
    Release,
    Repo,
    Workflow,
    /// `gh api`: a raw GitHub API call.
    Api,
    Other,
}

/// Where the operation's effects land.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Reach {
    /// Changes nothing.
    ReadOnly,
    /// Changes this repository (working tree, index, refs) and nothing beyond it.
    Local,
    /// Talks to a remote and may change it (a push, a pull request, a release), or brings its
    /// changes in.
    Remote,
}

impl Reach {
    /// The badge on the card.
    pub fn label(self) -> &'static str {
        match self {
            Self::ReadOnly => "Read-only",
            Self::Local => "Changes this repository",
            Self::Remote => "Reaches the remote",
        }
    }
}

/// One labelled fact about the operation ("Message", "Branch", …).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Detail {
    pub label: &'static str,
    pub value: String,
}

/// A reading of one `git` or `gh` command.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GitAction {
    pub tool: Tool,
    pub kind: Kind,
    /// What it does, as a short sentence ("Push feat/x to origin").
    pub title: String,
    pub reach: Reach,
    pub details: Vec<Detail>,
    /// What it can lose or rewrite, in words. Non-empty: Allow is not the suggested button.
    pub warnings: Vec<String>,
    /// Options it did not recognise, so the details may be incomplete or misread.
    pub uncertain: bool,
}

impl GitAction {
    fn new(tool: Tool, kind: Kind, reach: Reach, title: impl Into<String>) -> Self {
        Self {
            tool,
            kind,
            title: title.into(),
            reach,
            details: Vec::new(),
            warnings: Vec::new(),
            uncertain: false,
        }
    }

    fn detail(&mut self, label: &'static str, value: impl Into<String>) {
        let value = value.into();
        if !value.is_empty() {
            self.details.push(Detail { label, value });
        }
    }

    fn list(&mut self, label: &'static str, values: &[String]) {
        if !values.is_empty() {
            self.detail(label, values.join(", "));
        }
    }

    fn warn(&mut self, warning: impl Into<String>) {
        let warning = warning.into();
        if !self.warnings.contains(&warning) {
            self.warnings.push(warning);
        }
    }

    /// Whether it can lose work or rewrite history.
    pub fn destructive(&self) -> bool {
        !self.warnings.is_empty()
    }
}

/// Whether `s` is safe to show as text: no control (a line break aside: commit messages have
/// them), bidi or zero-width characters.
fn plain(s: &str) -> bool {
    !crate::text::has_control_or_bidi(s, &['\n'])
}

/// Reads `argv` as a `git` or `gh` command. `None`: it is neither, names no subcommand, or holds
/// text that is not plain.
pub fn describe(argv: &[String]) -> Option<GitAction> {
    if argv.iter().any(|a| !plain(a)) {
        return None;
    }
    let (program, rest) = argv.split_first()?;
    match basename(program) {
        "git" => git(rest),
        "gh" => gh(rest),
        _ => None,
    }
}

// ---- the command line ----

/// Reads a whole command line as a `git` or `gh` command: [`split`] it, unwrap one plain shell
/// wrapper (`bash -lc '…'`, as Codex sends every command), then [`describe`].
pub fn describe_command(command: &str) -> Option<GitAction> {
    let words = split(command)?;
    match words.as_slice() {
        [shell, flag, body]
            if ["sh", "bash", "zsh"].contains(&basename(shell))
                && (flag == "-c" || flag == "-lc") =>
        {
            describe(&split(body)?)
        }
        _ => describe(&words),
    }
}

/// Characters that end a reading when they appear outside quotes: separators and pipes,
/// substitution, escapes, redirection, subshells, braces, `~`, globs and line breaks.
const UNQUOTED_SYNTAX: &[char] = &[
    ';', '&', '|', '`', '$', '\\', '<', '>', '(', ')', '{', '}', '~', '*', '?', '[', '\n', '\r',
];

/// Splits a command line into words exactly as a POSIX shell would, or `None` for anything
/// whose words a shell could make differently: any shell syntax outside quotes (separators,
/// substitution, redirection, globs; see [`UNQUOTED_SYNTAX`]), a `$` or backtick inside double
/// quotes, a comment, a leading `NAME=value`, unbalanced quotes, or a control character (a line
/// break is allowed inside quotes only).
///
/// One substitution is read, because agents write every multi-line commit message with it:
/// `"$(cat <<'EOF'` … `EOF` `)"`, a heredoc with a quoted delimiter, whose body the shell
/// passes on literally.
///
/// For display only: unlike the approval rules' parser it accepts quoted parentheses and
/// semicolons (`-m "feat(ui): x"`), which change nothing about what runs.
pub fn split(command: &str) -> Option<Vec<String>> {
    if command
        .chars()
        .any(|c| (c.is_control() && c != '\n') || (c.is_whitespace() && !matches!(c, ' ' | '\n')))
    {
        return None;
    }
    let chars: Vec<char> = command.chars().collect();
    let mut words = Vec::new();
    let mut word = String::new();
    let mut in_word = false;
    let mut i = 0;
    while i < chars.len() {
        let c = chars[i];
        i += 1;
        match c {
            ' ' => {
                if in_word {
                    words.push(std::mem::take(&mut word));
                    in_word = false;
                }
            }
            '\'' => {
                let end = chars[i..].iter().position(|&q| q == '\'')? + i;
                word.extend(&chars[i..end]);
                i = end + 1;
                in_word = true;
            }
            '"' => {
                in_word = true;
                loop {
                    let c = *chars.get(i)?;
                    i += 1;
                    match c {
                        '"' => break,
                        '\\' => match chars.get(i) {
                            Some(&e @ ('"' | '\\' | '$' | '`')) => {
                                word.push(e);
                                i += 1;
                            }
                            Some('\n') => i += 1,
                            _ => word.push('\\'),
                        },
                        '$' => {
                            let (body, next) = quoted_heredoc(&chars, i - 1)?;
                            word.push_str(&body);
                            i = next;
                        }
                        '`' => return None,
                        c => word.push(c),
                    }
                }
            }
            '#' if !in_word => return None,
            // A shell expands `~` only at a word's start: `HEAD~3` is literal.
            '~' if in_word => word.push('~'),
            c if UNQUOTED_SYNTAX.contains(&c) => return None,
            c => {
                word.push(c);
                in_word = true;
            }
        }
    }
    if in_word {
        words.push(word);
    }
    let first = words.first()?;
    let assignment = first.split_once('=').is_some_and(|(name, _)| {
        !name.is_empty()
            && !name.starts_with(|c: char| c.is_ascii_digit())
            && name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
    });
    (!assignment).then_some(words)
}

/// At `chars[at]`, inside double quotes: `$(cat <<'DELIM'⏎body⏎DELIM⏎)`, the body (which the
/// shell passes on as written) and the index after the `)`. Only a quoted delimiter qualifies:
/// an unquoted one would expand `$` in the body.
fn quoted_heredoc(chars: &[char], at: usize) -> Option<(String, usize)> {
    let rest: String = chars[at..].iter().collect();
    let after = rest
        .strip_prefix("$(cat <<")?
        .trim_start_matches(' ')
        .strip_prefix('\'')?;
    let (delim, after) = after.split_once('\'')?;
    if delim.is_empty() || !delim.chars().all(|c| c.is_ascii_alphanumeric() || c == '_') {
        return None;
    }
    let body_start = after.strip_prefix('\n')?;
    let close = format!("\n{delim}\n)");
    let (body, tail) = if let Some(tail) = body_start.strip_prefix(&close[1..]) {
        (String::new(), tail)
    } else {
        let end = body_start.find(&close)?;
        (
            body_start[..end].to_owned(),
            &body_start[end + close.len()..],
        )
    };
    let consumed = rest.chars().count() - tail.chars().count();
    // `$(…)` drops trailing newlines from what it captured; the heredoc's own is the last.
    Some((body.trim_end_matches('\n').to_owned(), at + consumed))
}

// ---- options ----

/// The options a subcommand knows: those that take a value, and plain flags. Short options are
/// written `-m`, long ones `--message`. A flag also accepts `--flag=value` (git's optional
/// values: `--force-with-lease=main`); `optional` shorts take the rest of their cluster
/// (`-Skey`, `-uno`) but never the next word.
#[derive(Clone, Copy)]
struct Spec<'a> {
    values: &'a [&'a str],
    flags: &'a [&'a str],
    optional: &'a [&'a str],
}

const NO_OPTIONS: Spec<'static> = Spec {
    values: &[],
    flags: &[],
    optional: &[],
};

/// An argv split into the options it set (in order, with their values), its positionals, and
/// what followed `--`.
#[derive(Default)]
struct Args {
    opts: Vec<(String, Option<String>)>,
    positionals: Vec<String>,
    paths: Vec<String>,
    /// Options the spec does not know.
    unknown: Vec<String>,
}

impl Args {
    fn has(&self, names: &[&str]) -> bool {
        self.opts.iter().any(|(n, _)| names.contains(&n.as_str()))
    }

    /// Every value given to any of `names`.
    fn values(&self, names: &[&str]) -> Vec<String> {
        self.opts
            .iter()
            .filter(|(n, _)| names.contains(&n.as_str()))
            .filter_map(|(_, v)| v.clone())
            .collect()
    }

    fn value(&self, names: &[&str]) -> Option<String> {
        self.values(names).pop()
    }

    /// The positionals and the paths after `--`, together.
    fn targets(&self) -> Vec<String> {
        self.positionals
            .iter()
            .chain(&self.paths)
            .cloned()
            .collect()
    }
}

fn parse(args: &[String], spec: Spec<'_>) -> Args {
    let mut out = Args::default();
    let mut i = 0;
    while i < args.len() {
        let arg = &args[i];
        i += 1;
        if arg == "--" {
            out.paths.extend(args[i..].iter().cloned());
            break;
        }
        if let Some(long) = arg.strip_prefix("--").filter(|l| !l.is_empty()) {
            let (name, attached) = match long.split_once('=') {
                Some((n, v)) => (format!("--{n}"), Some(v.to_owned())),
                None => (format!("--{long}"), None),
            };
            if spec.values.contains(&name.as_str()) {
                let value = attached.or_else(|| {
                    let next = args.get(i).cloned();
                    i += usize::from(next.is_some());
                    next
                });
                out.opts.push((name, value));
            } else if spec.flags.contains(&name.as_str()) {
                out.opts.push((name, attached));
            } else {
                out.unknown.push(arg.clone());
                out.opts.push((name, attached));
            }
            continue;
        }
        if arg.len() > 1 && arg.starts_with('-') && is_number(&arg[1..]) {
            out.opts.push((arg.clone(), None));
            continue;
        }
        if arg.len() > 1 && arg.starts_with('-') {
            // A cluster of shorts: `-am msg`, `-fd`, `-mmsg`, `-Skey`.
            let chars: Vec<char> = arg[1..].chars().collect();
            let mut j = 0;
            while j < chars.len() {
                let name = format!("-{}", chars[j]);
                let rest: String = chars[j + 1..].iter().collect();
                j += 1;
                if spec.values.contains(&name.as_str()) {
                    let value = if rest.is_empty() {
                        let next = args.get(i).cloned();
                        i += usize::from(next.is_some());
                        next
                    } else {
                        Some(rest)
                    };
                    out.opts.push((name, value));
                    break;
                }
                if spec.optional.contains(&name.as_str()) {
                    out.opts.push((name, (!rest.is_empty()).then_some(rest)));
                    break;
                }
                if !spec.flags.contains(&name.as_str()) {
                    out.unknown.push(name.clone());
                }
                out.opts.push((name, None));
            }
            continue;
        }
        out.positionals.push(arg.clone());
    }
    out
}

/// `-1`, `-20`: a count (`git log -3`), not an option cluster.
fn is_number(s: &str) -> bool {
    !s.is_empty() && s.bytes().all(|b| b.is_ascii_digit())
}

/// Parses `args` for `action`, recording unknown options as uncertainty.
fn parse_for(action: &mut GitAction, args: &[String], spec: Spec<'_>) -> Args {
    let parsed = parse(args, spec);
    if !parsed.unknown.is_empty() {
        action.uncertain = true;
        action.detail("Other options", parsed.unknown.join(" "));
    }
    parsed
}

/// A short, one-line form of a message for the title.
fn quoted(s: &str) -> String {
    format!("“{s}”")
}

// ---- git ----

/// Git's global options that take a value.
const GIT_GLOBAL_VALUES: &[&str] = &[
    "-C",
    "-c",
    "--git-dir",
    "--work-tree",
    "--namespace",
    "--exec-path",
    "--config-env",
    "--super-prefix",
];

fn git(args: &[String]) -> Option<GitAction> {
    // Global options come before the subcommand.
    let mut i = 0;
    let mut globals: Vec<(String, Option<String>)> = Vec::new();
    while let Some(arg) = args.get(i) {
        if !arg.starts_with('-') {
            break;
        }
        i += 1;
        let (name, attached) = match arg.split_once('=') {
            Some((n, v)) if n.starts_with("--") => (n.to_owned(), Some(v.to_owned())),
            _ => (arg.clone(), None),
        };
        let value = if GIT_GLOBAL_VALUES.contains(&name.as_str()) && attached.is_none() {
            let next = args.get(i).cloned();
            i += usize::from(next.is_some());
            next
        } else {
            attached
        };
        globals.push((name, value));
    }
    let sub = args.get(i)?;
    let rest = &args[i + 1..];
    let mut action = git_sub(sub, rest);
    for (name, value) in &globals {
        match (name.as_str(), value) {
            ("-C", Some(dir)) => action.detail("In folder", dir.clone()),
            ("-c" | "--config-env", Some(setting)) => {
                action.detail("Git setting", setting.clone());
                action.warn(
                    "Overrides a git setting for this command, which can change what git runs \
                     (an alias, a hook path, an SSH command).",
                );
            }
            ("--git-dir" | "--work-tree", Some(dir)) => {
                action.detail("Repository", dir.clone());
            }
            ("--exec-path", _) => {
                action.warn("Points git at other programs to run for its subcommands.");
            }
            ("--no-pager" | "-P" | "--no-optional-locks" | "--literal-pathspecs", _) => {}
            _ => {
                action.uncertain = true;
                action.detail("Other options", name.clone());
            }
        }
    }
    Some(action)
}

fn git_sub(sub: &str, rest: &[String]) -> GitAction {
    use Kind::*;
    use Reach::{Local, ReadOnly};
    let new = |kind: Kind, reach: Reach, title: &str| GitAction::new(Tool::Git, kind, reach, title);
    match sub {
        "commit" => git_commit(rest),
        "push" => git_push(rest),
        "pull" => git_pull(rest),
        "fetch" => git_fetch(rest),
        "branch" => git_branch(rest),
        "checkout" => git_checkout(rest),
        "switch" => git_switch(rest),
        "merge" => git_merge(rest),
        "rebase" => git_rebase(rest),
        "reset" => git_reset(rest),
        "restore" => git_restore(rest),
        "add" | "stage" => git_add(rest),
        "rm" => git_rm(rest),
        "mv" => {
            let mut a = new(Stage, Local, "Move or rename files");
            let args = parse_for(&mut a, rest, flags(&["-f", "--force", "-k", "-n", "-v"]));
            a.list("Files", &args.targets());
            a
        }
        "clean" => git_clean(rest),
        "stash" => git_stash(rest),
        "tag" => git_tag(rest),
        "cherry-pick" => git_pick(rest, CherryPick, "Apply", "Commits"),
        "revert" => git_pick(rest, Revert, "Revert", "Commits"),
        "clone" => {
            let mut a = new(Clone, Reach::Remote, "Clone a repository");
            let args = parse_for(
                &mut a,
                rest,
                Spec {
                    values: &["-b", "--branch", "--depth", "-o", "--origin", "--reference"],
                    flags: &[
                        "--bare",
                        "--mirror",
                        "-q",
                        "--quiet",
                        "--recursive",
                        "--recurse-submodules",
                        "--single-branch",
                        "--no-checkout",
                        "-n",
                    ],
                    optional: &[],
                },
            );
            let mut targets = args.targets().into_iter();
            if let Some(url) = targets.next() {
                a.detail("From", url);
            }
            if let Some(dir) = targets.next() {
                a.detail("Into", dir);
            }
            a.detail(
                "Branch",
                args.value(&["-b", "--branch"]).unwrap_or_default(),
            );
            a
        }
        "remote" => git_remote(rest),
        "config" => {
            let mut a = new(Config, Local, "Read or change git settings");
            let args = parse(rest, NO_OPTIONS);
            let writes = args.positionals.len() >= 2
                || args.has(&["--unset", "--unset-all", "--add", "--replace-all"]);
            if writes {
                a.title = "Change a git setting".to_owned();
            } else {
                a.reach = ReadOnly;
                a.title = "Read git settings".to_owned();
            }
            if args.has(&["--global", "--system"]) {
                a.detail("Scope", "every repository (global)");
                if writes {
                    a.warn("Changes git's settings for every repository, not only this one.");
                }
            }
            a.list("Setting", &args.positionals);
            a
        }
        "status" | "log" | "diff" | "show" | "blame" | "reflog" | "shortlog" | "describe"
        | "ls-files" | "ls-tree" | "rev-parse" | "rev-list" | "cat-file" | "grep" | "show-ref"
        | "for-each-ref" | "merge-base" | "name-rev" | "whatchanged" | "help" | "version" => {
            let words = match sub {
                "status" => "Show the working tree status",
                "log" => "Show the commit history",
                "diff" => "Show changes",
                "show" => "Show a commit",
                "blame" => "Show who changed each line",
                _ => "Read repository information",
            };
            let mut a = new(Inspect, ReadOnly, words);
            let args = parse(rest, NO_OPTIONS);
            a.list("Of", &args.targets());
            // `--output` writes a file; `--ext-diff`/`--textconv` run configured programs.
            if args
                .opts
                .iter()
                .any(|(n, _)| ["--output", "--ext-diff", "--textconv"].contains(&n.as_str()))
            {
                a.reach = Local;
                a.warn("An option here writes a file or runs a configured program.");
            }
            a
        }
        "worktree" => {
            let action = rest.first().map(String::as_str).unwrap_or("list");
            let (reach, words) = match action {
                "list" => (ReadOnly, "List worktrees"),
                "add" => (Local, "Add a worktree"),
                "remove" => (Local, "Remove a worktree"),
                "prune" => (Local, "Prune stale worktrees"),
                _ => (Local, "Change worktrees"),
            };
            let mut a = new(Branch, reach, words);
            let args = parse(rest.get(1..).unwrap_or_default(), NO_OPTIONS);
            a.list("Worktree", &args.positionals);
            if action == "remove" && args.has(&["-f", "--force"]) {
                a.warn("Removes the worktree even with uncommitted changes in it.");
            }
            a
        }
        "init" => {
            let mut a = new(Repo, Local, "Create a repository");
            a.list("In", &parse(rest, NO_OPTIONS).positionals);
            a
        }
        "gc" | "prune" | "repack" | "maintenance" | "fsck" => {
            let mut a = new(Other, Local, "Maintain the repository");
            a.detail("Operation", sub);
            a
        }
        "submodule" => {
            let mut a = new(Other, Reach::Remote, "Work with submodules");
            a.list("Arguments", rest);
            a
        }
        _ => {
            let mut a = new(Other, Local, &format!("Run git {sub}"));
            a.list("Arguments", rest);
            a.uncertain = true;
            a
        }
    }
}

const fn flags(names: &'static [&'static str]) -> Spec<'static> {
    Spec {
        values: &[],
        flags: names,
        optional: &[],
    }
}

fn git_commit(rest: &[String]) -> GitAction {
    let mut a = GitAction::new(Tool::Git, Kind::Commit, Reach::Local, "Commit");
    let args = parse_for(
        &mut a,
        rest,
        Spec {
            values: &[
                "-m",
                "--message",
                "-F",
                "--file",
                "--author",
                "--date",
                "-C",
                "-c",
                "--reuse-message",
                "--reedit-message",
                "--fixup",
                "--squash",
                "--cleanup",
                "-t",
                "--template",
                "--trailer",
                "--pathspec-from-file",
            ],
            flags: &[
                "-a",
                "--all",
                "-p",
                "--patch",
                "--amend",
                "-n",
                "--no-verify",
                "--verify",
                "--allow-empty",
                "--allow-empty-message",
                "-s",
                "--signoff",
                "--no-signoff",
                "--gpg-sign",
                "--no-gpg-sign",
                "-q",
                "--quiet",
                "-v",
                "--verbose",
                "-e",
                "--edit",
                "--no-edit",
                "--dry-run",
                "--short",
                "--porcelain",
                "-o",
                "--only",
                "-i",
                "--include",
                "--untracked-files",
                "--reset-author",
                "-z",
                "--null",
                "--status",
                "--no-status",
            ],
            optional: &["-S", "-u"],
        },
    );
    let messages = args.values(&["-m", "--message"]);
    let amend = args.has(&["--amend"]);
    let all = args.has(&["-a", "--all"]);
    let files = args.targets();
    let what = if !files.is_empty() {
        "the named files"
    } else if all {
        "every tracked change"
    } else {
        "the staged changes"
    };
    a.title = if amend {
        format!("Amend the last commit with {what}")
    } else {
        format!("Commit {what}")
    };
    if let Some(subject) = messages.first().and_then(|m| m.lines().next()) {
        a.title = format!("{}: {}", a.title, quoted(subject));
    }
    a.detail("Message", messages.join("\n\n"));
    if let Some(file) = args.value(&["-F", "--file"]) {
        a.detail("Message from", file);
    }
    if let Some(reuse) = args.value(&["-C", "-c", "--reuse-message", "--reedit-message"]) {
        a.detail("Message of", reuse);
    }
    if let Some(fixup) = args.value(&["--fixup", "--squash"]) {
        a.detail("Fixes up", fixup);
    }
    a.list("Files", &files);
    a.detail("Author", args.value(&["--author"]).unwrap_or_default());
    if amend {
        a.warn(
            "Rewrites the last commit. If it was already pushed, the branch will need a force \
             push.",
        );
    }
    if args.has(&["-n", "--no-verify"]) {
        a.warn("Skips the repository's commit hooks (checks, linters, signing).");
    }
    if args.has(&["--dry-run"]) {
        a.reach = Reach::ReadOnly;
        a.title = format!("Preview: {}", a.title);
    }
    a
}

/// One refspec of a push: what it does to the remote.
fn refspec_words(spec: &str) -> (String, bool, bool) {
    let (forced, spec) = match spec.strip_prefix('+') {
        Some(rest) => (true, rest),
        None => (false, spec),
    };
    match spec.split_once(':') {
        Some(("", dst)) => (format!("delete {dst}"), forced, true),
        Some((src, dst)) if src == dst => (src.to_owned(), forced, false),
        Some((src, dst)) => (format!("{src} → {dst}"), forced, false),
        None => (spec.to_owned(), forced, false),
    }
}

fn git_push(rest: &[String]) -> GitAction {
    let mut a = GitAction::new(Tool::Git, Kind::Push, Reach::Remote, "Push");
    let args = parse_for(
        &mut a,
        rest,
        Spec {
            values: &[
                "-o",
                "--push-option",
                "--repo",
                "--receive-pack",
                "--exec",
                "--signed",
            ],
            flags: &[
                "-f",
                "--force",
                "--force-with-lease",
                "--no-force-with-lease",
                "--force-if-includes",
                "-u",
                "--set-upstream",
                "--tags",
                "--follow-tags",
                "--no-follow-tags",
                "--all",
                "--branches",
                "--mirror",
                "-d",
                "--delete",
                "-n",
                "--dry-run",
                "--no-verify",
                "--verify",
                "-q",
                "--quiet",
                "-v",
                "--verbose",
                "--atomic",
                "--no-atomic",
                "--porcelain",
                "--prune",
                "--progress",
                "--no-progress",
                "--thin",
                "--no-thin",
                "--ipv4",
                "--ipv6",
                "-4",
                "-6",
            ],
            optional: &[],
        },
    );
    let mut positionals = args.targets().into_iter();
    let remote = positionals
        .next()
        .or_else(|| args.value(&["--repo"]))
        .unwrap_or_default();
    let refs: Vec<String> = positionals.collect();
    let delete = args.has(&["-d", "--delete"]);
    let mut shown = Vec::new();
    let mut forced_ref = false;
    let mut deletes_ref = false;
    for spec in &refs {
        let (words, forced, deletes) = refspec_words(spec);
        forced_ref |= forced;
        deletes_ref |= deletes;
        shown.push(if delete {
            format!("delete {words}")
        } else {
            words
        });
    }
    let what = if args.has(&["--mirror"]) {
        "every ref (mirror)".to_owned()
    } else if args.has(&["--all", "--branches"]) {
        "every branch".to_owned()
    } else if shown.is_empty() {
        "the current branch".to_owned()
    } else {
        shown.join(", ")
    };
    let to = if remote.is_empty() {
        "its upstream".to_owned()
    } else {
        remote.clone()
    };
    a.title = if delete && !refs.is_empty() {
        format!("Delete {} on {to}", refs.join(", "))
    } else {
        format!("Push {what} to {to}")
    };
    a.detail("Remote", remote);
    a.list("Branches", &shown);
    if args.has(&["--tags", "--follow-tags"]) {
        a.detail("Tags", "pushed too");
    }
    if args.has(&["-u", "--set-upstream"]) {
        a.detail("Upstream", "set to this push");
    }
    if args.has(&["-f", "--force"]) || forced_ref {
        a.warn(
            "Force push: replaces the remote branch with this one, discarding commits on the \
             remote that are not here.",
        );
    } else if args.has(&["--force-with-lease"]) {
        a.warn(
            "Force push with lease: rewrites the remote branch, unless someone else pushed to it \
             since it was last fetched.",
        );
    }
    if delete || deletes_ref {
        a.warn("Deletes a branch or tag on the remote.");
    }
    if args.has(&["--mirror"]) {
        a.warn("Mirror push: makes the remote match this repository exactly, deleting refs.");
    }
    if args.has(&["--prune"]) {
        a.warn("Deletes remote branches that have no local counterpart.");
    }
    if args.has(&["--no-verify"]) {
        a.warn("Skips the repository's pre-push hook.");
    }
    if args.has(&["-n", "--dry-run"]) {
        a.reach = Reach::ReadOnly;
        a.title = format!("Preview: {}", a.title);
    }
    a
}

fn git_pull(rest: &[String]) -> GitAction {
    let mut a = GitAction::new(Tool::Git, Kind::Pull, Reach::Remote, "Pull");
    let args = parse_for(
        &mut a,
        rest,
        Spec {
            values: &["-s", "--strategy", "-X", "--strategy-option", "--depth"],
            flags: &[
                "--rebase",
                "-r",
                "--no-rebase",
                "--ff",
                "--no-ff",
                "--ff-only",
                "--squash",
                "--autostash",
                "--no-autostash",
                "--all",
                "-q",
                "--quiet",
                "-v",
                "--verbose",
                "--tags",
                "-t",
                "--prune",
                "-p",
                "--no-edit",
                "--edit",
                "-e",
                "--commit",
                "--no-commit",
                "--unshallow",
                "--recurse-submodules",
            ],
            optional: &[],
        },
    );
    let mut targets = args.targets().into_iter();
    let remote = targets.next().unwrap_or_default();
    let branches: Vec<String> = targets.collect();
    let from = match (remote.is_empty(), branches.is_empty()) {
        (true, _) => "the upstream".to_owned(),
        (false, true) => remote.clone(),
        (false, false) => format!("{remote}/{}", branches.join(", ")),
    };
    let how = if args.has(&["--rebase", "-r"]) {
        " and rebase onto it"
    } else if args.has(&["--ff-only"]) {
        " (fast-forward only)"
    } else {
        " and merge"
    };
    a.title = format!("Pull from {from}{how}");
    a.detail("Remote", remote);
    a.list("Branches", &branches);
    a
}

fn git_fetch(rest: &[String]) -> GitAction {
    let mut a = GitAction::new(Tool::Git, Kind::Fetch, Reach::Remote, "Fetch");
    let args = parse_for(
        &mut a,
        rest,
        Spec {
            values: &["--depth", "--deepen", "--shallow-since", "-j", "--jobs"],
            flags: &[
                "--all",
                "-p",
                "--prune",
                "-P",
                "--prune-tags",
                "-t",
                "--tags",
                "-n",
                "--no-tags",
                "-q",
                "--quiet",
                "-v",
                "--verbose",
                "-f",
                "--force",
                "--unshallow",
                "--dry-run",
                "--multiple",
                "--recurse-submodules",
            ],
            optional: &[],
        },
    );
    let targets = args.targets();
    a.title = match targets.first() {
        _ if args.has(&["--all"]) => "Fetch from every remote".to_owned(),
        Some(remote) => format!("Fetch from {remote}"),
        None => "Fetch from the upstream".to_owned(),
    };
    a.list("Refs", targets.get(1..).unwrap_or_default());
    if args.has(&["-p", "--prune", "-P", "--prune-tags"]) {
        a.detail(
            "Prune",
            "removes local copies of branches deleted on the remote",
        );
    }
    a
}

fn git_branch(rest: &[String]) -> GitAction {
    let mut a = GitAction::new(Tool::Git, Kind::Branch, Reach::Local, "Branch");
    let args = parse_for(
        &mut a,
        rest,
        Spec {
            values: &[
                "-u",
                "--set-upstream-to",
                "--contains",
                "--no-contains",
                "--merged",
                "--no-merged",
                "--points-at",
                "--sort",
                "--format",
                "--color",
            ],
            flags: &[
                "-d",
                "--delete",
                "-D",
                "-f",
                "--force",
                "-m",
                "--move",
                "-M",
                "-c",
                "--copy",
                "-C",
                "-l",
                "--list",
                "-a",
                "--all",
                "-r",
                "--remotes",
                "-v",
                "-vv",
                "--verbose",
                "-q",
                "--quiet",
                "--show-current",
                "--unset-upstream",
                "-t",
                "--track",
                "--no-track",
                "--edit-description",
                "--no-color",
                "--column",
                "--no-column",
                "-i",
                "--ignore-case",
            ],
            optional: &[],
        },
    );
    let names = args.positionals.clone();
    let force = args.has(&["-f", "--force"]);
    if args.has(&["-d", "--delete", "-D"]) {
        a.title = format!("Delete branch {}", names.join(", "));
        a.list("Branches", &names);
        if args.has(&["-D"]) || force {
            a.warn("Deletes the branch even if its commits are not merged anywhere.");
        }
        if args.has(&["-r", "--remotes"]) {
            a.detail("Which", "remote-tracking copies only");
        }
    } else if args.has(&["-m", "--move", "-M"]) {
        a.title = match names.as_slice() {
            [new] => format!("Rename the current branch to {new}"),
            [old, new, ..] => format!("Rename branch {old} to {new}"),
            [] => "Rename a branch".to_owned(),
        };
        if args.has(&["-M"]) || force {
            a.warn("Overwrites a branch that already has the new name.");
        }
    } else if args.has(&["-c", "--copy", "-C"]) {
        a.title = format!("Copy branch {}", names.join(" to "));
        if args.has(&["-C"]) || force {
            a.warn("Overwrites a branch that already has the new name.");
        }
    } else if let Some(upstream) = args.value(&["-u", "--set-upstream-to"]) {
        a.title = format!("Set the upstream to {upstream}");
        a.list("Branch", &names);
    } else if args.has(&["--unset-upstream"]) {
        a.title = "Remove the branch's upstream".to_owned();
    } else if names.is_empty()
        || args.has(&[
            "-l",
            "--list",
            "--show-current",
            "-a",
            "--all",
            "-r",
            "--remotes",
        ])
    {
        a.reach = Reach::ReadOnly;
        a.kind = Kind::Inspect;
        a.title = "List branches".to_owned();
    } else {
        a.title = match names.as_slice() {
            [name] => format!("Create branch {name}"),
            [name, start, ..] => format!("Create branch {name} from {start}"),
            [] => unreachable!("names is not empty here"),
        };
        if force {
            a.warn("Resets an existing branch of that name to the new start point.");
        }
    }
    a
}

fn git_checkout(rest: &[String]) -> GitAction {
    let mut a = GitAction::new(Tool::Git, Kind::Switch, Reach::Local, "Check out");
    let args = parse_for(
        &mut a,
        rest,
        Spec {
            values: &["-b", "-B", "--orphan", "--conflict", "--pathspec-from-file"],
            flags: &[
                "-f",
                "--force",
                "-t",
                "--track",
                "--no-track",
                "--detach",
                "-q",
                "--quiet",
                "-m",
                "--merge",
                "-p",
                "--patch",
                "--ours",
                "--theirs",
                "--progress",
                "--no-progress",
                "--guess",
                "--no-guess",
                "--recurse-submodules",
                "--overlay",
                "--no-overlay",
            ],
            optional: &[],
        },
    );
    if let Some(new) = args.value(&["-b", "-B", "--orphan"]) {
        a.title = match args.positionals.first() {
            Some(start) => format!("Create branch {new} from {start} and switch to it"),
            None => format!("Create branch {new} and switch to it"),
        };
        if args.has(&["-B"]) {
            a.warn("Resets an existing branch of that name to the new start point.");
        }
    } else if !args.paths.is_empty() || args.positionals.iter().any(|p| p == ".") {
        a.kind = Kind::Restore;
        let files = args.targets();
        let source = (!args.paths.is_empty())
            .then(|| args.positionals.first().cloned())
            .flatten();
        let files: Vec<String> = match &source {
            Some(_) => args.paths.clone(),
            None => files,
        };
        a.title = match &source {
            Some(rev) => format!("Replace files with their version in {rev}"),
            None => "Discard uncommitted changes in files".to_owned(),
        };
        a.list("Files", &files);
        a.warn("Overwrites these files in the working tree; their uncommitted changes are lost.");
    } else {
        let target = args.positionals.first().cloned().unwrap_or_default();
        a.title = if target.is_empty() {
            "Check out".to_owned()
        } else {
            format!("Switch to {target}")
        };
        if !target.is_empty() && args.positionals.len() == 1 {
            a.detail(
                "Note",
                format!(
                    "if {target} is a file rather than a branch, this discards its uncommitted \
                     changes"
                ),
            );
        }
        a.list("Files", args.positionals.get(1..).unwrap_or_default());
    }
    if args.has(&["-f", "--force"]) {
        a.warn("Throws away local changes that are in the way.");
    }
    a
}

fn git_switch(rest: &[String]) -> GitAction {
    let mut a = GitAction::new(Tool::Git, Kind::Switch, Reach::Local, "Switch");
    let args = parse_for(
        &mut a,
        rest,
        Spec {
            values: &["-c", "--create", "-C", "--force-create", "--orphan"],
            flags: &[
                "-d",
                "--detach",
                "-f",
                "--force",
                "--discard-changes",
                "-m",
                "--merge",
                "-t",
                "--track",
                "--no-track",
                "-q",
                "--quiet",
                "--guess",
                "--no-guess",
                "--progress",
                "--no-progress",
                "--recurse-submodules",
            ],
            optional: &[],
        },
    );
    let start = args.positionals.first().cloned();
    a.title = match (
        args.value(&["-c", "--create", "-C", "--force-create", "--orphan"]),
        start,
    ) {
        (Some(new), Some(start)) => format!("Create branch {new} from {start} and switch to it"),
        (Some(new), None) => format!("Create branch {new} and switch to it"),
        (None, Some(target)) => format!("Switch to {target}"),
        (None, None) => "Switch branches".to_owned(),
    };
    if args.has(&["-C", "--force-create"]) {
        a.warn("Resets an existing branch of that name to the new start point.");
    }
    if args.has(&["-f", "--force", "--discard-changes"]) {
        a.warn("Throws away local changes that are in the way.");
    }
    a
}

/// An in-progress operation's control (`--abort`, `--continue`, …), if `args` is one.
fn sequencer_step(args: &Args) -> Option<&'static str> {
    [
        ("--abort", "Abort"),
        ("--continue", "Continue"),
        ("--skip", "Skip a commit in"),
        ("--quit", "Stop"),
    ]
    .into_iter()
    .find(|(flag, _)| args.has(&[flag]))
    .map(|(_, words)| words)
}

fn git_merge(rest: &[String]) -> GitAction {
    let mut a = GitAction::new(Tool::Git, Kind::Merge, Reach::Local, "Merge");
    let args = parse_for(
        &mut a,
        rest,
        Spec {
            values: &[
                "-m",
                "-s",
                "--strategy",
                "-X",
                "--strategy-option",
                "-F",
                "--file",
                "--into-name",
            ],
            flags: &[
                "--no-ff",
                "--ff",
                "--ff-only",
                "--squash",
                "--no-squash",
                "--commit",
                "--no-commit",
                "--edit",
                "-e",
                "--no-edit",
                "--abort",
                "--continue",
                "--quit",
                "-q",
                "--quiet",
                "-v",
                "--verbose",
                "--stat",
                "--no-stat",
                "--allow-unrelated-histories",
                "--autostash",
                "--no-verify",
                "--signoff",
                "--log",
                "--no-log",
            ],
            optional: &["-S"],
        },
    );
    if let Some(step) = sequencer_step(&args) {
        a.title = format!("{step} the merge in progress");
        if step == "Abort" {
            a.warn("Drops the merge's resolved conflicts and returns to before it started.");
        }
        return a;
    }
    let branches = args.positionals.clone();
    a.title = format!(
        "Merge {} into the current branch{}",
        if branches.is_empty() {
            "the upstream".to_owned()
        } else {
            branches.join(", ")
        },
        if args.has(&["--squash"]) {
            " (squashed)"
        } else if args.has(&["--ff-only"]) {
            " (fast-forward only)"
        } else {
            ""
        }
    );
    a.detail("Message", args.values(&["-m"]).join("\n\n"));
    if args.has(&["--allow-unrelated-histories"]) {
        a.warn("Joins two histories that share no commit.");
    }
    a
}

fn git_rebase(rest: &[String]) -> GitAction {
    let mut a = GitAction::new(Tool::Git, Kind::Rebase, Reach::Local, "Rebase");
    let args = parse_for(
        &mut a,
        rest,
        Spec {
            values: &[
                "--onto",
                "-s",
                "--strategy",
                "-X",
                "--strategy-option",
                "-x",
                "--exec",
            ],
            flags: &[
                "-i",
                "--interactive",
                "--abort",
                "--continue",
                "--skip",
                "--quit",
                "--edit-todo",
                "--autosquash",
                "--no-autosquash",
                "--autostash",
                "--no-autostash",
                "--root",
                "-r",
                "--rebase-merges",
                "--keep-base",
                "-q",
                "--quiet",
                "-v",
                "--verbose",
                "--update-refs",
                "--no-update-refs",
                "-f",
                "--force-rebase",
                "--no-verify",
                "--committer-date-is-author-date",
                "--signoff",
            ],
            optional: &["-S"],
        },
    );
    if let Some(step) = sequencer_step(&args) {
        a.title = format!("{step} the rebase in progress");
        return a;
    }
    let onto = args.value(&["--onto"]);
    let upstream = args.positionals.first().cloned();
    let base = onto.clone().or(upstream.clone()).unwrap_or_else(|| {
        if args.has(&["--root"]) {
            "a new root".to_owned()
        } else {
            "its upstream".to_owned()
        }
    });
    let branch = args
        .positionals
        .get(1)
        .cloned()
        .unwrap_or_else(|| "the current branch".to_owned());
    a.title = format!(
        "{} {branch} onto {base}",
        if args.has(&["-i", "--interactive"]) {
            "Interactively rebase"
        } else {
            "Rebase"
        }
    );
    if let (Some(_), Some(up)) = (&onto, &upstream) {
        a.detail("Commits after", up.clone());
    }
    a.warn(
        "Rewrites the branch's commits. If they were already pushed, the branch will need a \
         force push.",
    );
    if let Some(exec) = args.value(&["-x", "--exec"]) {
        a.detail("Runs after each commit", exec);
        a.warn("Runs a shell command after each rebased commit.");
    }
    a
}

fn git_reset(rest: &[String]) -> GitAction {
    let mut a = GitAction::new(Tool::Git, Kind::Reset, Reach::Local, "Reset");
    let args = parse_for(
        &mut a,
        rest,
        flags(&[
            "--hard",
            "--soft",
            "--mixed",
            "--keep",
            "--merge",
            "-q",
            "--quiet",
            "-p",
            "--patch",
            "-N",
            "--intent-to-add",
            "--no-refresh",
            "--recurse-submodules",
        ]),
    );
    let target = args.positionals.first().cloned();
    let has_paths = !args.paths.is_empty() || args.positionals.len() > 1;
    if has_paths {
        let files = if args.paths.is_empty() {
            args.positionals[1..].to_vec()
        } else {
            args.paths.clone()
        };
        a.kind = Kind::Stage;
        a.title = "Unstage files".to_owned();
        a.list("Files", &files);
        a.detail("To", target.unwrap_or_default());
        return a;
    }
    let to = target.unwrap_or_else(|| "HEAD".to_owned());
    let mode = if args.has(&["--hard"]) {
        "hard"
    } else if args.has(&["--soft"]) {
        "soft"
    } else if args.has(&["--keep"]) {
        "keep"
    } else if args.has(&["--merge"]) {
        "merge"
    } else {
        "mixed"
    };
    a.title = match (mode, to.as_str()) {
        ("mixed", "HEAD") => "Unstage everything".to_owned(),
        ("hard", "HEAD") => "Discard every uncommitted change".to_owned(),
        (mode, to) => format!("Reset the current branch to {to} ({mode})"),
    };
    a.detail("To", to.clone());
    a.detail(
        "Keeps",
        match mode {
            "hard" => "nothing: the working tree and index are overwritten",
            "soft" => "the working tree and index (only the branch moves)",
            "keep" => "local changes, refusing if they would be overwritten",
            "merge" => "local changes not involved in a merge",
            _ => "the working tree (the index is reset)",
        },
    );
    if mode == "hard" {
        a.warn("Hard reset: uncommitted changes in tracked files are lost for good.");
    }
    if to != "HEAD" && mode != "keep" {
        a.warn("Moves the branch: commits after the new point are no longer on it.");
    }
    a
}

fn git_restore(rest: &[String]) -> GitAction {
    let mut a = GitAction::new(Tool::Git, Kind::Restore, Reach::Local, "Restore");
    let args = parse_for(
        &mut a,
        rest,
        Spec {
            values: &["-s", "--source", "--conflict", "--pathspec-from-file"],
            flags: &[
                "-S",
                "--staged",
                "-W",
                "--worktree",
                "-p",
                "--patch",
                "-q",
                "--quiet",
                "--ours",
                "--theirs",
                "-m",
                "--merge",
                "--overlay",
                "--no-overlay",
                "--ignore-unmerged",
                "--progress",
                "--no-progress",
                "--recurse-submodules",
            ],
            optional: &[],
        },
    );
    let staged = args.has(&["-S", "--staged"]);
    let worktree = args.has(&["-W", "--worktree"]) || !staged;
    let files = args.targets();
    a.title = match (staged, worktree) {
        (true, false) => "Unstage files".to_owned(),
        (true, true) => "Unstage and discard changes in files".to_owned(),
        _ => "Discard uncommitted changes in files".to_owned(),
    };
    if !staged || worktree {
        a.kind = Kind::Restore;
    } else {
        a.kind = Kind::Stage;
    }
    a.list("Files", &files);
    if let Some(source) = args.value(&["-s", "--source"]) {
        a.detail("From", source);
    }
    if worktree {
        a.warn("Overwrites these files in the working tree; their uncommitted changes are lost.");
    }
    a
}

fn git_add(rest: &[String]) -> GitAction {
    let mut a = GitAction::new(Tool::Git, Kind::Stage, Reach::Local, "Stage");
    let args = parse_for(
        &mut a,
        rest,
        Spec {
            values: &["--chmod", "--pathspec-from-file"],
            flags: &[
                "-A",
                "--all",
                "--no-all",
                "-u",
                "--update",
                "-p",
                "--patch",
                "-i",
                "--interactive",
                "-f",
                "--force",
                "-n",
                "--dry-run",
                "-v",
                "--verbose",
                "-N",
                "--intent-to-add",
                "--renormalize",
                "--ignore-errors",
                "--ignore-removal",
                "--refresh",
                "--sparse",
            ],
            optional: &[],
        },
    );
    let files = args.targets();
    a.title = if args.has(&["-A", "--all"]) || files.iter().any(|f| f == "." || f == ":/") {
        "Stage every change, new files included".to_owned()
    } else if args.has(&["-u", "--update"]) {
        "Stage changes to tracked files".to_owned()
    } else {
        "Stage files".to_owned()
    };
    a.list("Files", &files);
    if args.has(&["-f", "--force"]) {
        a.warn("Stages files .gitignore excludes (build output, secrets kept out of git).");
    }
    if args.has(&["-n", "--dry-run"]) {
        a.reach = Reach::ReadOnly;
        a.title = format!("Preview: {}", a.title);
    }
    a
}

fn git_rm(rest: &[String]) -> GitAction {
    let mut a = GitAction::new(Tool::Git, Kind::Remove, Reach::Local, "Remove");
    let args = parse_for(
        &mut a,
        rest,
        Spec {
            values: &["--pathspec-from-file"],
            flags: &[
                "--cached",
                "-r",
                "-f",
                "--force",
                "-n",
                "--dry-run",
                "-q",
                "--quiet",
                "--ignore-unmatch",
                "--sparse",
            ],
            optional: &[],
        },
    );
    let files = args.targets();
    a.list("Files", &files);
    if args.has(&["--cached"]) {
        a.title = "Stop tracking files (they stay on disk)".to_owned();
    } else {
        a.title = "Delete files and stage their removal".to_owned();
        a.warn("Deletes these files from the working tree.");
        if args.has(&["-f", "--force"]) {
            a.warn("Deletes them even with uncommitted changes, which are lost.");
        }
    }
    if args.has(&["-n", "--dry-run"]) {
        a.reach = Reach::ReadOnly;
        a.title = format!("Preview: {}", a.title);
        a.warnings.clear();
    }
    a
}

fn git_clean(rest: &[String]) -> GitAction {
    let mut a = GitAction::new(Tool::Git, Kind::Clean, Reach::Local, "Clean");
    let args = parse_for(
        &mut a,
        rest,
        Spec {
            values: &["-e", "--exclude"],
            flags: &[
                "-f",
                "--force",
                "-d",
                "-x",
                "-X",
                "-n",
                "--dry-run",
                "-q",
                "--quiet",
                "-i",
                "--interactive",
            ],
            optional: &[],
        },
    );
    let dry = args.has(&["-n", "--dry-run"]);
    let what = match (args.has(&["-x"]), args.has(&["-X"])) {
        (true, _) => "untracked and ignored files",
        (_, true) => "ignored files",
        _ => "untracked files",
    };
    let dirs = if args.has(&["-d"]) {
        " and folders"
    } else {
        ""
    };
    a.title = format!("{} {what}{dirs}", if dry { "List" } else { "Delete" });
    a.list("In", &args.targets());
    if dry {
        a.reach = Reach::ReadOnly;
    } else if args.has(&["-f", "--force"]) {
        a.warn(format!(
            "Permanently deletes {what}{dirs}: they were never committed, so git cannot bring \
             them back."
        ));
    }
    a
}

fn git_stash(rest: &[String]) -> GitAction {
    let mut a = GitAction::new(Tool::Git, Kind::Stash, Reach::Local, "Stash");
    let (action, rest) = match rest.first().map(String::as_str) {
        Some(
            s @ ("push" | "save" | "pop" | "apply" | "drop" | "clear" | "list" | "show" | "branch"
            | "create" | "store"),
        ) => (s, &rest[1..]),
        _ => ("push", rest),
    };
    let args = parse_for(
        &mut a,
        rest,
        Spec {
            values: &["-m", "--message", "--pathspec-from-file"],
            flags: &[
                "-u",
                "--include-untracked",
                "-a",
                "--all",
                "-k",
                "--keep-index",
                "--no-keep-index",
                "-p",
                "--patch",
                "-S",
                "--staged",
                "-q",
                "--quiet",
                "--index",
            ],
            optional: &[],
        },
    );
    let entry = args.positionals.first().cloned();
    let entry_words = entry
        .clone()
        .unwrap_or_else(|| "the latest stash".to_owned());
    a.title = match action {
        "push" | "save" => {
            a.detail(
                "Message",
                args.value(&["-m", "--message"]).unwrap_or_default(),
            );
            if action == "save" {
                a.detail("Message", args.positionals.join(" "));
            } else {
                a.list("Files", &args.paths);
            }
            if args.has(&["-u", "--include-untracked", "-a", "--all"]) {
                "Set aside every change, new files included".to_owned()
            } else {
                "Set aside uncommitted changes".to_owned()
            }
        }
        "pop" => format!("Re-apply {entry_words} and drop it"),
        "apply" => format!("Re-apply {entry_words}"),
        "drop" => {
            a.warn("Deletes a stash entry; its changes are lost unless applied elsewhere.");
            format!("Delete {entry_words}")
        }
        "clear" => {
            a.warn("Deletes every stash entry; their changes are lost.");
            "Delete every stash".to_owned()
        }
        "list" | "show" => {
            a.reach = Reach::ReadOnly;
            a.kind = Kind::Inspect;
            if action == "list" {
                "List stashes".to_owned()
            } else {
                format!("Show {entry_words}")
            }
        }
        "branch" => format!("Create branch {} from a stash", entry.unwrap_or_default()),
        _ => "Stash plumbing".to_owned(),
    };
    a
}

fn git_tag(rest: &[String]) -> GitAction {
    let mut a = GitAction::new(Tool::Git, Kind::Tag, Reach::Local, "Tag");
    let args = parse_for(
        &mut a,
        rest,
        Spec {
            values: &[
                "-m",
                "--message",
                "-F",
                "--file",
                "-u",
                "--local-user",
                "--sort",
                "--contains",
                "--no-contains",
                "--merged",
                "--no-merged",
                "--points-at",
                "--format",
                "--cleanup",
            ],
            flags: &[
                "-a",
                "--annotate",
                "-s",
                "--sign",
                "--no-sign",
                "-f",
                "--force",
                "-d",
                "--delete",
                "-v",
                "--verify",
                "-l",
                "--list",
                "-n",
                "-e",
                "--edit",
                "-i",
                "--ignore-case",
                "--column",
                "--no-column",
            ],
            optional: &[],
        },
    );
    let names = args.positionals.clone();
    if args.has(&["-d", "--delete"]) {
        a.title = format!("Delete tag {}", names.join(", "));
        a.warn("Deletes the tag here (the remote keeps its copy until it is deleted there).");
    } else if names.is_empty() || args.has(&["-l", "--list", "-v", "--verify"]) {
        a.reach = Reach::ReadOnly;
        a.kind = Kind::Inspect;
        a.title = "List tags".to_owned();
    } else {
        a.title = match names.as_slice() {
            [name] => format!("Tag the current commit {name}"),
            [name, at, ..] => format!("Tag {at} as {name}"),
            [] => unreachable!("names is not empty here"),
        };
        a.detail("Message", args.values(&["-m", "--message"]).join("\n\n"));
        if args.has(&["-f", "--force"]) {
            a.warn("Moves an existing tag of that name.");
        }
    }
    a
}

fn git_pick(rest: &[String], kind: Kind, verb: &str, label: &'static str) -> GitAction {
    let mut a = GitAction::new(Tool::Git, kind, Reach::Local, verb);
    let args = parse_for(
        &mut a,
        rest,
        Spec {
            values: &[
                "-m",
                "--mainline",
                "-s",
                "--strategy",
                "-X",
                "--strategy-option",
            ],
            flags: &[
                "-n",
                "--no-commit",
                "-e",
                "--edit",
                "--no-edit",
                "-x",
                "--signoff",
                "--abort",
                "--continue",
                "--skip",
                "--quit",
                "--ff",
                "--allow-empty",
                "--keep-redundant-commits",
            ],
            optional: &["-S"],
        },
    );
    let noun = if kind == Kind::Revert {
        "revert"
    } else {
        "cherry-pick"
    };
    if let Some(step) = sequencer_step(&args) {
        a.title = format!("{step} the {noun} in progress");
        return a;
    }
    let commits = args.positionals.clone();
    a.title = if kind == Kind::Revert {
        format!("Revert {} with new commits", commits.join(", "))
    } else {
        format!("Apply {} onto the current branch", commits.join(", "))
    };
    a.list(label, &commits);
    a
}

fn git_remote(rest: &[String]) -> GitAction {
    let mut a = GitAction::new(Tool::Git, Kind::Remote, Reach::Local, "Remotes");
    let action = rest.first().map(String::as_str).unwrap_or("");
    let args = parse(rest.get(1..).unwrap_or_default(), NO_OPTIONS);
    let p = &args.positionals;
    let at = |i: usize| p.get(i).cloned().unwrap_or_default();
    a.title = match action {
        "" | "-v" | "--verbose" | "show" | "get-url" => {
            a.reach = Reach::ReadOnly;
            a.kind = Kind::Inspect;
            "List remotes".to_owned()
        }
        "add" => {
            a.detail("URL", at(1));
            format!("Add remote {}", at(0))
        }
        "remove" | "rm" => {
            a.warn("Removes the remote and its remote-tracking branches here.");
            format!("Remove remote {}", at(0))
        }
        "rename" => format!("Rename remote {} to {}", at(0), at(1)),
        "set-url" => {
            a.detail("New URL", p.last().cloned().unwrap_or_default());
            a.warn("Changes where this remote's pushes and fetches go.");
            format!("Change the URL of remote {}", at(0))
        }
        "prune" | "update" => {
            a.reach = Reach::Remote;
            format!("Update remote {}", at(0))
        }
        other => {
            a.uncertain = true;
            format!("Run git remote {other}")
        }
    };
    a
}

// ---- gh ----

/// `gh`'s options that take a value, wherever they appear.
const GH_VALUES: &[&str] = &[
    "-R",
    "--repo",
    "-t",
    "--title",
    "-b",
    "--body",
    "-F",
    "--body-file",
    "-B",
    "--base",
    "-H",
    "--head",
    "-r",
    "--reviewer",
    "-a",
    "--assignee",
    "-l",
    "--label",
    "-m",
    "--milestone",
    "-p",
    "--project",
    "-n",
    "--notes",
    "--notes-file",
    "--target",
    "-X",
    "--method",
    "-f",
    "--raw-field",
    "--field",
    "-H",
    "--header",
    "--input",
    "-q",
    "--jq",
    "--template",
    "--json",
    "-L",
    "--limit",
    "-s",
    "--state",
    "-S",
    "--search",
    "-c",
    "--comment",
    "--add-label",
    "--remove-label",
    "--add-reviewer",
    "--remove-reviewer",
    "--add-assignee",
    "--remove-assignee",
    "--ref",
    "--subject",
    "--author-email",
    "--match-head-commit",
    "--workflow",
    "--branch",
    "-u",
    "--user",
    "--visibility",
    "--description",
    "--homepage",
    "--clone",
    "--source",
    "--remote",
    "--reason",
    "--delete-branch-on-merge",
];

const GH_FLAGS: &[&str] = &[
    "--draft",
    "--fill",
    "--fill-first",
    "--fill-verbose",
    "--web",
    "-w",
    "--merge",
    "--squash",
    "--rebase",
    "--delete-branch",
    "--admin",
    "--auto",
    "--disable-auto",
    "--approve",
    "--request-changes",
    "--prerelease",
    "--latest",
    "--generate-notes",
    "--verify-tag",
    "--yes",
    "-y",
    "--confirm",
    "--public",
    "--private",
    "--internal",
    "--push",
    "--paginate",
    "-i",
    "--include",
    "--silent",
    "--verbose",
    "--undo",
    "--force",
    "--no-maintainer-edit",
    "--dry-run",
    "--cleanup-tag",
    "--failed",
    "--debug",
];

/// Short options that are plain flags in one gh subcommand and take a value in others
/// (`-r` is `--reviewer <login>` in `pr create`, `--rebase` in `pr merge`).
fn gh_short_flags(group: &str, verb: &str) -> &'static [&'static str] {
    match (group, verb) {
        ("pr", "create" | "new") => &["-d", "-w", "-f"],
        ("pr", "merge") => &["-m", "-s", "-r", "-d"],
        ("pr", "review") => &["-a", "-r", "-c"],
        ("pr", "close") => &["-d"],
        ("release", "create" | "new") => &["-d", "-p"],
        ("repo", "create" | "new") => &["-p"],
        (_, "view" | "browse") => &["-w"],
        _ => &[],
    }
}

/// The options `gh <group> <verb>` knows, with its own meaning of the ambiguous shorts.
fn gh_spec(group: &str, verb: &str) -> (Vec<&'static str>, Vec<&'static str>) {
    let shorts = gh_short_flags(group, verb);
    let values = GH_VALUES
        .iter()
        .copied()
        .filter(|v| !shorts.contains(v))
        .collect();
    let flags = GH_FLAGS.iter().chain(shorts).copied().collect();
    (values, flags)
}

fn gh_parse(args: &[String], group: &str, verb: &str) -> Args {
    let (values, flags) = gh_spec(group, verb);
    parse(
        args,
        Spec {
            values: &values,
            flags: &flags,
            optional: &[],
        },
    )
}

fn gh(args: &[String]) -> Option<GitAction> {
    let group = args.first()?;
    if group.starts_with('-') {
        return None;
    }
    let verb = args.get(1).map(String::as_str).unwrap_or("");
    let rest = args.get(2..).unwrap_or_default();
    let mut a = GitAction::new(Tool::Gh, Kind::Other, Reach::Remote, "");
    let parsed = gh_parse(rest, group, verb);
    if !parsed.unknown.is_empty() {
        a.uncertain = true;
        a.detail("Other options", parsed.unknown.join(" "));
    }
    let target = parsed.positionals.first().cloned();
    let number = |prefix: &str| {
        target
            .clone()
            .map(|t| {
                if t.bytes().all(|b| b.is_ascii_digit()) {
                    format!("{prefix} #{t}")
                } else {
                    format!("{prefix} {t}")
                }
            })
            .unwrap_or_else(|| format!("the current branch's {prefix}"))
    };
    let read_only = matches!(
        verb,
        "view" | "list" | "status" | "diff" | "checks" | "ls" | "watch" | "download"
    );
    match group.as_str() {
        "pr" => {
            a.kind = Kind::PullRequest;
            a.title = match verb {
                "create" | "new" => {
                    let base = parsed.value(&["-B", "--base"]);
                    let head = parsed.value(&["-H", "--head"]);
                    a.detail(
                        "Title",
                        parsed.value(&["-t", "--title"]).unwrap_or_default(),
                    );
                    a.detail("Body", parsed.value(&["-b", "--body"]).unwrap_or_default());
                    a.detail(
                        "Body from",
                        parsed.value(&["-F", "--body-file"]).unwrap_or_default(),
                    );
                    a.detail(
                        "From branch",
                        head.clone()
                            .unwrap_or_else(|| "the current branch".to_owned()),
                    );
                    a.detail(
                        "Into",
                        base.clone()
                            .unwrap_or_else(|| "the default branch".to_owned()),
                    );
                    a.list("Reviewers", &parsed.values(&["-r", "--reviewer"]));
                    a.list("Labels", &parsed.values(&["-l", "--label"]));
                    if parsed.has(&["--fill", "--fill-first", "--fill-verbose"]) {
                        a.detail("Text", "filled in from the commits");
                    }
                    let draft = if parsed.has(&["-d", "--draft"]) {
                        "draft "
                    } else {
                        ""
                    };
                    if parsed.has(&["-w", "--web"]) {
                        format!("Open the {draft}pull request form in the browser")
                    } else {
                        format!("Open a {draft}pull request")
                    }
                }
                "merge" => {
                    let how = if parsed.has(&["-s", "--squash"]) {
                        "squash"
                    } else if parsed.has(&["-r", "--rebase"]) {
                        "rebase"
                    } else if parsed.has(&["-m", "--merge"]) {
                        "merge commit"
                    } else {
                        "default method"
                    };
                    a.detail("Method", how);
                    if parsed.has(&["-d", "--delete-branch"]) {
                        a.detail("Then", "deletes the branch, here and on GitHub");
                    }
                    if parsed.has(&["--admin"]) {
                        a.warn("Merges with admin rights, bypassing required reviews and checks.");
                    }
                    if parsed.has(&["--auto"]) {
                        format!("Merge {} once its checks pass", number("pull request"))
                    } else {
                        format!("Merge {}", number("pull request"))
                    }
                }
                "close" => {
                    if parsed.has(&["-d", "--delete-branch"]) {
                        a.warn("Also deletes the pull request's branch.");
                    }
                    format!("Close {}", number("pull request"))
                }
                "reopen" => format!("Reopen {}", number("pull request")),
                "comment" => {
                    a.detail(
                        "Comment",
                        parsed.value(&["-b", "--body"]).unwrap_or_default(),
                    );
                    format!("Comment on {}", number("pull request"))
                }
                "review" => {
                    a.detail(
                        "Comment",
                        parsed.value(&["-b", "--body"]).unwrap_or_default(),
                    );
                    let how = if parsed.has(&["-a", "--approve"]) {
                        "Approve"
                    } else if parsed.has(&["-r", "--request-changes"]) {
                        "Request changes on"
                    } else {
                        "Review"
                    };
                    format!("{how} {}", number("pull request"))
                }
                "edit" => {
                    a.detail(
                        "Title",
                        parsed.value(&["-t", "--title"]).unwrap_or_default(),
                    );
                    format!("Edit {}", number("pull request"))
                }
                "ready" => format!("Mark {} ready for review", number("pull request")),
                "checkout" | "co" => {
                    a.reach = Reach::Local;
                    format!("Check out {} here", number("pull request"))
                }
                _ if read_only => {
                    a.reach = Reach::ReadOnly;
                    a.kind = Kind::Inspect;
                    format!("Look at pull requests ({verb})")
                }
                other => {
                    a.uncertain = true;
                    format!("Run gh pr {other}")
                }
            };
        }
        "issue" => {
            a.kind = Kind::Issue;
            a.title = match verb {
                "create" | "new" => {
                    a.detail(
                        "Title",
                        parsed.value(&["-t", "--title"]).unwrap_or_default(),
                    );
                    a.detail("Body", parsed.value(&["-b", "--body"]).unwrap_or_default());
                    a.list("Labels", &parsed.values(&["-l", "--label"]));
                    "Open an issue".to_owned()
                }
                "close" => format!("Close {}", number("issue")),
                "reopen" => format!("Reopen {}", number("issue")),
                "comment" => {
                    a.detail(
                        "Comment",
                        parsed.value(&["-b", "--body"]).unwrap_or_default(),
                    );
                    format!("Comment on {}", number("issue"))
                }
                "edit" => format!("Edit {}", number("issue")),
                "delete" => {
                    a.warn("Deletes the issue on GitHub for good.");
                    format!("Delete {}", number("issue"))
                }
                "transfer" => format!("Move {} to another repository", number("issue")),
                _ if read_only => {
                    a.reach = Reach::ReadOnly;
                    a.kind = Kind::Inspect;
                    format!("Look at issues ({verb})")
                }
                other => {
                    a.uncertain = true;
                    format!("Run gh issue {other}")
                }
            };
        }
        "release" => {
            a.kind = Kind::Release;
            a.title = match verb {
                "create" | "new" => {
                    let tag = target.clone().unwrap_or_default();
                    a.detail("Tag", tag.clone());
                    a.detail(
                        "Title",
                        parsed.value(&["-t", "--title"]).unwrap_or_default(),
                    );
                    a.detail(
                        "Notes",
                        parsed.value(&["-n", "--notes"]).unwrap_or_default(),
                    );
                    a.detail(
                        "Notes from",
                        parsed.value(&["-F", "--notes-file"]).unwrap_or_default(),
                    );
                    a.list("Files", parsed.positionals.get(1..).unwrap_or_default());
                    if parsed.has(&["-d", "--draft"]) {
                        format!("Draft release {tag}")
                    } else {
                        a.warn("Publishes a release that others can see and download.");
                        format!("Publish release {tag}")
                    }
                }
                "delete" => {
                    a.warn("Deletes the release on GitHub.");
                    if parsed.has(&["--cleanup-tag"]) {
                        a.warn("Also deletes its tag on GitHub.");
                    }
                    format!("Delete release {}", target.clone().unwrap_or_default())
                }
                "upload" => format!(
                    "Upload files to release {}",
                    target.clone().unwrap_or_default()
                ),
                "edit" => format!("Edit release {}", target.clone().unwrap_or_default()),
                _ if read_only => {
                    a.reach = Reach::ReadOnly;
                    a.kind = Kind::Inspect;
                    format!("Look at releases ({verb})")
                }
                other => {
                    a.uncertain = true;
                    format!("Run gh release {other}")
                }
            };
        }
        "repo" => {
            a.kind = Kind::Repo;
            let name = target
                .clone()
                .unwrap_or_else(|| "this repository".to_owned());
            a.title = match verb {
                "create" | "new" => {
                    let visibility = if parsed.has(&["--public"]) {
                        "public "
                    } else if parsed.has(&["--private"]) {
                        "private "
                    } else {
                        ""
                    };
                    if parsed.has(&["--public"]) {
                        a.warn("Creates a public repository: anyone can see what is pushed.");
                    }
                    format!("Create {visibility}repository {name}")
                }
                "delete" => {
                    a.warn("Deletes the repository on GitHub, with its issues and pull requests.");
                    format!("Delete repository {name}")
                }
                "archive" => format!("Archive repository {name}"),
                "rename" => format!("Rename this repository to {name}"),
                "edit" => {
                    if parsed.value(&["--visibility"]).as_deref() == Some("public") {
                        a.warn("Makes the repository public.");
                    }
                    format!("Change the settings of {name}")
                }
                "fork" => format!("Fork {name}"),
                "clone" => {
                    a.kind = Kind::Clone;
                    format!("Clone {name}")
                }
                "sync" => {
                    if parsed.has(&["--force"]) {
                        a.warn("Force sync: discards commits that are only here.");
                    }
                    format!("Sync {name} with its parent")
                }
                _ if read_only => {
                    a.reach = Reach::ReadOnly;
                    a.kind = Kind::Inspect;
                    format!("Look at repositories ({verb})")
                }
                other => {
                    a.uncertain = true;
                    format!("Run gh repo {other}")
                }
            };
        }
        "workflow" | "run" => {
            a.kind = Kind::Workflow;
            a.title = match (group.as_str(), verb) {
                ("workflow", "run") => {
                    format!("Start workflow {}", target.clone().unwrap_or_default())
                }
                ("workflow", "enable" | "disable") => format!(
                    "{} workflow {}",
                    if verb == "enable" {
                        "Enable"
                    } else {
                        "Disable"
                    },
                    target.clone().unwrap_or_default()
                ),
                ("run", "rerun") => format!("Re-run run {}", target.clone().unwrap_or_default()),
                ("run", "cancel") => format!("Cancel run {}", target.clone().unwrap_or_default()),
                ("run", "delete") => format!("Delete run {}", target.clone().unwrap_or_default()),
                _ if read_only => {
                    a.reach = Reach::ReadOnly;
                    a.kind = Kind::Inspect;
                    format!("Look at workflow runs ({verb})")
                }
                (g, other) => {
                    a.uncertain = true;
                    format!("Run gh {g} {other}")
                }
            };
        }
        "api" => {
            a.kind = Kind::Api;
            // `gh api <endpoint> [flags]`: the endpoint is the verb slot.
            let parsed = gh_parse(args.get(1..).unwrap_or_default(), "api", "");
            let endpoint = parsed.positionals.first().cloned().unwrap_or_default();
            let fields = parsed.has(&["-f", "--raw-field", "-F", "--field", "--input"]);
            let method = parsed
                .value(&["-X", "--method"])
                .map(|m| m.to_ascii_uppercase())
                .unwrap_or_else(|| if fields { "POST" } else { "GET" }.to_owned());
            a.detail("Method", method.clone());
            a.detail("Endpoint", endpoint.clone());
            a.list(
                "Fields",
                &parsed.values(&["-f", "--raw-field", "-F", "--field"]),
            );
            a.title = format!("Call the GitHub API: {method} {endpoint}");
            if method == "GET" {
                a.reach = Reach::ReadOnly;
            } else {
                a.warn(
                    "A raw API call can change anything the account can; read the endpoint \
                     and fields.",
                );
            }
            if method == "DELETE" {
                a.warn("Deletes something on GitHub.");
            }
            a.uncertain = !parsed.unknown.is_empty();
            a.details.retain(|d| d.label != "Other options");
            if a.uncertain {
                a.detail("Other options", parsed.unknown.join(" "));
            }
        }
        "auth" => {
            a.kind = Kind::Config;
            a.title = match verb {
                "status" => {
                    a.reach = Reach::ReadOnly;
                    "Show the GitHub sign-in".to_owned()
                }
                "token" => {
                    a.reach = Reach::ReadOnly;
                    a.warn("Prints the GitHub token: anything that sees the output can use it.");
                    "Print the GitHub token".to_owned()
                }
                "login" => "Sign in to GitHub".to_owned(),
                "logout" => "Sign out of GitHub".to_owned(),
                other => format!("Run gh auth {other}"),
            };
        }
        "browse" => {
            a.kind = Kind::Inspect;
            a.reach = Reach::ReadOnly;
            a.title = "Open the repository in the browser".to_owned();
        }
        "secret" | "variable" => {
            a.kind = Kind::Config;
            a.title = match verb {
                "set" => format!("Set {group} {}", target.clone().unwrap_or_default()),
                "delete" | "remove" => {
                    a.warn(format!("Deletes the {group} on GitHub."));
                    format!("Delete {group} {}", target.clone().unwrap_or_default())
                }
                _ if read_only => {
                    a.reach = Reach::ReadOnly;
                    format!("List {group}s")
                }
                other => format!("Run gh {group} {other}"),
            };
        }
        other => {
            a.uncertain = true;
            a.title = format!("Run gh {other} {verb}").trim_end().to_owned();
            a.list("Arguments", rest);
        }
    }
    if let Some(repo) = parsed.value(&["-R", "--repo"]) {
        a.detail("Repository", repo);
    }
    a.title = a.title.trim_end().to_owned();
    Some(a)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn read(words: &[&str]) -> GitAction {
        let argv: Vec<String> = words.iter().map(|w| (*w).to_owned()).collect();
        describe(&argv).unwrap_or_else(|| panic!("no reading of {words:?}"))
    }

    fn detail<'a>(a: &'a GitAction, label: &str) -> Option<&'a str> {
        a.details
            .iter()
            .find(|d| d.label == label)
            .map(|d| d.value.as_str())
    }

    fn words(command: &str) -> Vec<String> {
        split(command).unwrap_or_else(|| panic!("no split of {command:?}"))
    }

    #[test]
    fn the_command_line_splits_as_a_shell_would() {
        assert_eq!(
            words(r#"git commit -m "feat(ui): add a PR button; and more""#),
            ["git", "commit", "-m", "feat(ui): add a PR button; and more"]
        );
        assert_eq!(
            words(r#"git commit -m 'it''s' -m "say \"hi\" \$5""#),
            ["git", "commit", "-m", "its", "-m", "say \"hi\" $5"]
        );
        assert_eq!(words("git  status   "), ["git", "status"]);
        assert_eq!(words("git reset --hard HEAD~3")[3], "HEAD~3");
        assert_eq!(words("git commit -m 'two\nlines'")[3], "two\nlines");
        assert_eq!(
            words(r#"a "x\y""#)[1],
            r"x\y",
            "an ordinary backslash stays"
        );
        // Anything a shell could word differently, or that runs something else, is no reading.
        for bad in [
            "git status; rm -rf x",
            "git status && git push",
            "git log | head",
            "git commit -m \"$(whoami)\"",
            "git commit -m \"$HOME\"",
            "git commit -m \"`id`\"",
            "git add *.rs",
            "git show ~/x",
            "git status > out",
            "GIT_DIR=/x git status",
            "git commit -m 'open",
            "git status\nrm x",
            "git status # comment",
            "git\tstatus",
            "",
        ] {
            assert_eq!(split(bad), None, "{bad:?}");
        }
    }

    #[test]
    fn a_quoted_heredoc_message_is_read_literally() {
        let command = "git commit -m \"$(cat <<'EOF'\nfeat(ui): add \"Open a PR\"\n\nBody with $HOME and `x`.\nEOF\n)\"";
        assert_eq!(
            words(command),
            [
                "git",
                "commit",
                "-m",
                "feat(ui): add \"Open a PR\"\n\nBody with $HOME and `x`."
            ]
        );
        let a = describe_command(command).expect("read");
        assert_eq!(
            a.title,
            "Commit the staged changes: “feat(ui): add \"Open a PR\"”"
        );
        // An unquoted delimiter expands the body, and anything after the `)` but inside the
        // quotes is still read.
        assert_eq!(split("git commit -m \"$(cat <<EOF\nx\nEOF\n)\""), None);
        assert_eq!(
            words("git commit -m \"$(cat <<'EOF'\nx\nEOF\n) tail\"")[3],
            "x tail"
        );
        assert_eq!(
            split("git commit -m \"$(cat <<'EOF'\nx\n)\""),
            None,
            "unterminated"
        );
    }

    #[test]
    fn one_plain_shell_wrapper_is_unwrapped() {
        let a = describe_command("/bin/bash -lc 'git push -u origin feat/x'").expect("read");
        assert_eq!(a.title, "Push feat/x to origin");
        assert!(describe_command("bash -lc 'git status; rm -rf x'").is_none());
        assert!(describe_command("bash -c 'ls'").is_none());
        assert!(describe_command("git status").is_some());
    }

    #[test]
    fn only_git_and_gh_with_a_subcommand_and_plain_text_are_read() {
        assert!(describe(&[]).is_none());
        assert!(describe(&["ls".to_owned()]).is_none());
        assert!(describe(&["git".to_owned()]).is_none());
        assert!(describe(&["git".to_owned(), "-C".to_owned(), "/x".to_owned()]).is_none());
        assert!(describe(&["gh".to_owned(), "--version".to_owned()]).is_none());
        // A bidi override could make the card say something the command does not.
        assert!(describe(&[
            "git".to_owned(),
            "commit".to_owned(),
            "-m".to_owned(),
            "fix \u{202E}txt".to_owned()
        ])
        .is_none());
        assert_eq!(read(&["/usr/bin/git", "status"]).kind, Kind::Inspect);
    }

    #[test]
    fn a_commit_names_its_message_and_what_it_takes() {
        let a = read(&["git", "commit", "-m", "Fix the thing", "-m", "Body text"]);
        assert_eq!(a.kind, Kind::Commit);
        assert_eq!(a.reach, Reach::Local);
        assert_eq!(a.title, "Commit the staged changes: “Fix the thing”");
        assert_eq!(detail(&a, "Message"), Some("Fix the thing\n\nBody text"));
        assert!(!a.destructive());
        assert!(!a.uncertain);

        // `-am msg`: a cluster whose last short takes the next word.
        let a = read(&["git", "commit", "-am", "wip"]);
        assert_eq!(a.title, "Commit every tracked change: “wip”");
        let a = read(&["git", "commit", "-mattached"]);
        assert_eq!(detail(&a, "Message"), Some("attached"));
        let a = read(&["git", "commit", "--message=long form", "--", "src/a.rs"]);
        assert_eq!(detail(&a, "Files"), Some("src/a.rs"));
        assert!(a.title.starts_with("Commit the named files"));
    }

    #[test]
    fn rewriting_or_skipping_hooks_is_warned() {
        let a = read(&["git", "commit", "--amend", "--no-edit"]);
        assert!(a.title.starts_with("Amend the last commit"));
        assert!(a.warnings[0].contains("Rewrites the last commit"));
        let a = read(&["git", "commit", "-n", "-m", "x"]);
        assert!(a.warnings.iter().any(|w| w.contains("hooks")));
        let a = read(&["git", "commit", "-S", "-m", "signed"]);
        assert!(!a.uncertain, "-S takes an optional attached key only");
        assert_eq!(detail(&a, "Message"), Some("signed"));
    }

    #[test]
    fn a_push_says_what_goes_where_and_warns_on_force() {
        let a = read(&["git", "push", "-u", "origin", "feat/x"]);
        assert_eq!(a.kind, Kind::Push);
        assert_eq!(a.reach, Reach::Remote);
        assert_eq!(a.title, "Push feat/x to origin");
        assert_eq!(detail(&a, "Remote"), Some("origin"));
        assert_eq!(detail(&a, "Upstream"), Some("set to this push"));
        assert!(!a.destructive());

        assert_eq!(
            read(&["git", "push"]).title,
            "Push the current branch to its upstream"
        );
        for force in [
            &["git", "push", "--force", "origin", "main"][..],
            &["git", "push", "-f"],
            &["git", "push", "origin", "+main"],
        ] {
            let a = read(force);
            assert!(a.warnings[0].starts_with("Force push"), "{force:?}");
        }
        let a = read(&["git", "push", "--force-with-lease=main", "origin", "main"]);
        assert!(a.warnings[0].contains("lease"));
        let a = read(&["git", "push", "origin", ":old"]);
        assert!(a.warnings.iter().any(|w| w.contains("Deletes")));
        assert_eq!(detail(&a, "Branches"), Some("delete old"));
        let a = read(&["git", "push", "--delete", "origin", "old"]);
        assert_eq!(a.title, "Delete old on origin");
        let a = read(&["git", "push", "origin", "HEAD:refs/heads/x"]);
        assert_eq!(detail(&a, "Branches"), Some("HEAD → refs/heads/x"));
        let a = read(&["git", "push", "--dry-run", "--force"]);
        assert_eq!(a.reach, Reach::ReadOnly);
        assert!(a.title.starts_with("Preview: "));
    }

    #[test]
    fn an_unknown_option_marks_the_reading_uncertain() {
        let a = read(&["git", "push", "--mystery", "origin", "main"]);
        assert!(a.uncertain);
        assert_eq!(detail(&a, "Other options"), Some("--mystery"));
        let a = read(&["git", "frobnicate", "x"]);
        assert!(a.uncertain);
        assert_eq!(a.title, "Run git frobnicate");
    }

    #[test]
    fn a_config_override_before_the_subcommand_is_warned() {
        let a = read(&["git", "-c", "core.sshCommand=evil", "push"]);
        assert_eq!(a.kind, Kind::Push);
        assert_eq!(detail(&a, "Git setting"), Some("core.sshCommand=evil"));
        assert!(a
            .warnings
            .iter()
            .any(|w| w.contains("Overrides a git setting")));
        let a = read(&["git", "-C", "/repo", "status"]);
        assert_eq!(detail(&a, "In folder"), Some("/repo"));
        assert!(!a.destructive());
        assert!(read(&["git", "--bogus", "status"]).uncertain);
    }

    #[test]
    fn branch_operations() {
        assert_eq!(read(&["git", "branch"]).reach, Reach::ReadOnly);
        assert_eq!(read(&["git", "branch", "-a"]).title, "List branches");
        assert_eq!(
            read(&["git", "branch", "feat/y"]).title,
            "Create branch feat/y"
        );
        assert_eq!(
            read(&["git", "branch", "feat/y", "main"]).title,
            "Create branch feat/y from main"
        );
        let a = read(&["git", "branch", "-d", "old"]);
        assert_eq!(a.title, "Delete branch old");
        assert!(!a.destructive(), "a safe delete refuses unmerged work");
        let a = read(&["git", "branch", "-D", "old"]);
        assert!(a.warnings[0].contains("not merged"));
        assert_eq!(
            read(&["git", "branch", "-m", "new"]).title,
            "Rename the current branch to new"
        );
        assert_eq!(
            read(&["git", "branch", "-u", "origin/main"]).title,
            "Set the upstream to origin/main"
        );
    }

    #[test]
    fn checkout_and_switch() {
        assert_eq!(read(&["git", "checkout", "main"]).title, "Switch to main");
        assert!(detail(&read(&["git", "checkout", "main"]), "Note").is_some());
        assert_eq!(
            read(&["git", "checkout", "-b", "feat/z"]).title,
            "Create branch feat/z and switch to it"
        );
        assert_eq!(
            read(&["git", "checkout", "-b", "feat/z", "origin/main"]).title,
            "Create branch feat/z from origin/main and switch to it"
        );
        let a = read(&["git", "checkout", "--", "src/a.rs"]);
        assert_eq!(a.kind, Kind::Restore);
        assert_eq!(detail(&a, "Files"), Some("src/a.rs"));
        assert!(a.destructive());
        let a = read(&["git", "checkout", "."]);
        assert!(a.destructive());
        let a = read(&["git", "checkout", "HEAD~1", "--", "a.rs"]);
        assert_eq!(a.title, "Replace files with their version in HEAD~1");
        assert_eq!(detail(&a, "Files"), Some("a.rs"));

        assert_eq!(read(&["git", "switch", "main"]).title, "Switch to main");
        assert_eq!(
            read(&["git", "switch", "-c", "new", "main"]).title,
            "Create branch new from main and switch to it"
        );
        assert!(read(&["git", "switch", "--discard-changes", "main"]).destructive());
    }

    #[test]
    fn history_rewrites_and_resets() {
        let a = read(&["git", "rebase", "origin/main"]);
        assert_eq!(a.title, "Rebase the current branch onto origin/main");
        assert!(a.destructive());
        assert_eq!(
            read(&["git", "rebase", "--abort"]).title,
            "Abort the rebase in progress"
        );
        let a = read(&["git", "rebase", "-x", "make test", "main"]);
        assert_eq!(detail(&a, "Runs after each commit"), Some("make test"));

        let a = read(&["git", "reset", "--hard"]);
        assert_eq!(a.title, "Discard every uncommitted change");
        assert!(a.warnings[0].starts_with("Hard reset"));
        let a = read(&["git", "reset", "--hard", "HEAD~2"]);
        assert_eq!(a.title, "Reset the current branch to HEAD~2 (hard)");
        assert_eq!(a.warnings.len(), 2);
        let a = read(&["git", "reset"]);
        assert_eq!(a.title, "Unstage everything");
        assert!(!a.destructive());
        let a = read(&["git", "reset", "HEAD", "--", "a.rs"]);
        assert_eq!(a.title, "Unstage files");
        assert!(!a.destructive());

        let a = read(&["git", "merge", "--no-ff", "feat/x"]);
        assert_eq!(a.title, "Merge feat/x into the current branch");
        assert!(!a.destructive());
    }

    #[test]
    fn staging_restoring_and_deleting_files() {
        assert_eq!(
            read(&["git", "add", "-A"]).title,
            "Stage every change, new files included"
        );
        assert_eq!(
            read(&["git", "add", "."]).title,
            "Stage every change, new files included"
        );
        assert_eq!(
            detail(&read(&["git", "add", "a.rs", "b.rs"]), "Files"),
            Some("a.rs, b.rs")
        );
        assert!(read(&["git", "add", "-f", "secret.env"]).destructive());

        let a = read(&["git", "restore", "a.rs"]);
        assert_eq!(a.title, "Discard uncommitted changes in files");
        assert!(a.destructive());
        let a = read(&["git", "restore", "--staged", "a.rs"]);
        assert_eq!(a.title, "Unstage files");
        assert!(!a.destructive());

        let a = read(&["git", "rm", "--cached", "a.rs"]);
        assert!(!a.destructive());
        assert!(read(&["git", "rm", "a.rs"]).destructive());

        let a = read(&["git", "clean", "-fdx"]);
        assert_eq!(a.title, "Delete untracked and ignored files and folders");
        assert!(a.warnings[0].starts_with("Permanently deletes"));
        let a = read(&["git", "clean", "-n"]);
        assert_eq!(a.reach, Reach::ReadOnly);
        assert!(!a.destructive());
    }

    #[test]
    fn stash_and_tag() {
        assert_eq!(
            read(&["git", "stash"]).title,
            "Set aside uncommitted changes"
        );
        assert_eq!(
            read(&["git", "stash", "-u"]).title,
            "Set aside every change, new files included"
        );
        assert_eq!(
            read(&["git", "stash", "pop"]).title,
            "Re-apply the latest stash and drop it"
        );
        assert!(read(&["git", "stash", "drop", "stash@{1}"]).destructive());
        assert!(read(&["git", "stash", "clear"]).destructive());
        assert_eq!(read(&["git", "stash", "list"]).reach, Reach::ReadOnly);

        assert_eq!(read(&["git", "tag"]).reach, Reach::ReadOnly);
        let a = read(&["git", "tag", "-a", "v1.2.0", "-m", "Release 1.2.0"]);
        assert_eq!(a.title, "Tag the current commit v1.2.0");
        assert_eq!(detail(&a, "Message"), Some("Release 1.2.0"));
        assert!(read(&["git", "tag", "-d", "v1"]).destructive());
    }

    #[test]
    fn read_only_commands_are_read_only_unless_they_write_or_run_something() {
        for words in [
            &["git", "status", "--short"][..],
            &["git", "log", "-3", "--oneline"],
            &["git", "diff", "HEAD~1"],
            &["git", "show", "abc123"],
        ] {
            let a = read(words);
            assert_eq!(a.reach, Reach::ReadOnly, "{words:?}");
            assert!(!a.destructive());
        }
        let a = read(&["git", "log", "--output=/tmp/x"]);
        assert_eq!(a.reach, Reach::Local);
        assert!(a.destructive());
    }

    #[test]
    fn fetch_pull_and_remote() {
        assert_eq!(read(&["git", "fetch", "origin"]).title, "Fetch from origin");
        assert_eq!(
            read(&["git", "fetch", "--all", "--prune"]).title,
            "Fetch from every remote"
        );
        assert_eq!(
            read(&["git", "pull", "--rebase", "origin", "main"]).title,
            "Pull from origin/main and rebase onto it"
        );
        assert_eq!(
            read(&["git", "pull"]).title,
            "Pull from the upstream and merge"
        );
        assert_eq!(read(&["git", "remote", "-v"]).reach, Reach::ReadOnly);
        let a = read(&["git", "remote", "add", "upstream", "https://github.com/o/r"]);
        assert_eq!(a.title, "Add remote upstream");
        assert_eq!(detail(&a, "URL"), Some("https://github.com/o/r"));
        assert!(read(&["git", "remote", "set-url", "origin", "x"]).destructive());
    }

    #[test]
    fn gh_pull_requests() {
        let a = read(&[
            "gh",
            "pr",
            "create",
            "--title",
            "Add PR button",
            "--body",
            "Details",
            "--base",
            "master",
            "--draft",
        ]);
        assert_eq!(a.tool, Tool::Gh);
        assert_eq!(a.kind, Kind::PullRequest);
        assert_eq!(a.reach, Reach::Remote);
        assert_eq!(a.title, "Open a draft pull request");
        assert_eq!(detail(&a, "Title"), Some("Add PR button"));
        assert_eq!(detail(&a, "Body"), Some("Details"));
        assert_eq!(detail(&a, "Into"), Some("master"));
        assert_eq!(detail(&a, "From branch"), Some("the current branch"));
        assert!(!a.uncertain);

        let a = read(&["gh", "pr", "merge", "45", "--squash", "--delete-branch"]);
        assert_eq!(a.title, "Merge pull request #45");
        assert_eq!(detail(&a, "Method"), Some("squash"));
        assert!(!a.destructive());
        assert!(read(&["gh", "pr", "merge", "45", "--admin"]).destructive());
        assert_eq!(read(&["gh", "pr", "view", "45"]).reach, Reach::ReadOnly);
        assert_eq!(read(&["gh", "pr", "checkout", "7"]).reach, Reach::Local);
        assert_eq!(
            read(&["gh", "pr", "review", "7", "--approve"]).title,
            "Approve pull request #7"
        );
        let a = read(&["gh", "pr", "create", "-R", "o/r", "--fill"]);
        assert_eq!(detail(&a, "Repository"), Some("o/r"));
    }

    #[test]
    fn gh_releases_repos_and_raw_api_calls() {
        let a = read(&["gh", "release", "create", "v3.1.0", "--notes", "Fixes"]);
        assert_eq!(a.title, "Publish release v3.1.0");
        assert!(a.destructive(), "publishing is outward-facing");
        assert_eq!(
            read(&["gh", "release", "create", "v3.1.0", "--draft"]).title,
            "Draft release v3.1.0"
        );
        assert!(read(&["gh", "repo", "delete", "o/r", "--yes"]).destructive());
        assert!(read(&["gh", "repo", "create", "x", "--public"]).destructive());
        assert!(!read(&["gh", "repo", "create", "x", "--private"]).destructive());

        let a = read(&["gh", "api", "repos/o/r/pulls"]);
        assert_eq!(a.reach, Reach::ReadOnly);
        assert_eq!(a.title, "Call the GitHub API: GET repos/o/r/pulls");
        let a = read(&["gh", "api", "repos/o/r/issues", "-f", "title=x"]);
        assert_eq!(detail(&a, "Method"), Some("POST"));
        assert!(a.destructive());
        let a = read(&["gh", "api", "-X", "delete", "repos/o/r"]);
        assert!(a.warnings.iter().any(|w| w.contains("Deletes")));
        assert!(read(&["gh", "auth", "token"]).destructive());
        assert!(read(&["gh", "mystery", "thing"]).uncertain);
    }
}
