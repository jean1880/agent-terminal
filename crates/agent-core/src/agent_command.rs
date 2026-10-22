//! An agent CLI launched from a shell command, recognised for what it is (pure: no I/O).
//!
//! An agent that runs `cross-agent claude "review this"`, `claude -p …`, `codex exec …` or
//! `agy -p …` through its shell tool starts a sub-agent that no delegation tool announced.
//! [`parse`] reads a command line and names the first such launch; [`from_tool_input`] does the
//! same for the input of the three drivers' shell tools.
//!
//! Like [`crate::git_command`] it is a reading aid, never a gate: it says what the command
//! looks like, and nothing here allows or refuses anything by itself. The shell is read the way
//! a shell would split it, but not executed or expanded. Chains (`&&`, `;`, `|`, newlines),
//! subshells, redirections and heredoc bodies are separated out, and wrappers (`sudo`, `env`,
//! `timeout 600`, `nohup`, `bash -c '…'`, leading `NAME=value`) are looked through. A word that
//! merely mentions an agent (`echo "claude -p"`, `grep codex exec file`, `which claude`) is not
//! a launch. Its ceilings are honest ones: an option it does not know is read as a flag, so a
//! value-taking option it has not heard of may be misread as the prompt, and a command built by
//! expansion (`$AGENT -p x`) is not seen.

use serde_json::Value;

use crate::adapter::Driver;

/// How the agent was reached.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Via {
    /// Through the `cross-agent` delegation CLI.
    CrossAgent,
    /// By running the agent's own CLI headless (`claude -p`, `codex exec`, `agy -p`).
    Direct,
}

/// One recognised agent launch.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AgentCommand {
    /// The agent that was launched.
    pub driver: Driver,
    pub via: Via,
    /// The prompt argument, when it is on the command line as plain text (not read from a file or
    /// stdin).
    pub task: Option<String>,
    /// The model it was asked to use (`-m` / `--model`), if any.
    pub model: Option<String>,
}

/// How many `bash -c '…'` layers are looked through.
const MAX_DEPTH: usize = 4;

/// The first agent launch in `command`, or `None` when it holds none.
pub fn parse(command: &str) -> Option<AgentCommand> {
    parse_at(command, 0)
}

/// The first agent launch in an already-split command (`["bash", "-lc", "claude -p x"]`).
pub fn parse_argv(argv: &[String]) -> Option<AgentCommand> {
    from_words(argv, 0)
}

/// The agent launch in a driver's shell-tool call, or `None` for any other tool or command.
///
/// `tool` is the provider's tool name: Claude `Bash` (`{command}`), Codex `command_execution`
/// (`{command}`, a string or an argv array), agy `run_command` (`{CommandLine}`). The command is
/// read from the first non-empty of `CommandLine`, `command` and `cmd`, as the approval rules do.
pub fn from_tool_input(tool: &str, input: &Value) -> Option<AgentCommand> {
    if !matches!(tool, "Bash" | "bash" | "command_execution" | "run_command") {
        return None;
    }
    for key in ["CommandLine", "command", "cmd"] {
        match input.get(key) {
            Some(Value::String(s)) if !s.trim().is_empty() => return parse(s),
            Some(Value::Array(items)) if !items.is_empty() => {
                let argv: Option<Vec<String>> = items
                    .iter()
                    .map(|v| v.as_str().map(str::to_owned))
                    .collect();
                return argv.and_then(|a| parse_argv(&a));
            }
            _ => {}
        }
    }
    None
}

fn parse_at(command: &str, depth: usize) -> Option<AgentCommand> {
    if depth > MAX_DEPTH {
        return None;
    }
    let (segments, subs) = segments(command);
    segments
        .iter()
        .find_map(|words| from_words(words, depth))
        .or_else(|| subs.iter().find_map(|sub| parse_at(sub, depth + 1)))
}

// ---- reading the command line ----

struct Lexer {
    out: Vec<Vec<String>>,
    /// Command substitutions found inside double quotes, each a command line of its own.
    subs: Vec<String>,
    words: Vec<String>,
    word: String,
    in_word: bool,
}

impl Lexer {
    fn end_word(&mut self) {
        if self.in_word {
            self.words.push(std::mem::take(&mut self.word));
            self.in_word = false;
        }
    }

    fn end_segment(&mut self) {
        self.end_word();
        if !self.words.is_empty() {
            self.out.push(std::mem::take(&mut self.words));
        }
    }

    fn push(&mut self, c: char) {
        self.word.push(c);
        self.in_word = true;
    }
}

/// A shell word starting at `chars[i]`, with its quotes removed, and the index after it. Used for
/// what follows a redirection, which is not a command word.
fn plain_word(chars: &[char], mut i: usize) -> (String, usize) {
    let mut word = String::new();
    while let Some(&c) = chars.get(i) {
        match c {
            ' ' | '\t' | '\n' | '\r' | ';' | '|' | '&' | '<' | '>' | '(' | ')' => break,
            '\'' => {
                i += 1;
                while let Some(&q) = chars.get(i) {
                    i += 1;
                    if q == '\'' {
                        break;
                    }
                    word.push(q);
                }
            }
            '"' => {
                i += 1;
                while let Some(&q) = chars.get(i) {
                    i += 1;
                    if q == '"' {
                        break;
                    }
                    word.push(q);
                }
            }
            c => {
                word.push(c);
                i += 1;
            }
        }
    }
    (word, i)
}

/// Skips the bodies of the heredocs opened on the line just ended: every line up to and
/// including the one that is only the delimiter. Returns the index after them.
fn skip_heredocs(chars: &[char], mut i: usize, delimiters: &[String]) -> usize {
    for delimiter in delimiters {
        while i < chars.len() {
            let end = chars[i..]
                .iter()
                .position(|&c| c == '\n')
                .map_or(chars.len(), |p| p + i);
            let line: String = chars[i..end].iter().collect();
            i = (end + 1).min(chars.len());
            if line.trim() == delimiter {
                break;
            }
        }
    }
    i
}

/// Splits a command line into simple commands, each as its words with quotes removed. Operators
/// that end a command (`;`, `&`, `|`, `(`, `)`, a backtick, a newline) split it; redirections,
/// comments and heredoc bodies are dropped. Nothing is expanded.
fn segments(command: &str) -> (Vec<Vec<String>>, Vec<String>) {
    let chars: Vec<char> = command.chars().collect();
    let mut lx = Lexer {
        out: Vec::new(),
        subs: Vec::new(),
        words: Vec::new(),
        word: String::new(),
        in_word: false,
    };
    let mut heredocs: Vec<String> = Vec::new();
    let mut i = 0;
    while i < chars.len() {
        let c = chars[i];
        i += 1;
        match c {
            ' ' | '\t' | '\r' => lx.end_word(),
            '\n' => {
                lx.end_segment();
                if !heredocs.is_empty() {
                    i = skip_heredocs(&chars, i, &std::mem::take(&mut heredocs));
                }
            }
            ';' | '|' | '(' | ')' | '`' => lx.end_segment(),
            '&' if chars.get(i) == Some(&'>') => {
                // `&>file`: a redirection of both streams.
                lx.end_word();
                i += 1;
                if chars.get(i) == Some(&'>') {
                    i += 1;
                }
                i = plain_word(&chars, skip_blanks(&chars, i)).1;
            }
            '&' => lx.end_segment(),
            '<' | '>' => {
                // `2>file`: the descriptor is part of the redirection, not a word.
                if lx.in_word && lx.word.bytes().all(|b| b.is_ascii_digit()) {
                    lx.word.clear();
                    lx.in_word = false;
                }
                lx.end_word();
                let mut op = String::from(c);
                while let Some(&n) = chars.get(i).filter(|&&n| n == '<' || n == '>') {
                    op.push(n);
                    i += 1;
                }
                if chars.get(i) == Some(&'&') {
                    i += 1;
                }
                let heredoc = op == "<<";
                if heredoc && chars.get(i) == Some(&'-') {
                    i += 1;
                }
                let (target, next) = plain_word(&chars, skip_blanks(&chars, i));
                i = next;
                if heredoc && !target.is_empty() {
                    heredocs.push(target);
                }
            }
            '#' if !lx.in_word => {
                while chars.get(i).is_some_and(|&n| n != '\n') {
                    i += 1;
                }
            }
            '\\' => match chars.get(i) {
                Some('\n') => i += 1,
                Some(&n) => {
                    lx.push(n);
                    i += 1;
                }
                None => {}
            },
            '\'' => {
                lx.in_word = true;
                while let Some(&q) = chars.get(i) {
                    i += 1;
                    if q == '\'' {
                        break;
                    }
                    lx.word.push(q);
                }
            }
            '"' => {
                lx.in_word = true;
                while let Some(&q) = chars.get(i) {
                    i += 1;
                    match q {
                        '"' => break,
                        '\\' => match chars.get(i) {
                            Some(&e @ ('"' | '\\' | '$' | '`')) => {
                                lx.word.push(e);
                                i += 1;
                            }
                            Some('\n') => i += 1,
                            _ => lx.word.push('\\'),
                        },
                        // A substitution runs its command even inside double quotes; its text
                        // stays in the word and is read again as a command line of its own.
                        '$' | '`' if q == '`' || chars.get(i) == Some(&'(') => {
                            let (open, close) = if q == '`' {
                                (None, '`')
                            } else {
                                (Some('('), ')')
                            };
                            let start = if q == '`' { i } else { i + 1 };
                            let mut level = 0usize;
                            let mut j = i;
                            while let Some(&n) = chars.get(j) {
                                if Some(n) == open {
                                    level += 1;
                                } else if n == close {
                                    if level <= 1 {
                                        break;
                                    }
                                    level -= 1;
                                }
                                j += 1;
                            }
                            let end = j.min(chars.len());
                            lx.subs.push(chars[start.min(end)..end].iter().collect());
                            lx.word.push(q);
                            lx.word.extend(&chars[i..(end + 1).min(chars.len())]);
                            i = (end + 1).min(chars.len());
                        }
                        q => lx.word.push(q),
                    }
                }
            }
            '$' if chars.get(i) == Some(&'\'') => {
                // `$'…\n…'`: C-style escapes.
                lx.in_word = true;
                i += 1;
                while let Some(&q) = chars.get(i) {
                    i += 1;
                    match q {
                        '\'' => break,
                        '\\' => {
                            let e = chars.get(i).copied();
                            i += usize::from(e.is_some());
                            match e {
                                Some('n') => lx.word.push('\n'),
                                Some('t') => lx.word.push('\t'),
                                Some(o) => lx.word.push(o),
                                None => {}
                            }
                        }
                        q => lx.word.push(q),
                    }
                }
            }
            c => lx.push(c),
        }
    }
    lx.end_segment();
    (lx.out, lx.subs)
}

fn skip_blanks(chars: &[char], mut i: usize) -> usize {
    while chars.get(i).is_some_and(|c| matches!(c, ' ' | '\t')) {
        i += 1;
    }
    i
}

// ---- reading one simple command ----

fn basename(word: &str) -> &str {
    word.rsplit('/').next().unwrap_or(word)
}

fn is_assignment(word: &str) -> bool {
    word.split_once('=').is_some_and(|(name, _)| {
        !name.is_empty()
            && !name.starts_with(|c: char| c.is_ascii_digit())
            && name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
    })
}

/// Reserved words and groupers that can open a simple command.
const OPENERS: &[&str] = &[
    "if", "then", "else", "elif", "do", "while", "until", "{", "!",
];

/// Shells whose `-c` argument is another command line.
const SHELLS: &[&str] = &["sh", "bash", "zsh", "dash", "ksh", "ash", "fish"];

/// Skips a wrapper's options. `with_value` lists the options (exact words) that take the next
/// word; any other `-x` or `--x` is a flag.
fn skip_options(words: &[String], mut i: usize, with_value: &[&str]) -> usize {
    while let Some(w) = words.get(i) {
        if w == "--" {
            return i + 1;
        }
        if !w.starts_with('-') || w == "-" {
            break;
        }
        i += if with_value.contains(&w.as_str()) {
            2
        } else {
            1
        };
    }
    i
}

fn from_words(words: &[String], depth: usize) -> Option<AgentCommand> {
    let mut i = 0;
    loop {
        let w = words.get(i)?;
        if is_assignment(w) || OPENERS.contains(&w.as_str()) {
            i += 1;
            continue;
        }
        i = match basename(w) {
            "sudo" | "doas" => skip_options(
                words,
                i + 1,
                &[
                    "-u", "-g", "-C", "-h", "-p", "-r", "-t", "-T", "-D", "-U", "--user",
                    "--group", "--host", "--prompt", "--chdir", "--role", "--type",
                ],
            ),
            "env" => {
                let mut j = skip_options(
                    words,
                    i + 1,
                    &["-u", "-C", "-S", "--unset", "--chdir", "--split-string"],
                );
                while words.get(j).is_some_and(|w| is_assignment(w)) {
                    j += 1;
                }
                j
            }
            "timeout" => {
                let j = skip_options(words, i + 1, &["-s", "-k", "--signal", "--kill-after"]);
                // The duration.
                j + usize::from(words.get(j).is_some())
            }
            "nice" => skip_options(words, i + 1, &["-n", "--adjustment"]),
            "stdbuf" => skip_options(words, i + 1, &["-i", "-o", "-e"]),
            "nohup" | "time" | "exec" | "command" | "builtin" | "setsid" | "unbuffer" => {
                skip_options(words, i + 1, &[])
            }
            _ => break,
        };
    }
    let program = basename(words.get(i)?);
    let rest = &words[i + 1..];
    match program {
        "cross-agent" => cross_agent(rest),
        "claude" => claude(rest),
        "codex" => codex(rest),
        "agy" => agy(rest),
        p if SHELLS.contains(&p) => {
            let body = shell_body(rest)?;
            parse_at(&body, depth + 1)
        }
        _ => None,
    }
}

/// The command a shell is told to run: the word after its `-c` option (`-c`, `-lc`, `-ec`, …).
fn shell_body(args: &[String]) -> Option<String> {
    let mut i = 0;
    while let Some(a) = args.get(i) {
        if a == "--" || !a.starts_with('-') {
            return None;
        }
        let cluster = a.strip_prefix('-').unwrap_or(a);
        if !a.starts_with("--") && cluster.chars().all(|c| c.is_ascii_alphabetic()) {
            if cluster.contains('c') {
                return args.get(i + 1).cloned();
            }
            // `-o option` and `-O option` take a word.
            i += if cluster.ends_with(['o', 'O']) { 2 } else { 1 };
        } else {
            i += 1;
        }
    }
    None
}

// ---- options ----

/// An argument list split into its options and positionals. Option names are kept without their
/// dashes (`-m` is `m`, `--model` is `model`).
struct Scan {
    opts: Vec<(String, Option<String>)>,
    positionals: Vec<String>,
}

impl Scan {
    fn has(&self, names: &[&str]) -> bool {
        self.opts.iter().any(|(n, _)| names.contains(&n.as_str()))
    }

    /// The last non-empty value given to any of `names`.
    fn value(&self, names: &[&str]) -> Option<String> {
        self.opts
            .iter()
            .rev()
            .filter(|(n, _)| names.contains(&n.as_str()))
            .find_map(|(_, v)| v.clone().filter(|v| !v.is_empty()))
    }
}

/// Scans `args`. `values` lists the options (dashless) that take a value; every other option is
/// a flag. With `go`, a single dash starts a whole option name (`-model x`), as Go's flag
/// package reads it; otherwise `-m` starts a cluster of shorts and a value-taking short takes the
/// rest of its word (`-mfast`) or the next one.
fn scan(args: &[String], values: &[&str], go: bool) -> Scan {
    let mut out = Scan {
        opts: Vec::new(),
        positionals: Vec::new(),
    };
    let mut i = 0;
    while i < args.len() {
        let arg = &args[i];
        i += 1;
        if arg == "--" {
            out.positionals.extend(args[i..].iter().cloned());
            break;
        }
        let long = arg.strip_prefix("--").filter(|l| !l.is_empty());
        let go_long = if go && long.is_none() {
            arg.strip_prefix('-').filter(|l| !l.is_empty())
        } else {
            None
        };
        if let Some(name) = long.or(go_long) {
            let (name, attached) = match name.split_once('=') {
                Some((n, v)) => (n, Some(v.to_owned())),
                None => (name, None),
            };
            if values.contains(&name) {
                let value = attached.or_else(|| {
                    let next = args.get(i).cloned();
                    i += usize::from(next.is_some());
                    next
                });
                out.opts.push((name.to_owned(), value));
            } else {
                out.opts.push((name.to_owned(), attached));
            }
        } else if arg.len() > 1 && arg.starts_with('-') {
            let chars: Vec<char> = arg[1..].chars().collect();
            for (j, c) in chars.iter().enumerate() {
                let name = c.to_string();
                if values.contains(&name.as_str()) {
                    let rest: String = chars[j + 1..].iter().collect();
                    let value = if rest.is_empty() {
                        let next = args.get(i).cloned();
                        i += usize::from(next.is_some());
                        next
                    } else {
                        Some(rest.strip_prefix('=').unwrap_or(&rest).to_owned())
                    };
                    out.opts.push((name, value));
                    break;
                }
                out.opts.push((name, None));
            }
        } else {
            out.positionals.push(arg.clone());
        }
    }
    out
}

/// Text that is safe to show: no control (a line break and tab aside), bidi or zero-width
/// character.
fn plain(s: &str) -> bool {
    !s.chars().any(|c| {
        (c.is_control() && !matches!(c, '\n' | '\t'))
            || matches!(c,
                '\u{200B}'..='\u{200F}' | '\u{202A}'..='\u{202E}' | '\u{2060}'..='\u{2064}'
                | '\u{2066}'..='\u{2069}' | '\u{FEFF}' | '\u{E0000}'..='\u{E007F}')
    })
}

fn shown(text: Option<String>) -> Option<String> {
    text.filter(|t| !t.trim().is_empty() && t != "-" && plain(t))
}

fn asks_for_help(scan: &Scan) -> bool {
    scan.has(&["help", "h", "version", "V"])
}

// ---- the four programs ----

fn cross_agent(rest: &[String]) -> Option<AgentCommand> {
    let (sub, rest) = rest.split_first()?;
    let driver = match sub.as_str() {
        "agy" => Driver::Agy,
        "claude" => Driver::Claude,
        "codex" => Driver::Codex,
        _ => return None,
    };
    let args = scan(
        rest,
        &[
            "a",
            "agent",
            "m",
            "model",
            "e",
            "effort",
            "t",
            "timeout",
            "prompt-file",
            "task-id",
            "review-id",
        ],
        false,
    );
    if asks_for_help(&args) {
        return None;
    }
    let from_file = args.has(&["prompt-file"]);
    // Without a prompt there is nothing to run.
    if !from_file && args.positionals.is_empty() {
        return None;
    }
    let task = if from_file {
        None
    } else {
        shown(args.positionals.first().cloned())
    };
    Some(AgentCommand {
        driver,
        via: Via::CrossAgent,
        task,
        model: shown(args.value(&["m", "model"])),
    })
}

/// `claude` subcommands: none of these launches an agent.
const CLAUDE_SUBCOMMANDS: &[&str] = &[
    "mcp",
    "config",
    "doctor",
    "update",
    "upgrade",
    "install",
    "migrate-installer",
    "setup-token",
    "plugin",
    "plugins",
    "agents",
    "auth",
    "project",
];

fn claude(rest: &[String]) -> Option<AgentCommand> {
    let args = scan(
        rest,
        &[
            "model",
            "agent",
            "add-dir",
            "allowedTools",
            "allowed-tools",
            "disallowedTools",
            "disallowed-tools",
            "tools",
            "mcp-config",
            "system-prompt",
            "system-prompt-file",
            "append-system-prompt",
            "append-system-prompt-file",
            "permission-mode",
            "output-format",
            "input-format",
            "max-turns",
            "session-id",
            "settings",
            "fallback-model",
            "effort",
            "max-budget-usd",
            "plugin-dir",
            "setting-sources",
            "betas",
            "permission-prompt-tool",
            "json-schema",
            "debug-file",
            "file",
            "from-pr",
            "name",
            "n",
            "resume",
            "r",
        ],
        false,
    );
    let subcommand = args
        .positionals
        .first()
        .is_some_and(|p| CLAUDE_SUBCOMMANDS.contains(&p.as_str()));
    if !args.has(&["p", "print"]) || asks_for_help(&args) || subcommand {
        return None;
    }
    Some(AgentCommand {
        driver: Driver::Claude,
        via: Via::Direct,
        task: shown(args.positionals.first().cloned()),
        model: shown(args.value(&["model"])),
    })
}

fn codex(rest: &[String]) -> Option<AgentCommand> {
    let args = scan(
        rest,
        &[
            "c",
            "config",
            "m",
            "model",
            "p",
            "profile",
            "s",
            "sandbox",
            "a",
            "ask-for-approval",
            "C",
            "cd",
            "i",
            "image",
            "add-dir",
            "enable",
            "disable",
            "local-provider",
            "thread-source",
            "cyber-access-program",
            "output-schema",
            "color",
            "o",
            "output-last-message",
        ],
        false,
    );
    if asks_for_help(&args) {
        return None;
    }
    let (sub, tail) = args.positionals.split_first()?;
    // `codex review` is the non-interactive review; `codex exec` and its alias `e` run a prompt.
    let task = match (sub.as_str(), tail.first().map(String::as_str)) {
        // `exec resume [ID] [PROMPT]`, `exec fork [ID] [PROMPT]`: the prompt is the last word,
        // and only when it is not the session id.
        ("exec" | "e", Some("resume" | "fork")) => {
            let after = tail.get(1..).unwrap_or_default();
            let has_id = !args.has(&["last"]);
            (after.len() > usize::from(has_id))
                .then(|| after.last().cloned())
                .flatten()
        }
        ("exec" | "e", Some("review")) => tail.get(1).cloned(),
        ("exec" | "e", _) | ("review", _) => tail.first().cloned(),
        _ => return None,
    };
    let configured = args
        .opts
        .iter()
        .rev()
        .filter(|(n, _)| n == "c" || n == "config")
        .filter_map(|(_, v)| v.as_deref())
        .find_map(|v| v.strip_prefix("model="))
        .map(|m| m.trim_matches(['"', '\'']).to_owned());
    Some(AgentCommand {
        driver: Driver::Codex,
        via: Via::Direct,
        task: shown(task),
        model: shown(args.value(&["m", "model"]).or(configured)),
    })
}

/// `agy` subcommands: none of these runs a prompt.
const AGY_SUBCOMMANDS: &[&str] = &[
    "agent",
    "agents",
    "changelog",
    "help",
    "install",
    "mcp",
    "mic-serve",
    "models",
    "plugin",
    "plugins",
    "remote-control",
    "update",
];

fn agy(rest: &[String]) -> Option<AgentCommand> {
    let args = scan(
        rest,
        &[
            "add-dir",
            "agent",
            "conversation",
            "effort",
            "input-format",
            "json-schema",
            "log-file",
            "mode",
            "model",
            "output-format",
            "print-timeout",
            "project",
            "p",
            "print",
            "prompt",
            "prompt-interactive",
            "i",
        ],
        true,
    );
    let subcommand = args
        .positionals
        .first()
        .is_some_and(|p| AGY_SUBCOMMANDS.contains(&p.as_str()));
    // `-p` / `--print` / `--prompt` run one prompt and print the reply; without them agy opens
    // its interactive session.
    if !args.has(&["p", "print", "prompt"]) || asks_for_help(&args) || subcommand {
        return None;
    }
    Some(AgentCommand {
        driver: Driver::Agy,
        via: Via::Direct,
        task: shown(args.value(&["p", "print", "prompt"])),
        model: shown(args.value(&["model"])),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn launch(command: &str) -> AgentCommand {
        parse(command).unwrap_or_else(|| panic!("no launch in {command:?}"))
    }

    fn none(command: &str) {
        assert_eq!(parse(command), None, "{command:?}");
    }

    fn want(command: &str, driver: Driver, via: Via, task: Option<&str>, model: Option<&str>) {
        let got = launch(command);
        assert_eq!(got.driver, driver, "{command:?}");
        assert_eq!(got.via, via, "{command:?}");
        assert_eq!(got.task.as_deref(), task, "{command:?}");
        assert_eq!(got.model.as_deref(), model, "{command:?}");
    }

    #[test]
    fn cross_agent_each_driver() {
        want(
            "cross-agent claude \"review this\"",
            Driver::Claude,
            Via::CrossAgent,
            Some("review this"),
            None,
        );
        want(
            "cross-agent agy 'fix the build'",
            Driver::Agy,
            Via::CrossAgent,
            Some("fix the build"),
            None,
        );
        want(
            "cross-agent codex review",
            Driver::Codex,
            Via::CrossAgent,
            Some("review"),
            None,
        );
    }

    #[test]
    fn cross_agent_options_and_model() {
        want(
            "cross-agent claude -a rust-reviewer -m opus -e high -t 600 \"check\" --json",
            Driver::Claude,
            Via::CrossAgent,
            Some("check"),
            Some("opus"),
        );
        want(
            "cross-agent claude \"check\" --model=sonnet --timeout 30",
            Driver::Claude,
            Via::CrossAgent,
            Some("check"),
            Some("sonnet"),
        );
        want(
            "cross-agent codex -mgpt-5 --effort low \"look\"",
            Driver::Codex,
            Via::CrossAgent,
            Some("look"),
            Some("gpt-5"),
        );
        want(
            "cross-agent agy --task-id t1 --review-id r1 -- \"-weird prompt\"",
            Driver::Agy,
            Via::CrossAgent,
            Some("-weird prompt"),
            None,
        );
    }

    #[test]
    fn cross_agent_prompt_file_has_no_task() {
        want(
            "cross-agent claude --prompt-file /tmp/brief.md -m opus",
            Driver::Claude,
            Via::CrossAgent,
            None,
            Some("opus"),
        );
        want(
            "cross-agent codex --prompt-file - < brief.md",
            Driver::Codex,
            Via::CrossAgent,
            None,
            None,
        );
    }

    #[test]
    fn cross_agent_non_launches() {
        none("cross-agent status --json");
        none("cross-agent select --tags code");
        none("cross-agent help");
        none("cross-agent --help");
        none("cross-agent claude --help");
        none("cross-agent claude -h");
        none("cross-agent claude");
        none("cross-agent");
        none("cross-agent frobnicate \"x\"");
    }

    #[test]
    fn claude_print_modes() {
        want(
            "claude -p \"summarise\"",
            Driver::Claude,
            Via::Direct,
            Some("summarise"),
            None,
        );
        want(
            "claude --print --model opus \"summarise\"",
            Driver::Claude,
            Via::Direct,
            Some("summarise"),
            Some("opus"),
        );
        want(
            "claude \"summarise\" -p --output-format json --model=haiku",
            Driver::Claude,
            Via::Direct,
            Some("summarise"),
            Some("haiku"),
        );
        want(
            "claude -p --allowedTools Bash --permission-mode plan \"go\"",
            Driver::Claude,
            Via::Direct,
            Some("go"),
            None,
        );
        // The prompt arrives on stdin.
        want("claude -p", Driver::Claude, Via::Direct, None, None);
        want(
            "git diff | claude -p --model sonnet",
            Driver::Claude,
            Via::Direct,
            None,
            Some("sonnet"),
        );
        want(
            "/usr/local/bin/claude -p hi",
            Driver::Claude,
            Via::Direct,
            Some("hi"),
            None,
        );
    }

    #[test]
    fn claude_non_launches() {
        none("claude");
        none("claude \"explain this repo\"");
        none("claude --version");
        none("claude -v");
        none("claude --help");
        none("claude -p --help");
        none("claude mcp list");
        none("claude mcp add -p 8080 x");
        none("claude config set theme dark");
        none("claude doctor");
        none("claude update");
        none("claude --model opus");
        none("claude --resume abc");
    }

    #[test]
    fn codex_exec_forms() {
        want(
            "codex exec \"fix it\"",
            Driver::Codex,
            Via::Direct,
            Some("fix it"),
            None,
        );
        want(
            "codex e -m gpt-5 --json \"fix it\"",
            Driver::Codex,
            Via::Direct,
            Some("fix it"),
            Some("gpt-5"),
        );
        want(
            "codex -m o3 exec -s read-only -C /repo \"look\"",
            Driver::Codex,
            Via::Direct,
            Some("look"),
            Some("o3"),
        );
        want(
            "codex exec --model=gpt-5 --skip-git-repo-check -o out.md 'x'",
            Driver::Codex,
            Via::Direct,
            Some("x"),
            Some("gpt-5"),
        );
        want(
            "codex exec -c model=\"o4\" \"x\"",
            Driver::Codex,
            Via::Direct,
            Some("x"),
            Some("o4"),
        );
        want(
            "echo hi | codex exec -",
            Driver::Codex,
            Via::Direct,
            None,
            None,
        );
        want("codex exec", Driver::Codex, Via::Direct, None, None);
    }

    #[test]
    fn codex_resume_fork_review() {
        want(
            "codex exec resume --last \"go on\"",
            Driver::Codex,
            Via::Direct,
            Some("go on"),
            None,
        );
        want(
            "codex exec resume abc123 \"go on\"",
            Driver::Codex,
            Via::Direct,
            Some("go on"),
            None,
        );
        want(
            "codex exec resume abc123",
            Driver::Codex,
            Via::Direct,
            None,
            None,
        );
        want(
            "codex exec review --uncommitted",
            Driver::Codex,
            Via::Direct,
            None,
            None,
        );
        want(
            "codex review --uncommitted",
            Driver::Codex,
            Via::Direct,
            None,
            None,
        );
    }

    #[test]
    fn codex_non_launches() {
        none("codex");
        none("codex --version");
        none("codex -V");
        none("codex login");
        none("codex logout");
        none("codex mcp list");
        none("codex exec --help");
        none("codex \"explain this\"");
        none("codex -p myprofile");
        none("codex resume --last");
    }

    #[test]
    fn agy_headless_forms() {
        want(
            "agy -p \"do the thing\"",
            Driver::Agy,
            Via::Direct,
            Some("do the thing"),
            None,
        );
        want(
            "agy --print \"x\" --output-format json --model flash",
            Driver::Agy,
            Via::Direct,
            Some("x"),
            Some("flash"),
        );
        want(
            "agy --model pro -p \"x\"",
            Driver::Agy,
            Via::Direct,
            Some("x"),
            Some("pro"),
        );
        want(
            "agy -model=pro --prompt=\"x y\"",
            Driver::Agy,
            Via::Direct,
            Some("x y"),
            Some("pro"),
        );
        want(
            "/usr/bin/agy -p /usage --output-format json",
            Driver::Agy,
            Via::Direct,
            Some("/usage"),
            None,
        );
        want("agy -p", Driver::Agy, Via::Direct, None, None);
    }

    #[test]
    fn agy_non_launches() {
        none("agy");
        none("agy --version");
        none("agy --help");
        none("agy models");
        none("agy mcp list");
        none("agy update");
        none("agy --continue");
        none("agy -i \"hello\"");
        none("agy --model pro");
    }

    #[test]
    fn env_prefixes_and_wrappers() {
        let task = Some("x");
        for command in [
            "FOO=1 BAR=two claude -p x",
            "env FOO=1 claude -p x",
            "env -u HOME -i FOO=1 claude -p x",
            "sudo claude -p x",
            "sudo -u alice -H claude -p x",
            "sudo -E env A=b claude -p x",
            "timeout 600 claude -p x",
            "timeout -s KILL -k 5 600 claude -p x",
            "timeout --signal=TERM 10 claude -p x",
            "nohup claude -p x",
            "nohup nice -n 5 claude -p x",
            "time claude -p x",
            "exec claude -p x",
            "command claude -p x",
            "setsid -f claude -p x",
            "stdbuf -oL claude -p x",
            "stdbuf -o L claude -p x",
            "/usr/bin/env claude -p x",
            "~/bin/claude -p x",
            "$HOME/.local/bin/claude -p x",
            "{ claude -p x; }",
            "( claude -p x )",
            "if true; then claude -p x; fi",
            "! claude -p x",
        ] {
            want(command, Driver::Claude, Via::Direct, task, None);
        }
    }

    #[test]
    fn chains_pipes_and_substitutions() {
        for command in [
            "cd /repo && claude -p x",
            "false || claude -p x",
            "echo start; claude -p x; echo done",
            "echo start\nclaude -p x\necho done",
            "cat brief.md | claude -p x",
            "claude -p x | tee out.log",
            "claude -p x > out.log 2>&1",
            "claude -p x 2>/dev/null &",
            "claude -p x &> out.log",
            "echo \"$(claude -p x)\"",
            "out=$(claude -p x)",
            "echo `claude -p x`",
            "cd /repo \\\n && claude -p x",
            "# note\nclaude -p x",
        ] {
            want(command, Driver::Claude, Via::Direct, Some("x"), None);
        }
    }

    #[test]
    fn the_first_launch_wins() {
        want(
            "cross-agent codex \"a\" && claude -p b",
            Driver::Codex,
            Via::CrossAgent,
            Some("a"),
            None,
        );
        want(
            "claude -p b ; cross-agent codex \"a\"",
            Driver::Claude,
            Via::Direct,
            Some("b"),
            None,
        );
    }

    #[test]
    fn shell_wrappers_are_looked_through() {
        let task = Some("x y");
        for command in [
            "bash -c 'claude -p \"x y\"'",
            "bash -lc 'claude -p \"x y\"'",
            "/usr/bin/zsh -lc \"cd /repo && claude -p 'x y'\"",
            "sh -c 'timeout 60 claude -p \"x y\"'",
            "sudo bash -c 'claude -p \"x y\"'",
            "bash -c \"bash -c 'claude -p \\\"x y\\\"'\"",
            "env FOO=1 bash -ec 'claude -p \"x y\"'",
        ] {
            want(command, Driver::Claude, Via::Direct, task, None);
        }
        none("bash -c 'echo hi'");
        none("bash script.sh claude -p");
        none("bash -c");
        none("bash");
    }

    #[test]
    fn nesting_is_bounded() {
        let mut command = "claude -p x".to_owned();
        for _ in 0..=MAX_DEPTH {
            command = format!("bash -c '{}'", command.replace('\'', "'\\''"));
        }
        none(&command);
    }

    #[test]
    fn mentions_are_not_launches() {
        none("echo \"claude -p\"");
        none("echo 'cross-agent claude \"x\"'");
        none("printf '%s\\n' \"codex exec x\"");
        none("grep codex exec file");
        none("grep -r 'agy -p' .");
        none("cat cross-agent.log");
        none("tail -f ~/.local/state/claude.log");
        none("which claude");
        none("type -a codex");
        none("ls /usr/bin/agy");
        none("pgrep -f 'claude -p'");
        none("man claude");
        none("vim claude.md");
        none("git commit -m \"run claude -p in CI\"");
        none("cp claude codex");
        none("echo claude -p # claude -p");
        none("cat <<'EOF'\nclaude -p x\nEOF");
        none("cat <<EOF > notes.md\ncross-agent claude \"x\"\nEOF\nls");
        none("FOO=\"claude -p x\" true");
        none("");
        none("   ");
    }

    #[test]
    fn a_heredoc_does_not_hide_a_later_launch() {
        want(
            "cat <<'EOF' > brief.md\nignored claude -p\nEOF\nclaude -p x",
            Driver::Claude,
            Via::Direct,
            Some("x"),
            None,
        );
    }

    #[test]
    fn quoting_and_escapes() {
        want(
            "claude -p \"say \\\"hi\\\" now\"",
            Driver::Claude,
            Via::Direct,
            Some("say \"hi\" now"),
            None,
        );
        want(
            "claude -p 'it'\\''s fine'",
            Driver::Claude,
            Via::Direct,
            Some("it's fine"),
            None,
        );
        want(
            "claude -p $'two\\nlines'",
            Driver::Claude,
            Via::Direct,
            Some("two\nlines"),
            None,
        );
        want(
            "claude -p \"multi\nline\" --model opus",
            Driver::Claude,
            Via::Direct,
            Some("multi\nline"),
            Some("opus"),
        );
        want(
            "cl\"aude\" -p x",
            Driver::Claude,
            Via::Direct,
            Some("x"),
            None,
        );
        // An unterminated quote still reads what is there.
        want(
            "claude -p \"never closed",
            Driver::Claude,
            Via::Direct,
            Some("never closed"),
            None,
        );
        // A prompt that is itself a command line stays one word.
        want(
            "cross-agent claude \"run claude -p inside; and && more\"",
            Driver::Claude,
            Via::CrossAgent,
            Some("run claude -p inside; and && more"),
            None,
        );
    }

    #[test]
    fn unreadable_text_is_not_shown() {
        let got = launch("claude -p \"a\u{202e}b\"");
        assert_eq!(got.task, None);
        let got = launch("claude -p x --model \"o\u{200b}pus\"");
        assert_eq!(got.model, None);
        assert_eq!(got.task.as_deref(), Some("x"));
    }

    #[test]
    fn tool_inputs_of_the_three_drivers() {
        let claude = from_tool_input(
            "Bash",
            &json!({"command": "claude -p hi", "description": "d"}),
        );
        assert_eq!(claude.map(|c| c.driver), Some(Driver::Claude));

        let codex = from_tool_input(
            "command_execution",
            &json!({"command": "/usr/bin/zsh -lc 'cross-agent codex \"look\" -m gpt-5'", "cwd": "/w"}),
        );
        assert_eq!(
            codex,
            Some(AgentCommand {
                driver: Driver::Codex,
                via: Via::CrossAgent,
                task: Some("look".to_owned()),
                model: Some("gpt-5".to_owned()),
            })
        );

        let agy = from_tool_input(
            "run_command",
            &json!({"CommandLine": "agy -p \"x\" --model pro", "Cwd": "/w", "WaitMsBeforeAsync": 10000}),
        );
        assert_eq!(
            agy,
            Some(AgentCommand {
                driver: Driver::Agy,
                via: Via::Direct,
                task: Some("x".to_owned()),
                model: Some("pro".to_owned()),
            })
        );
    }

    #[test]
    fn tool_input_argv_arrays() {
        let launch = from_tool_input(
            "command_execution",
            &json!({"command": ["bash", "-lc", "cd /w && codex exec 'go'"]}),
        );
        assert_eq!(launch.map(|c| c.driver), Some(Driver::Codex));
        let direct = from_tool_input(
            "command_execution",
            &json!({"command": ["claude", "-p", "a && b"]}),
        );
        assert_eq!(direct.and_then(|c| c.task), Some("a && b".to_owned()));
        assert_eq!(
            from_tool_input("command_execution", &json!({"command": ["ls", "-la"]})),
            None
        );
        assert_eq!(
            from_tool_input("command_execution", &json!({"command": ["claude", 3]})),
            None
        );
        assert_eq!(
            from_tool_input("command_execution", &json!({"command": []})),
            None
        );
    }

    #[test]
    fn tool_input_other_tools_and_shapes() {
        let input = json!({"command": "claude -p hi"});
        for tool in [
            "Read",
            "Edit",
            "send_command_input",
            "start_subagent",
            "Task",
            "",
        ] {
            assert_eq!(from_tool_input(tool, &input), None, "{tool}");
        }
        assert_eq!(from_tool_input("Bash", &json!({})), None);
        assert_eq!(from_tool_input("Bash", &json!({"command": ""})), None);
        assert_eq!(from_tool_input("Bash", &json!({"command": 4})), None);
        assert_eq!(from_tool_input("Bash", &json!(null)), None);
        assert_eq!(from_tool_input("Bash", &json!({"command": "ls"})), None);
        // The first non-empty key is the command, as the approval rules read it.
        let both = json!({"CommandLine": "  ", "cmd": "codex exec x"});
        assert_eq!(
            from_tool_input("run_command", &both).map(|c| c.driver),
            Some(Driver::Codex)
        );
    }

    #[test]
    fn parse_argv_reads_a_split_command() {
        let argv: Vec<String> = ["env", "A=1", "cross-agent", "agy", "do it"]
            .map(str::to_owned)
            .to_vec();
        let got = parse_argv(&argv).expect("a launch");
        assert_eq!((got.driver, got.via), (Driver::Agy, Via::CrossAgent));
        assert_eq!(got.task.as_deref(), Some("do it"));
        assert_eq!(parse_argv(&[]), None);
    }
}
