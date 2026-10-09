//! Slash-command registry and typeahead ranking for the composer.
//!
//! Pure logic, no UI: trigger detection, the built-in command registry, merging of built-ins with
//! the agent's own commands and skills, and ranking.
//!
//! The ranking is a port of T3 Code's `composerSlashCommandSearch.ts` and the scoring helpers in
//! `searchRanking.ts` (MIT, see `THIRD_PARTY.md`). Differences: indices count characters rather than
//! UTF-16 units, and built-in aliases are scored alongside the primary name.

use std::ops::Range;

use crate::caps::Capabilities;
use crate::event::{AgentCommand, AgentCommandKind};

// ---------------------------------------------------------------------------------------------
// Trigger detection
// ---------------------------------------------------------------------------------------------

/// What the user is completing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TriggerKind {
    /// `/command`, only at the start of the prompt.
    Slash,
    /// `@path`, anywhere at a word boundary.
    File,
    /// `$skill`, anywhere at a word boundary.
    Skill,
}

/// An active completion trigger under the cursor.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Trigger {
    pub kind: TriggerKind,
    /// Text after the sigil, up to the cursor.
    pub query: String,
    /// Byte range of the sigil plus query: what a completion replaces.
    pub range: Range<usize>,
    /// Only whitespace precedes the trigger token.
    pub at_prompt_start: bool,
}

/// Finds the trigger the cursor (a byte offset) sits in, if any.
///
/// The token is the run of non-whitespace characters ending at the cursor. `/` only triggers when
/// that token opens the prompt; `@` and `$` trigger wherever the token starts a word. A cursor that
/// is out of range or inside a multi-byte character yields `None`.
pub fn detect_trigger(text: &str, cursor: usize) -> Option<Trigger> {
    if cursor > text.len() || !text.is_char_boundary(cursor) {
        return None;
    }
    let before = &text[..cursor];
    let start = before
        .char_indices()
        .rev()
        .find(|(_, c)| c.is_whitespace())
        .map_or(0, |(i, c)| i + c.len_utf8());
    let token = &before[start..];
    let sigil = token.chars().next()?;
    let kind = match sigil {
        '/' => TriggerKind::Slash,
        '@' => TriggerKind::File,
        '$' => TriggerKind::Skill,
        _ => return None,
    };
    let at_prompt_start = text[..start].trim().is_empty();
    if kind == TriggerKind::Slash && !at_prompt_start {
        return None;
    }
    Some(Trigger {
        kind,
        query: token[sigil.len_utf8()..].to_owned(),
        range: start..cursor,
        at_prompt_start,
    })
}

// ---------------------------------------------------------------------------------------------
// Built-in registry
// ---------------------------------------------------------------------------------------------

/// What the UI does when a built-in is chosen. Built-ins are handled locally and never sent.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BuiltinAction {
    OpenModelPicker,
    OpenMcpPanel,
    OpenSettings,
    Compact,
    Handoff,
    Fork,
    /// `/mode plan|default|accept-edits`; the UI parses the argument.
    SetMode,
    OpenUsage,
    OpenContext,
    Rewind,
    NewThread,
    Help,
}

/// A command the app handles itself.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Builtin {
    pub name: &'static str,
    pub aliases: &'static [&'static str],
    pub description: &'static str,
    pub hint: Option<&'static str>,
    pub action: BuiltinAction,
}

impl Builtin {
    /// Whether `name` (lowercase, no slash) is this command's name or an alias.
    fn answers_to(&self, name: &str) -> bool {
        self.name == name || self.aliases.contains(&name)
    }

    /// Whether the agent behind `caps` can back this command. Commands that need a native panel
    /// the agent cannot answer are hidden rather than offered and failed.
    pub fn available(&self, caps: &Capabilities) -> bool {
        match self.action {
            BuiltinAction::OpenModelPicker => caps.model_list,
            BuiltinAction::OpenMcpPanel => caps.mcp_panel,
            BuiltinAction::OpenSettings => caps.settings_panel,
            BuiltinAction::OpenUsage => caps.usage,
            BuiltinAction::OpenContext => caps.context_usage,
            // Compact falls back to handoff-to-self, so it is always offered.
            BuiltinAction::Compact
            | BuiltinAction::Handoff
            | BuiltinAction::Fork
            | BuiltinAction::SetMode
            | BuiltinAction::Rewind
            | BuiltinAction::NewThread
            | BuiltinAction::Help => true,
        }
    }
}

static BUILTINS: [Builtin; 12] = [
    Builtin {
        name: "model",
        aliases: &[],
        description: "Choose the model and effort",
        hint: None,
        action: BuiltinAction::OpenModelPicker,
    },
    Builtin {
        name: "mcp",
        aliases: &[],
        description: "Manage MCP servers",
        hint: None,
        action: BuiltinAction::OpenMcpPanel,
    },
    Builtin {
        name: "config",
        aliases: &[],
        description: "View and edit settings",
        hint: None,
        action: BuiltinAction::OpenSettings,
    },
    Builtin {
        name: "compact",
        aliases: &["compress"],
        description: "Compact the conversation context",
        hint: None,
        action: BuiltinAction::Compact,
    },
    Builtin {
        name: "handoff",
        aliases: &[],
        description: "Continue this thread in another agent",
        hint: Some("<agent>"),
        action: BuiltinAction::Handoff,
    },
    Builtin {
        name: "fork",
        aliases: &[],
        description: "Branch the thread from this turn",
        hint: None,
        action: BuiltinAction::Fork,
    },
    Builtin {
        name: "mode",
        aliases: &[],
        description: "Set the interaction mode",
        hint: Some("plan|default|accept-edits"),
        action: BuiltinAction::SetMode,
    },
    Builtin {
        name: "usage",
        aliases: &[],
        description: "Show quota and usage",
        hint: None,
        action: BuiltinAction::OpenUsage,
    },
    Builtin {
        name: "context",
        aliases: &[],
        description: "Show the context window gauge",
        hint: None,
        action: BuiltinAction::OpenContext,
    },
    Builtin {
        name: "rewind",
        aliases: &[],
        description: "Undo the last turn's file changes",
        hint: None,
        action: BuiltinAction::Rewind,
    },
    Builtin {
        name: "clear",
        aliases: &["new"],
        description: "Start a new thread",
        hint: None,
        action: BuiltinAction::NewThread,
    },
    Builtin {
        name: "help",
        aliases: &[],
        description: "List commands and shortcuts",
        hint: None,
        action: BuiltinAction::Help,
    },
];

/// The built-in command registry.
pub fn builtins() -> &'static [Builtin] {
    &BUILTINS
}

/// Normalizes a typed command name: no leading slashes, lowercase.
fn normalize_name(name: &str) -> String {
    name.trim().trim_start_matches('/').to_lowercase()
}

/// Resolves a typed name or alias (`compress`, `/new`) to a built-in the agent can back.
pub fn resolve_alias(name: &str, caps: &Capabilities) -> Option<&'static Builtin> {
    let name = normalize_name(name);
    BUILTINS
        .iter()
        .find(|b| b.answers_to(&name))
        .filter(|b| b.available(caps))
}

/// The text to send the agent for a compaction (Claude `/compact`), or `None` when it has no such
/// command and the UI should fall back to handoff-to-self.
pub fn compact_text(caps: &Capabilities) -> Option<String> {
    let cmd = caps.compact_command.as_deref()?.trim();
    if cmd.is_empty() {
        return None;
    }
    Some(if cmd.starts_with('/') {
        cmd.to_owned()
    } else {
        format!("/{cmd}")
    })
}

// ---------------------------------------------------------------------------------------------
// Completion items
// ---------------------------------------------------------------------------------------------

/// Source of a completion. The derived order is the ranking tie-break: built-in, agent, skill.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum CompletionKind {
    Builtin,
    Agent,
    Skill,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CompletionItem {
    pub kind: CompletionKind,
    /// Bare name, without the sigil.
    pub name: String,
    pub description: String,
    pub hint: Option<String>,
    /// Text that replaces the trigger range, sigil included, with a trailing space.
    pub insert_text: String,
}

/// Builds the unranked candidate list for `trigger`, then ranks it by the trigger's query.
///
/// - `TerminalOnly` and `SideCommand` agent commands are never shown.
/// - A built-in shadows an agent command of the same name or alias: one entry, the built-in.
/// - Agent commands appear only when the trigger is at the start of the prompt.
/// - Built-ins the agent cannot back are hidden.
/// - `$` offers skills; `@` offers nothing here (files come from the file provider).
pub fn completion_items(
    builtins: &[Builtin],
    agent: &[AgentCommand],
    driver_caps: &Capabilities,
    trigger: &Trigger,
) -> Vec<CompletionItem> {
    let mut items = Vec::new();
    match trigger.kind {
        TriggerKind::File => return items,
        TriggerKind::Slash => {
            let shown: Vec<&Builtin> = builtins
                .iter()
                .filter(|b| b.available(driver_caps))
                .collect();
            items.extend(shown.iter().map(|b| builtin_item(b)));
            // Shadowing uses every built-in, so a hidden /mcp still masks the agent's /mcp: it
            // would open a TUI that is not running.
            if trigger.at_prompt_start {
                for cmd in agent {
                    let name = normalize_name(&cmd.name);
                    if cmd.kind != AgentCommandKind::Command
                        || name.is_empty()
                        || builtins.iter().any(|b| b.answers_to(&name))
                        || items
                            .iter()
                            .any(|i| i.kind == CompletionKind::Agent && i.name == name)
                    {
                        continue;
                    }
                    items.push(agent_item(CompletionKind::Agent, name, cmd, '/'));
                }
            }
        }
        TriggerKind::Skill => {
            for cmd in agent.iter().filter(|c| c.kind == AgentCommandKind::Skill) {
                let name = cmd.name.trim().trim_start_matches('$').to_owned();
                if name.is_empty()
                    || items
                        .iter()
                        .any(|i: &CompletionItem| i.name.eq_ignore_ascii_case(&name))
                {
                    continue;
                }
                items.push(agent_item(CompletionKind::Skill, name, cmd, '$'));
            }
        }
    }
    rank(items, &trigger.query, builtins)
}

fn builtin_item(b: &Builtin) -> CompletionItem {
    CompletionItem {
        kind: CompletionKind::Builtin,
        name: b.name.to_owned(),
        description: b.description.to_owned(),
        hint: b.hint.map(str::to_owned),
        insert_text: format!("/{} ", b.name),
    }
}

fn agent_item(
    kind: CompletionKind,
    name: String,
    cmd: &AgentCommand,
    sigil: char,
) -> CompletionItem {
    CompletionItem {
        kind,
        description: cmd.description.clone().unwrap_or_default(),
        hint: cmd.argument_hint.clone().filter(|h| !h.is_empty()),
        insert_text: format!("{sigil}{name} "),
        name,
    }
}

// ---------------------------------------------------------------------------------------------
// Ranking (port of T3 Code searchRanking.ts / composerSlashCommandSearch.ts)
// ---------------------------------------------------------------------------------------------

const NAME_MARKERS: &[char] = &['-', '_', '/'];
const DESCRIPTION_MARKERS: &[char] = &[' ', '-', '_', '/'];

struct Bases {
    exact: u32,
    prefix: Option<u32>,
    boundary: Option<u32>,
    includes: Option<u32>,
    fuzzy: Option<u32>,
}

const NAME_BASES: Bases = Bases {
    exact: 0,
    prefix: Some(2),
    boundary: Some(4),
    includes: Some(6),
    fuzzy: Some(100),
};

const DESCRIPTION_BASES: Bases = Bases {
    exact: 20,
    prefix: Some(22),
    boundary: Some(24),
    includes: Some(26),
    fuzzy: None,
};

fn length_penalty(value_len: usize, query_len: usize) -> u32 {
    u32::try_from(value_len.saturating_sub(query_len).min(64)).unwrap_or(64)
}

/// Character index of `needle` in `haystack`, if present.
fn char_find(haystack: &str, needle: &str) -> Option<usize> {
    haystack
        .find(needle)
        .map(|byte| haystack[..byte].chars().count())
}

/// Lower is better. Both inputs must already be lowercase.
fn score_subsequence(value: &str, query: &str) -> Option<u32> {
    let value: Vec<char> = value.chars().collect();
    let query: Vec<char> = query.chars().collect();
    if query.is_empty() {
        return Some(0);
    }
    let (mut qi, mut first, mut prev) = (0usize, None::<usize>, None::<usize>);
    let mut gap = 0usize;
    for (vi, c) in value.iter().enumerate() {
        if *c != query[qi] {
            continue;
        }
        let first_idx = *first.get_or_insert(vi);
        if let Some(p) = prev {
            gap += vi - p - 1;
        }
        prev = Some(vi);
        qi += 1;
        if qi == query.len() {
            let span = vi - first_idx + 1 - query.len();
            let len_pen = value.len().saturating_sub(query.len()).min(64);
            let total = first_idx * 2 + gap * 3 + span + len_pen;
            return Some(u32::try_from(total).unwrap_or(u32::MAX));
        }
    }
    None
}

/// Tiered match score; `None` when the query does not match. Both inputs must be lowercase.
fn score_query_match(value: &str, query: &str, bases: &Bases, markers: &[char]) -> Option<u32> {
    if value.is_empty() || query.is_empty() {
        return None;
    }
    if value == query {
        return Some(bases.exact);
    }
    let (vlen, qlen) = (value.chars().count(), query.chars().count());
    let penalty = length_penalty(vlen, qlen);
    if value.starts_with(query) {
        if let Some(base) = bases.prefix {
            return Some(base + penalty);
        }
    }
    if let Some(base) = bases.boundary {
        let best = markers
            .iter()
            .filter_map(|m| {
                char_find(value, &format!("{m}{query}")).map(|i| i + 1) // marker is one char
            })
            .min();
        if let Some(index) = best {
            return Some(base + u32::try_from(index * 2).unwrap_or(u32::MAX) + penalty);
        }
    }
    if let (Some(base), Some(index)) = (bases.includes, char_find(value, query)) {
        return Some(base + u32::try_from(index * 2).unwrap_or(u32::MAX) + penalty);
    }
    if let Some(base) = bases.fuzzy {
        return score_subsequence(value, query).map(|s| base + s);
    }
    None
}

fn score_item(item: &CompletionItem, query: &str, builtins: &[Builtin]) -> Option<u32> {
    let name = item.name.to_lowercase();
    let mut best = score_query_match(&name, query, &NAME_BASES, NAME_MARKERS);
    let builtin = builtins.iter().find(|b| b.name == item.name);
    if let (CompletionKind::Builtin, Some(b)) = (item.kind, builtin) {
        for alias in b.aliases {
            best = best.min_or(score_query_match(alias, query, &NAME_BASES, NAME_MARKERS));
        }
    }
    best.min_or(score_query_match(
        &item.description.to_lowercase(),
        query,
        &DESCRIPTION_BASES,
        DESCRIPTION_MARKERS,
    ))
}

/// `Option<u32>` minimum where `None` means "no match" rather than "smallest".
trait MinOr {
    fn min_or(self, other: Self) -> Self;
}

impl MinOr for Option<u32> {
    fn min_or(self, other: Self) -> Self {
        match (self, other) {
            (Some(a), Some(b)) => Some(a.min(b)),
            (a, None) => a,
            (None, b) => b,
        }
    }
}

/// Ranks `items` against `query`. Leading slashes are stripped; an empty query keeps the order.
fn rank(items: Vec<CompletionItem>, query: &str, builtins: &[Builtin]) -> Vec<CompletionItem> {
    let query = query.trim().trim_start_matches('/').to_lowercase();
    if query.is_empty() {
        return items;
    }
    let mut scored: Vec<(u32, CompletionItem)> = items
        .into_iter()
        .filter_map(|item| score_item(&item, &query, builtins).map(|s| (s, item)))
        .collect();
    scored.sort_by(|(sa, a), (sb, b)| {
        sa.cmp(sb)
            .then_with(|| a.kind.cmp(&b.kind))
            .then_with(|| a.name.cmp(&b.name))
    });
    scored.into_iter().map(|(_, item)| item).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cmd(name: &str, kind: AgentCommandKind) -> AgentCommand {
        AgentCommand {
            name: name.to_owned(),
            description: None,
            argument_hint: None,
            kind,
        }
    }

    fn slash(query: &str) -> Trigger {
        Trigger {
            kind: TriggerKind::Slash,
            query: query.to_owned(),
            range: 0..query.len() + 1,
            at_prompt_start: true,
        }
    }

    fn names(items: &[CompletionItem]) -> Vec<&str> {
        items.iter().map(|i| i.name.as_str()).collect()
    }

    // --- triggers ---

    #[test]
    fn slash_at_prompt_start() {
        let t = detect_trigger("/mo", 3).expect("trigger");
        assert_eq!(t.kind, TriggerKind::Slash);
        assert_eq!(t.query, "mo");
        assert_eq!(t.range, 0..3);
        assert!(t.at_prompt_start);
    }

    #[test]
    fn slash_after_leading_whitespace_is_prompt_start() {
        let t = detect_trigger("  \n/mo", 6).expect("trigger");
        assert!(t.at_prompt_start);
        assert_eq!(t.range, 3..6);
    }

    #[test]
    fn slash_mid_prompt_is_not_a_trigger() {
        assert!(detect_trigger("hello /mo", 9).is_none());
        assert!(detect_trigger("path/to", 7).is_none());
    }

    #[test]
    fn at_and_dollar_trigger_anywhere_at_a_word_boundary() {
        let f = detect_trigger("see @src/ma", 11).expect("file");
        assert_eq!(f.kind, TriggerKind::File);
        assert_eq!(f.query, "src/ma");
        assert_eq!(f.range, 4..11);
        assert!(!f.at_prompt_start);
        let s = detect_trigger("use $rev", 8).expect("skill");
        assert_eq!(s.kind, TriggerKind::Skill);
        assert_eq!(s.query, "rev");
    }

    #[test]
    fn sigils_inside_a_word_do_not_trigger() {
        assert!(detect_trigger("mail a@b", 8).is_none());
        assert!(detect_trigger("cost US$5", 9).is_none());
    }

    #[test]
    fn cursor_uses_text_before_it_only() {
        let t = detect_trigger("/model rest", 3).expect("trigger");
        assert_eq!(t.query, "mo");
        assert_eq!(t.range, 0..3);
        // After a space the token is empty or plain text.
        assert!(detect_trigger("/model ", 7).is_none());
    }

    #[test]
    fn utf8_positions_are_safe() {
        let text = "héllo @café";
        let end = text.len();
        let t = detect_trigger(text, end).expect("trigger");
        assert_eq!(t.query, "café");
        assert_eq!(t.range, 7..end);
        // Cursor inside the two-byte 'é' (byte 2) and past the end.
        assert!(detect_trigger(text, 2).is_none());
        assert!(detect_trigger(text, end + 1).is_none());
        // Multibyte whitespace before the token.
        let t = detect_trigger("a\u{3000}$日本", "a\u{3000}$日本".len()).expect("trigger");
        assert_eq!(t.query, "日本");
        assert_eq!(t.range.start, 1 + '\u{3000}'.len_utf8());
        assert!(detect_trigger("/日本", 2).is_none());
    }

    #[test]
    fn empty_and_plain_text_yield_nothing() {
        assert!(detect_trigger("", 0).is_none());
        assert!(detect_trigger("hello", 5).is_none());
        let bare = detect_trigger("/", 1).expect("bare slash");
        assert_eq!(bare.query, "");
    }

    // --- registry ---

    #[test]
    fn registry_has_every_builtin() {
        let all: Vec<&str> = builtins().iter().map(|b| b.name).collect();
        assert_eq!(
            all,
            [
                "model", "mcp", "config", "compact", "handoff", "fork", "mode", "usage", "context",
                "rewind", "clear", "help"
            ]
        );
    }

    #[test]
    fn aliases_resolve() {
        let caps = Capabilities::claude();
        assert_eq!(
            resolve_alias("compress", &caps).map(|b| b.action),
            Some(BuiltinAction::Compact)
        );
        assert_eq!(
            resolve_alias("/NEW", &caps).map(|b| b.action),
            Some(BuiltinAction::NewThread)
        );
        assert_eq!(resolve_alias("clear", &caps).map(|b| b.name), Some("clear"));
        assert!(resolve_alias("nope", &caps).is_none());
    }

    #[test]
    fn alias_respects_capabilities() {
        assert!(resolve_alias("mcp", &Capabilities::claude()).is_some());
        assert!(resolve_alias("mcp", &Capabilities::agy()).is_none());
    }

    #[test]
    fn compact_text_follows_caps() {
        assert_eq!(
            compact_text(&Capabilities::claude()).as_deref(),
            Some("/compact")
        );
        assert_eq!(compact_text(&Capabilities::agy()), None);
        let mut caps = Capabilities::agy();
        caps.compact_command = Some("compress".to_owned());
        assert_eq!(compact_text(&caps).as_deref(), Some("/compress"));
    }

    // --- merging ---

    #[test]
    fn terminal_only_and_side_commands_never_shown() {
        let agent = [
            cmd("doctor", AgentCommandKind::TerminalOnly),
            cmd("side", AgentCommandKind::SideCommand),
            cmd("review", AgentCommandKind::Command),
        ];
        let items = completion_items(builtins(), &agent, &Capabilities::claude(), &slash(""));
        assert!(!names(&items).contains(&"doctor"));
        assert!(!names(&items).contains(&"side"));
        assert!(names(&items).contains(&"review"));
    }

    #[test]
    fn builtin_shadows_agent_command() {
        let agent = [
            cmd("model", AgentCommandKind::Command),
            cmd("compress", AgentCommandKind::Command),
        ];
        let items = completion_items(builtins(), &agent, &Capabilities::claude(), &slash(""));
        assert_eq!(names(&items).iter().filter(|n| **n == "model").count(), 1);
        assert!(!names(&items).contains(&"compress"));
        let model = items.iter().find(|i| i.name == "model").expect("model");
        assert_eq!(model.kind, CompletionKind::Builtin);
    }

    #[test]
    fn shadowing_holds_even_when_the_builtin_is_hidden() {
        let agent = [cmd("mcp", AgentCommandKind::Command)];
        let items = completion_items(builtins(), &agent, &Capabilities::agy(), &slash(""));
        assert!(!names(&items).contains(&"mcp"));
    }

    #[test]
    fn agent_commands_need_prompt_start() {
        let agent = [cmd("review", AgentCommandKind::Command)];
        let mut t = slash("");
        t.at_prompt_start = false;
        let items = completion_items(builtins(), &agent, &Capabilities::claude(), &t);
        assert!(!names(&items).contains(&"review"));
        assert!(names(&items).contains(&"model"));
    }

    #[test]
    fn builtins_without_capability_are_hidden() {
        let items = completion_items(builtins(), &[], &Capabilities::agy(), &slash(""));
        let n = names(&items);
        assert!(!n.contains(&"mcp"));
        assert!(n.contains(&"context"));
        assert!(n.contains(&"config"));
        assert!(n.contains(&"compact"));

        let mut no_context = Capabilities::agy();
        no_context.context_usage = false;
        let items = completion_items(builtins(), &[], &no_context, &slash(""));
        assert!(!names(&items).contains(&"context"));
    }

    #[test]
    fn dollar_offers_skills_only() {
        let agent = [
            cmd("review", AgentCommandKind::Command),
            cmd("pdf", AgentCommandKind::Skill),
            cmd("doctor", AgentCommandKind::TerminalOnly),
        ];
        let t = Trigger {
            kind: TriggerKind::Skill,
            query: String::new(),
            range: 0..1,
            at_prompt_start: false,
        };
        let items = completion_items(builtins(), &agent, &Capabilities::claude(), &t);
        assert_eq!(names(&items), ["pdf"]);
        assert_eq!(items[0].insert_text, "$pdf ");
        assert_eq!(items[0].kind, CompletionKind::Skill);
    }

    #[test]
    fn file_trigger_has_no_items_here() {
        let t = Trigger {
            kind: TriggerKind::File,
            query: String::new(),
            range: 0..1,
            at_prompt_start: true,
        };
        assert!(completion_items(builtins(), &[], &Capabilities::claude(), &t).is_empty());
    }

    #[test]
    fn insert_text_and_hint() {
        let mut review = cmd("/review", AgentCommandKind::Command);
        review.argument_hint = Some("<pr>".to_owned());
        let items = completion_items(builtins(), &[review], &Capabilities::claude(), &slash(""));
        let handoff = items.iter().find(|i| i.name == "handoff").expect("handoff");
        assert_eq!(handoff.insert_text, "/handoff ");
        assert_eq!(handoff.hint.as_deref(), Some("<agent>"));
        let r = items.iter().find(|i| i.name == "review").expect("review");
        assert_eq!(r.insert_text, "/review ");
        assert_eq!(r.hint.as_deref(), Some("<pr>"));
    }

    // --- ranking ---

    #[test]
    fn score_tiers_match_t3() {
        let s = |v: &str, q: &str| score_query_match(v, q, &NAME_BASES, NAME_MARKERS);
        assert_eq!(s("model", "model"), Some(0));
        assert_eq!(s("model", "mo"), Some(2 + 3));
        // Boundary: "-" at index 3, match at 4 -> 4 + 8 + penalty(8 - 3).
        assert_eq!(s("fix-bug", "bug"), Some(4 + 8 + 4));
        assert_eq!(s("remodel", "mod"), Some(6 + 4 + 4));
        assert_eq!(s("model", "mdl"), Some(100 + 6 + 2 + 2));
        assert_eq!(s("model", "xyz"), None);
        assert_eq!(s("", "a"), None);
        let d = |v: &str, q: &str| score_query_match(v, q, &DESCRIPTION_BASES, DESCRIPTION_MARKERS);
        assert_eq!(d("compact", "compact"), Some(20));
        assert_eq!(d("compact it", "comp"), Some(22 + 6));
        assert_eq!(d("show usage", "usage"), Some(24 + 10 + 5));
        assert_eq!(d("show usage", "mdl"), None);
    }

    #[test]
    fn exact_beats_prefix_beats_nearer_substring_matches() {
        let agent = [
            cmd("xmodel", AgentCommandKind::Command),
            cmd("my-model", AgentCommandKind::Command),
            cmd("modeling", AgentCommandKind::Command),
        ];
        let items = completion_items(builtins(), &agent, &Capabilities::claude(), &slash("model"));
        assert_eq!(names(&items), ["model", "modeling", "xmodel", "my-model"]);
    }

    #[test]
    fn leading_slashes_stripped_and_case_ignored() {
        let items = completion_items(builtins(), &[], &Capabilities::claude(), &slash("/MODEL"));
        assert_eq!(names(&items).first(), Some(&"model"));
    }

    #[test]
    fn ties_sort_by_kind_then_name() {
        let items = vec![
            CompletionItem {
                kind: CompletionKind::Skill,
                name: "aaa".into(),
                description: String::new(),
                hint: None,
                insert_text: String::new(),
            },
            CompletionItem {
                kind: CompletionKind::Agent,
                name: "aaa".into(),
                description: String::new(),
                hint: None,
                insert_text: String::new(),
            },
            CompletionItem {
                kind: CompletionKind::Builtin,
                name: "aaa".into(),
                description: String::new(),
                hint: None,
                insert_text: String::new(),
            },
            CompletionItem {
                kind: CompletionKind::Agent,
                name: "aab".into(),
                description: String::new(),
                hint: None,
                insert_text: String::new(),
            },
        ];
        // "aa" is a prefix of "aaa" (penalty 1) and of "aab" (penalty 1): all tie.
        let ranked = rank(items, "aa", &[]);
        let order: Vec<(CompletionKind, &str)> =
            ranked.iter().map(|i| (i.kind, i.name.as_str())).collect();
        assert_eq!(
            order,
            [
                (CompletionKind::Builtin, "aaa"),
                (CompletionKind::Agent, "aaa"),
                (CompletionKind::Agent, "aab"),
                (CompletionKind::Skill, "aaa"),
            ]
        );
    }

    #[test]
    fn description_matches_and_min_of_scores() {
        let mut review = cmd("review", AgentCommandKind::Command);
        review.description = Some("Check the pull request".to_owned());
        let items = completion_items(
            builtins(),
            &[review],
            &Capabilities::claude(),
            &slash("pull"),
        );
        assert_eq!(names(&items), ["review"]);
        // Name score beats the description score when both match.
        let items = completion_items(builtins(), &[], &Capabilities::claude(), &slash("compact"));
        assert_eq!(names(&items), ["compact"]);
    }

    #[test]
    fn builtin_alias_matches_the_query() {
        let items = completion_items(builtins(), &[], &Capabilities::claude(), &slash("compress"));
        assert_eq!(names(&items).first(), Some(&"compact"));
    }

    #[test]
    fn fuzzy_matches_rank_last_and_nonmatches_drop() {
        let items = completion_items(builtins(), &[], &Capabilities::claude(), &slash("mdl"));
        assert_eq!(names(&items), ["model"]);
        let items = completion_items(builtins(), &[], &Capabilities::claude(), &slash("zzzz"));
        assert!(items.is_empty());
    }

    #[test]
    fn empty_query_keeps_registry_order() {
        let items = completion_items(builtins(), &[], &Capabilities::claude(), &slash(""));
        assert_eq!(items.len(), builtins().len());
        assert_eq!(names(&items).first(), Some(&"model"));
    }

    #[test]
    fn unicode_names_rank_without_panicking() {
        let agent = [cmd("café-déjà", AgentCommandKind::Command)];
        let items = completion_items(builtins(), &agent, &Capabilities::claude(), &slash("déj"));
        assert_eq!(names(&items), ["café-déjà"]);
    }
}
