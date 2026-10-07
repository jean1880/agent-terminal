//! Importing an agent's own saved conversation as canonical [`Envelope`]s, so a resumed or listed
//! session shows its history in the chat view. Pure: lines and rows in, envelopes out.
//!
//! # Claude transcripts
//!
//! `~/.claude/projects/<dir>/<session>.jsonl` holds one JSON record per line. `user` and
//! `assistant` records carry the same `message` objects as stream-json output, so the assistant
//! side (text, thinking, `tool_use`, `tool_result`) goes through [`ClaudeAdapter::feed`] and the
//! view gets exactly the event kinds a live turn produces. The adapter does not make user prompts
//! (the live session does), so this module does.
//!
//! How each record type is mapped:
//!
//! | Record | Result |
//! |---|---|
//! | `user`, plain text | closes the previous turn, opens a new one: `TurnStarted`, then a `UserMessage` item |
//! | `user`, `<command-name>` | a `UserMessage` reading `/name args` (the slash command the user typed) |
//! | `user`, `<local-command-stdout>` / `-stderr` | a `Notice` with the tags stripped |
//! | `user`, `[Request interrupted…` | a `Notice`; the turn it ends is `Interrupted` |
//! | `user`, `isCompactSummary` | a `Notice` saying the conversation was compacted (the summary is not shown) |
//! | `user`, `tool_result` blocks | the adapter completes the matching tool item, except a backgrounded sub-agent's "launched" result (`toolUseResult.isAsync`), which leaves it open across turns |
//! | `user`, `origin.kind: "task-notification"` | completes that task's tool item with its `<result>` (else `<summary>`); no user message, no new turn |
//! | `user`, `isMeta` or only `<local-command-caveat>` / system reminders | skipped |
//! | `assistant` | the adapter: `AssistantMessage`, `Reasoning`, `Command`, `FileChange`, … items |
//! | any record with `isSidechain: true` (subagent traffic) | skipped |
//! | `attachment`, `system`, `summary`, `ai-title`, `last-prompt`, `queue-operation`, … | skipped |
//!
//! Current Claude builds keep a sub-agent's own steps in `<session>/subagents/agent-<id>.jsonl`,
//! which this importer does not read, so an imported sub-agent card has its result but no steps.
//! A backgrounded sub-agent whose notification never arrives is settled as `Interrupted` at the
//! end of the import.
//!
//! Inside text, `<system-reminder>…</system-reminder>` blocks are removed. A tool call that never
//! got a result (the session was interrupted or the transcript ends mid-call) is settled as
//! `Interrupted` when its turn ends. Tool output is cut at [`MAX_TOOL_OUTPUT_CHARS`] and the
//! native frame (`raw`) is dropped, so a 40 MB transcript cannot flood the store.
//!
//! Item ids come from the transcript (`<message id>:<block>`, the tool-use id, `user:<uuid>`), so
//! importing the same lines twice gives identical ids.
//!
//! # Limits
//!
//! At most [`MAX_IMPORTED_ITEMS`] items survive (the newest); a leading `Notice` says how many
//! were left out. The importer trims as it goes, so memory stays bounded for a huge transcript.
//! Ceiling: the adapter's dedup window ([`ClaudeAdapter`]'s `SEEN_CAP`) is 20 000 frames, so a
//! transcript that repeats frames further apart than that would show them twice.

use std::collections::HashSet;

use serde_json::Value;

use crate::adapter::Adapter;
use crate::claude::{is_async_launch, task_status, ClaudeAdapter};
use crate::event::{Envelope, Event, ItemKind, ItemStatus, StreamKind, TurnState};

/// The newest this many items are kept from one imported conversation.
pub const MAX_IMPORTED_ITEMS: usize = 1500;

/// Longest tool output (in characters) kept on an imported item.
pub const MAX_TOOL_OUTPUT_CHARS: usize = 20_000;

/// The notice that ends an imported agy conversation.
pub const AGY_REPLIES_NOTICE: &str = "Antigravity keeps its replies in its own format; only your prompts are shown here. agy still has the full conversation when you continue.";

/// Imports a Claude Code transcript held in memory. See the module notes; for a large file feed
/// lines to a [`ClaudeImporter`] instead.
pub fn claude_transcript<'a>(lines: impl IntoIterator<Item = &'a str>) -> Vec<Envelope> {
    let mut importer = ClaudeImporter::new();
    for line in lines {
        importer.push_line(line);
    }
    importer.finish()
}

/// Renders agy's prompt history (oldest first) as one turn per prompt, then one `Notice` that the
/// replies are not shown. Empty rows are skipped; no prompts at all gives no envelopes.
pub fn agy_prompts(rows: &[(String, Option<i64>)]) -> Vec<Envelope> {
    let mut out = Vec::new();
    for (i, (text, _at)) in rows.iter().enumerate() {
        let text = text.trim();
        if text.is_empty() {
            continue;
        }
        out.push(Envelope::new(Event::TurnStarted { model: None }));
        push_user_message(&mut out, format!("agy-user:{i}"), text);
        out.push(turn_completed(TurnState::Completed));
    }
    if out.is_empty() {
        return out;
    }
    let dropped = cap(&mut out, MAX_IMPORTED_ITEMS);
    finish_cap(&mut out, dropped);
    out.push(Envelope::new(Event::Notice {
        text: AGY_REPLIES_NOTICE.to_owned(),
    }));
    out
}

/// Streaming form of [`claude_transcript`]: push lines one at a time, then [`finish`](Self::finish).
pub struct ClaudeImporter {
    adapter: ClaudeAdapter,
    out: Vec<Envelope>,
    /// Item starts currently in `out`.
    items: usize,
    /// Item starts trimmed away so far.
    dropped: usize,
    /// Every item id started, so a stray completion for an unknown item is dropped.
    known: HashSet<String>,
    /// Tool items waiting for a result, in start order.
    open_tools: Vec<String>,
    /// Backgrounded sub-agents (their tool result was the "launched" metadata): they stay open
    /// across turns until their `<task-notification>`.
    background: HashSet<String>,
    turn_open: bool,
    turn_interrupted: bool,
    line_no: usize,
}

impl Default for ClaudeImporter {
    fn default() -> Self {
        Self::new()
    }
}

impl ClaudeImporter {
    pub fn new() -> Self {
        Self {
            adapter: ClaudeAdapter::new(),
            out: Vec::new(),
            items: 0,
            dropped: 0,
            known: HashSet::new(),
            open_tools: Vec::new(),
            background: HashSet::new(),
            turn_open: false,
            turn_interrupted: false,
            line_no: 0,
        }
    }

    /// Feeds one transcript line. Blank, malformed and irrelevant lines are ignored.
    pub fn push_line(&mut self, line: &str) {
        self.line_no += 1;
        let line = line.trim();
        if line.is_empty() {
            return;
        }
        let Ok(record) = serde_json::from_str::<Value>(line) else {
            return;
        };
        if flag(&record, "isSidechain") {
            return;
        }
        match record.get("type").and_then(Value::as_str) {
            Some("assistant") => {
                if flag(&record, "isMeta") {
                    return;
                }
                self.ensure_turn();
                let envs = self.adapter.feed(line);
                self.absorb(envs);
            }
            Some("user") => self.user(&record, line),
            _ => {}
        }
        if self.items > 2 * MAX_IMPORTED_ITEMS {
            self.dropped += cap(&mut self.out, MAX_IMPORTED_ITEMS);
            self.items = count_items(&self.out);
        }
    }

    /// Settles the last turn and returns the capped envelopes.
    pub fn finish(mut self) -> Vec<Envelope> {
        if self.turn_open {
            self.close_turn();
        }
        // A background sub-agent whose notification never came is not running any more.
        self.settle_open_tools();
        let dropped = self.dropped + cap(&mut self.out, MAX_IMPORTED_ITEMS);
        finish_cap(&mut self.out, dropped);
        self.out
    }

    fn user(&mut self, record: &Value, line: &str) {
        let content = record
            .get("message")
            .and_then(|m| m.get("content"))
            .unwrap_or(&Value::Null);

        if flag(record, "isCompactSummary") {
            self.out.push(Envelope::new(Event::Notice {
                text: "The earlier conversation was compacted here.".to_owned(),
            }));
            return;
        }
        if flag(record, "isMeta") {
            return;
        }
        // Claude tells itself a background task finished with a `<task-notification>` prompt:
        // it completes that task's item, and is neither a user message nor a new turn.
        if record
            .get("origin")
            .and_then(|o| o.get("kind"))
            .and_then(Value::as_str)
            == Some("task-notification")
        {
            self.task_notification(&user_text(content));
            return;
        }
        // Tool results go to the adapter, which completes the matching item.
        if let Value::Array(blocks) = content {
            let results: Vec<&str> = blocks
                .iter()
                .filter(|b| b.get("type").and_then(Value::as_str) == Some("tool_result"))
                .filter_map(|b| b.get("tool_use_id").and_then(Value::as_str))
                .collect();
            if !results.is_empty() {
                // A backgrounded sub-agent's "launched" result: the adapter leaves it open.
                if let [id] = results.as_slice() {
                    if is_async_launch(record) {
                        self.background.insert((*id).to_owned());
                    }
                }
                self.ensure_turn();
                let envs = self.adapter.feed(line);
                self.absorb(envs);
                return;
            }
        }

        let text = clean_user_text(&user_text(content));
        if text.is_empty() {
            return;
        }
        if let Some(inner) = between(&text, "<local-command-stdout>", "</local-command-stdout>")
            .or_else(|| between(&text, "<local-command-stderr>", "</local-command-stderr>"))
        {
            let inner = inner.trim();
            if !inner.is_empty() {
                self.out.push(Envelope::new(Event::Notice {
                    text: inner.to_owned(),
                }));
            }
            return;
        }
        if text.starts_with("<local-command-caveat>") {
            return;
        }
        if text.starts_with("[Request interrupted") {
            self.turn_interrupted = true;
            self.out.push(Envelope::new(Event::Notice { text }));
            return;
        }

        let text = slash_command(&text).unwrap_or(text);
        let id = match record.get("uuid").and_then(Value::as_str) {
            Some(uuid) => format!("user:{uuid}"),
            None => format!("user:l{}", self.line_no),
        };
        if self.turn_open {
            self.close_turn();
        }
        self.open_turn();
        self.known.insert(id.clone());
        push_user_message(&mut self.out, id, &text);
        self.items += 1;
    }

    fn ensure_turn(&mut self) {
        if !self.turn_open {
            self.open_turn();
        }
    }

    fn open_turn(&mut self) {
        self.turn_open = true;
        self.turn_interrupted = false;
        self.out
            .push(Envelope::new(Event::TurnStarted { model: None }));
    }

    /// Settles tool calls that never got a result, then ends the turn. Background sub-agents
    /// outlive their turn.
    fn close_turn(&mut self) {
        let (keep, settle): (Vec<String>, Vec<String>) = std::mem::take(&mut self.open_tools)
            .into_iter()
            .partition(|id| self.background.contains(id));
        self.open_tools = settle;
        self.settle_open_tools();
        self.open_tools = keep;
        let state = if self.turn_interrupted {
            TurnState::Interrupted
        } else {
            TurnState::Completed
        };
        self.out.push(turn_completed(state));
        self.turn_open = false;
        self.turn_interrupted = false;
    }

    /// Settles every open tool item as `Interrupted`.
    fn settle_open_tools(&mut self) {
        for id in std::mem::take(&mut self.open_tools) {
            self.background.remove(&id);
            self.out.push(
                Envelope::new(Event::ItemCompleted {
                    status: ItemStatus::Interrupted,
                    output: None,
                    error: None,
                })
                .item(id),
            );
        }
    }

    /// `<task-notification>…<tool-use-id>T</tool-use-id>…<status>S</status>…</task-notification>`:
    /// completes item `T` with the task's `<result>` (its final report), else its `<summary>`.
    /// A notification for an item this import never saw is dropped.
    fn task_notification(&mut self, text: &str) {
        let Some(id) = between(text, "<tool-use-id>", "</tool-use-id>").map(str::trim) else {
            return;
        };
        if !self.known.contains(id) {
            return;
        }
        let id = id.to_owned();
        let status = task_status(
            between(text, "<status>", "</status>")
                .map(str::trim)
                .unwrap_or(""),
        );
        // The report is free text and the last element: its end is the LAST `</result>`.
        let result = text.find("<result>").and_then(|start| {
            let start = start + "<result>".len();
            let end = text.rfind("</result>").filter(|e| *e >= start)?;
            Some(&text[start..end])
        });
        let mut output = result
            .or_else(|| between(text, "<summary>", "</summary>"))
            .map(|s| s.trim().to_owned())
            .filter(|s| !s.is_empty());
        if let Some(o) = output.as_mut() {
            truncate_chars(o, MAX_TOOL_OUTPUT_CHARS);
        }
        self.open_tools.retain(|t| *t != id);
        self.background.remove(&id);
        self.out.push(
            Envelope::new(Event::ItemCompleted {
                status,
                error: (status == ItemStatus::Failed).then(|| {
                    output
                        .clone()
                        .unwrap_or_else(|| "the sub-agent failed".to_owned())
                }),
                output,
            })
            .item(id),
        );
    }

    /// Keeps what the view renders from the adapter's output, without the native frames.
    fn absorb(&mut self, envs: Vec<Envelope>) {
        for mut env in envs {
            env.raw = None;
            match &mut env.event {
                Event::ItemStarted { kind, .. } => {
                    let Some(id) = env.item.clone() else {
                        continue;
                    };
                    if !self.known.insert(id.clone()) {
                        continue; // a repeated start (a resumed transcript replays frames)
                    }
                    if !matches!(
                        kind,
                        ItemKind::AssistantMessage | ItemKind::Reasoning | ItemKind::UserMessage
                    ) {
                        self.open_tools.push(id);
                    }
                    self.items += 1;
                }
                Event::ContentSnapshot { .. } | Event::ContentDelta { .. } => {
                    if !env.item.as_ref().is_some_and(|i| self.known.contains(i)) {
                        continue;
                    }
                }
                Event::ItemCompleted { output, error, .. } => {
                    let Some(id) = env.item.clone() else {
                        continue;
                    };
                    if !self.known.contains(&id) {
                        continue;
                    }
                    self.open_tools.retain(|t| *t != id);
                    for field in [output, error].into_iter().flatten() {
                        truncate_chars(field, MAX_TOOL_OUTPUT_CHARS);
                    }
                }
                Event::Notice { .. } => {}
                // Session, quota, command and control events are not conversation.
                _ => continue,
            }
            self.out.push(env);
        }
    }
}

fn flag(record: &Value, key: &str) -> bool {
    record.get(key).and_then(Value::as_bool) == Some(true)
}

fn turn_completed(state: TurnState) -> Envelope {
    Envelope::new(Event::TurnCompleted {
        state,
        usage: None,
        cost_usd: None,
        error: None,
    })
}

/// A prompt as the live session stores it: `ItemStarted{UserMessage}` then a snapshot.
fn push_user_message(out: &mut Vec<Envelope>, id: String, text: &str) {
    out.push(
        Envelope::new(Event::ItemStarted {
            kind: ItemKind::UserMessage,
            title: String::new(),
            input: None,
            parent: None,
        })
        .item(id.clone()),
    );
    out.push(
        Envelope::new(Event::ContentSnapshot {
            stream: StreamKind::Assistant,
            text: text.to_owned(),
        })
        .item(id),
    );
}

fn is_item_start(e: &Envelope) -> bool {
    matches!(e.event, Event::ItemStarted { .. })
}

fn count_items(out: &[Envelope]) -> usize {
    out.iter().filter(|e| is_item_start(e)).count()
}

/// Drops the oldest envelopes so at most `keep` item starts remain, preferring to cut between
/// turns. Returns how many items were dropped. Later events of a dropped item are removed too.
fn cap(out: &mut Vec<Envelope>, keep: usize) -> usize {
    let total = count_items(out);
    if total <= keep {
        return 0;
    }
    let skip = total - keep;
    let Some(first_kept) = out
        .iter()
        .enumerate()
        .filter(|(_, e)| is_item_start(e))
        .nth(skip)
        .map(|(i, _)| i)
    else {
        return 0;
    };
    let is_turn_start = |e: &Envelope| matches!(e.event, Event::TurnStarted { .. });
    let before = out[..=first_kept].iter().rposition(is_turn_start);
    let cut = match before {
        // The first kept item opens its turn: keep the turn start with it.
        Some(t) if !out[t + 1..first_kept].iter().any(is_item_start) => t,
        _ => out[first_kept + 1..]
            .iter()
            .position(is_turn_start)
            .map_or(first_kept, |p| first_kept + 1 + p),
    };
    let dropped_ids: HashSet<String> = out[..cut]
        .iter()
        .filter(|e| is_item_start(e))
        .filter_map(|e| e.item.clone())
        .collect();
    let dropped = dropped_ids.len();
    out.drain(..cut);
    out.retain(|e| !e.item.as_ref().is_some_and(|i| dropped_ids.contains(i)));
    dropped
}

/// After capping: a leading notice for what was left out, and a turn start if the cut landed
/// inside a turn (the turn's own `TurnCompleted` is still there).
fn finish_cap(out: &mut Vec<Envelope>, dropped: usize) {
    if dropped == 0 {
        return;
    }
    if !out
        .first()
        .is_some_and(|e| matches!(e.event, Event::TurnStarted { .. }))
    {
        out.insert(0, Envelope::new(Event::TurnStarted { model: None }));
    }
    let noun = if dropped == 1 { "item" } else { "items" };
    out.insert(
        0,
        Envelope::new(Event::Notice {
            text: format!("{dropped} earlier {noun} were left out of this imported conversation."),
        }),
    );
}

/// Cuts `s` to at most `max` characters, marking the cut.
fn truncate_chars(s: &mut String, max: usize) {
    if let Some((at, _)) = s.char_indices().nth(max) {
        s.truncate(at);
        s.push_str("\n… (output truncated)");
    }
}

/// Text of a user message `content`: a string, or the text of each block (an image is marked).
fn user_text(content: &Value) -> String {
    match content {
        Value::String(s) => s.clone(),
        Value::Array(blocks) => blocks
            .iter()
            .filter_map(|b| match b.get("type").and_then(Value::as_str) {
                Some("text") => b.get("text").and_then(Value::as_str).map(str::to_owned),
                Some("image") => Some("[image]".to_owned()),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join("\n"),
        _ => String::new(),
    }
}

/// Removes `<system-reminder>` blocks and trims.
fn clean_user_text(text: &str) -> String {
    const OPEN: &str = "<system-reminder>";
    const CLOSE: &str = "</system-reminder>";
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(start) = rest.find(OPEN) {
        out.push_str(&rest[..start]);
        match rest[start..].find(CLOSE) {
            Some(end) => rest = &rest[start + end + CLOSE.len()..],
            None => {
                rest = "";
                break;
            }
        }
    }
    out.push_str(rest);
    out.trim().to_owned()
}

fn between<'a>(text: &'a str, open: &str, close: &str) -> Option<&'a str> {
    let start = text.find(open)? + open.len();
    let end = text[start..].find(close)? + start;
    Some(&text[start..end])
}

/// `<command-name>/x</command-name>…<command-args>y</command-args>` as the typed `/x y`.
fn slash_command(text: &str) -> Option<String> {
    let name = between(text, "<command-name>", "</command-name>")?.trim();
    if name.is_empty() {
        return None;
    }
    let args = between(text, "<command-args>", "</command-args>")
        .map(str::trim)
        .unwrap_or("");
    Some(if args.is_empty() {
        name.to_owned()
    } else {
        format!("{name} {args}")
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    const TRANSCRIPT: &str = include_str!("../tests/fixtures/claude-transcript-synthetic.jsonl");

    fn import() -> Vec<Envelope> {
        claude_transcript(TRANSCRIPT.lines())
    }

    fn user_texts(out: &[Envelope]) -> Vec<String> {
        let users: Vec<&str> = out
            .iter()
            .filter(|e| {
                matches!(
                    e.event,
                    Event::ItemStarted {
                        kind: ItemKind::UserMessage,
                        ..
                    }
                )
            })
            .filter_map(|e| e.item.as_deref())
            .collect();
        users
            .iter()
            .filter_map(|id| {
                out.iter().find_map(|e| match &e.event {
                    Event::ContentSnapshot { text, .. } if e.item.as_deref() == Some(*id) => {
                        Some(text.clone())
                    }
                    _ => None,
                })
            })
            .collect()
    }

    fn started<'a>(out: &'a [Envelope], id: &str) -> Option<&'a Event> {
        out.iter()
            .find(|e| e.item.as_deref() == Some(id) && is_item_start(e))
            .map(|e| &e.event)
    }

    fn completed<'a>(out: &'a [Envelope], id: &str) -> Option<&'a Event> {
        out.iter()
            .find(|e| {
                e.item.as_deref() == Some(id) && matches!(e.event, Event::ItemCompleted { .. })
            })
            .map(|e| &e.event)
    }

    #[test]
    fn prompts_become_user_messages_in_order() {
        assert_eq!(
            user_texts(&import()),
            [
                "Fix the greeting in /repo/hello.txt",
                "/cost",
                "Thanks",
                "Next"
            ]
        );
    }

    #[test]
    fn each_prompt_is_one_balanced_turn() {
        let out = import();
        let starts = out
            .iter()
            .filter(|e| matches!(e.event, Event::TurnStarted { .. }))
            .count();
        let ends = out
            .iter()
            .filter(|e| matches!(e.event, Event::TurnCompleted { .. }))
            .count();
        assert_eq!((starts, ends), (4, 4));
        assert!(matches!(
            out.first().map(|e| &e.event),
            Some(Event::TurnStarted { .. })
        ));
        assert!(matches!(
            out.last().map(|e| &e.event),
            Some(Event::TurnCompleted { .. })
        ));
    }

    #[test]
    fn assistant_text_and_thinking_use_the_live_item_kinds() {
        let out = import();
        assert!(matches!(
            started(&out, "msg_1:0"),
            Some(Event::ItemStarted {
                kind: ItemKind::Reasoning,
                ..
            })
        ));
        assert!(matches!(
            started(&out, "msg_1:1"),
            Some(Event::ItemStarted {
                kind: ItemKind::AssistantMessage,
                ..
            })
        ));
        assert!(out.iter().any(|e| e.item.as_deref() == Some("msg_1:1")
            && matches!(&e.event, Event::ContentSnapshot { text, .. } if text == "I will look first.")));
    }

    #[test]
    fn bash_and_edit_tools_pair_with_their_results() {
        let out = import();
        assert!(matches!(
            started(&out, "toolu_bash1"),
            Some(Event::ItemStarted {
                kind: ItemKind::Command,
                title,
                input: Some(_),
                ..
            }) if title == "Bash"
        ));
        assert!(matches!(
            completed(&out, "toolu_bash1"),
            Some(Event::ItemCompleted {
                status: ItemStatus::Completed,
                output: Some(o),
                ..
            }) if o == "hello world"
        ));
        assert!(matches!(
            started(&out, "toolu_edit1"),
            Some(Event::ItemStarted {
                kind: ItemKind::FileChange,
                ..
            })
        ));
        assert!(matches!(
            completed(&out, "toolu_edit1"),
            Some(Event::ItemCompleted {
                status: ItemStatus::Failed,
                error: Some(_),
                ..
            })
        ));
    }

    #[test]
    fn a_call_with_no_result_is_settled_as_interrupted() {
        let out = import();
        assert!(matches!(
            completed(&out, "toolu_bash2"),
            Some(Event::ItemCompleted {
                status: ItemStatus::Interrupted,
                ..
            })
        ));
    }

    #[test]
    fn meta_sidechain_reminders_and_other_record_types_are_skipped() {
        let out = import();
        let all = serde_json::to_string(&out).expect("serialize");
        assert!(!all.contains("subagent prompt"));
        assert!(!all.contains("Caveat"));
        assert!(!all.contains("noise"));
        assert!(!all.contains("skill body"));
        assert!(out.iter().all(|e| e.raw.is_none()));
        assert!(out.iter().all(|e| e.event != Event::Unknown));
    }

    #[test]
    fn local_command_output_and_interrupts_are_notices() {
        let out = import();
        let notices: Vec<&str> = out
            .iter()
            .filter_map(|e| match &e.event {
                Event::Notice { text } => Some(text.as_str()),
                _ => None,
            })
            .collect();
        assert!(notices.contains(&"Total cost: $0.00"));
        assert!(notices
            .iter()
            .any(|n| n.starts_with("[Request interrupted")));
        assert!(notices.iter().any(|n| n.contains("compacted")));
        // The interrupted turn ends as Interrupted.
        assert!(out.iter().any(|e| matches!(
            e.event,
            Event::TurnCompleted {
                state: TurnState::Interrupted,
                ..
            }
        )));
    }

    #[test]
    fn a_second_import_yields_identical_ids() {
        let ids = |out: &[Envelope]| -> Vec<Option<String>> {
            out.iter().map(|e| e.item.clone()).collect()
        };
        assert_eq!(ids(&import()), ids(&import()));
        let out = import();
        let mut seen = HashSet::new();
        for e in out.iter().filter(|e| is_item_start(e)) {
            assert!(seen.insert(e.item.clone()), "duplicate id {:?}", e.item);
        }
    }

    #[test]
    fn long_tool_output_is_truncated() {
        let big = "x".repeat(MAX_TOOL_OUTPUT_CHARS + 50);
        let lines = [
            r#"{"type":"user","uuid":"u1","message":{"role":"user","content":"go"}}"#.to_owned(),
            r#"{"type":"assistant","uuid":"a1","message":{"id":"m1","role":"assistant","content":[{"type":"tool_use","id":"t1","name":"Bash","input":{"command":"ls"}}]}}"#.to_owned(),
            format!(
                r#"{{"type":"user","uuid":"u2","message":{{"role":"user","content":[{{"type":"tool_result","tool_use_id":"t1","content":"{big}"}}]}}}}"#
            ),
        ];
        let out = claude_transcript(lines.iter().map(String::as_str));
        let Some(Event::ItemCompleted {
            output: Some(o), ..
        }) = completed(&out, "t1")
        else {
            panic!("no completion");
        };
        assert!(o.ends_with("(output truncated)"));
        assert!(o.chars().count() < MAX_TOOL_OUTPUT_CHARS + 40);
    }

    /// A background `Agent` call as Claude 2.1.29x records it (ids shortened): the call, the
    /// immediate "launched" result, a reply, a later prompt, then the task-notification record.
    fn background_agent_transcript(notify: bool) -> Vec<String> {
        let mut lines = vec![
            r#"{"type":"user","uuid":"u1","message":{"role":"user","content":"review it"}}"#.to_owned(),
            r#"{"type":"assistant","uuid":"a1","message":{"id":"m1","role":"assistant","content":[{"type":"tool_use","id":"toolu_bg","name":"Agent","input":{"description":"Review fixes","prompt":"Review.","run_in_background":true}}]}}"#.to_owned(),
            r#"{"type":"user","uuid":"u2","message":{"role":"user","content":[{"tool_use_id":"toolu_bg","type":"tool_result","content":[{"type":"text","text":"Async agent launched successfully. (This tool result is internal metadata.)"}]}]},"toolUseResult":{"isAsync":true,"status":"async_launched","agentId":"ag1","description":"Review fixes"}}"#.to_owned(),
            r#"{"type":"assistant","uuid":"a2","message":{"id":"m2","role":"assistant","content":[{"type":"text","text":"Launched."}]}}"#.to_owned(),
            r#"{"type":"user","uuid":"u3","message":{"role":"user","content":"meanwhile, hello"}}"#.to_owned(),
        ];
        if notify {
            lines.push(r#"{"type":"user","uuid":"u4","origin":{"kind":"task-notification","producer":"session-task"},"message":{"role":"user","content":"<task-notification>\n<task-id>ag1</task-id>\n<tool-use-id>toolu_bg</tool-use-id>\n<output-file>/tmp/t/tasks/ag1.output</output-file>\n<status>completed</status>\n<summary>Agent \"Review fixes\" finished</summary>\n<note>A task-notification fires each time this agent stops.</note>\n<result>**Verdict: PASS**</result>\n</task-notification>"}}"#.to_owned());
            lines.push(r#"{"type":"assistant","uuid":"a3","message":{"id":"m3","role":"assistant","content":[{"type":"text","text":"It passed."}]}}"#.to_owned());
        }
        lines
    }

    #[test]
    fn a_task_notification_completes_its_background_agent_without_a_prompt() {
        let lines = background_agent_transcript(true);
        let out = claude_transcript(lines.iter().map(String::as_str));
        assert_eq!(user_texts(&out), ["review it", "meanwhile, hello"]);
        let all = serde_json::to_string(&out).expect("serialize");
        assert!(!all.contains("<task-notification>"));
        // One completion only: the launch result and the turn end left it open.
        let done: Vec<&Event> = out
            .iter()
            .filter(|e| {
                e.item.as_deref() == Some("toolu_bg")
                    && matches!(e.event, Event::ItemCompleted { .. })
            })
            .map(|e| &e.event)
            .collect();
        assert!(
            matches!(
                done.as_slice(),
                [Event::ItemCompleted { status: ItemStatus::Completed, output: Some(o), .. }]
                    if o == "**Verdict: PASS**"
            ),
            "{done:?}"
        );
        // No turn of its own: two prompts, two turns.
        let starts = out
            .iter()
            .filter(|e| matches!(e.event, Event::TurnStarted { .. }))
            .count();
        let ends = out
            .iter()
            .filter(|e| matches!(e.event, Event::TurnCompleted { .. }))
            .count();
        assert_eq!((starts, ends), (2, 2));
    }

    #[test]
    fn a_background_agent_with_no_notification_ends_interrupted() {
        let lines = background_agent_transcript(false);
        let out = claude_transcript(lines.iter().map(String::as_str));
        assert!(matches!(
            completed(&out, "toolu_bg"),
            Some(Event::ItemCompleted {
                status: ItemStatus::Interrupted,
                ..
            })
        ));
        assert!(!serde_json::to_string(&out)
            .expect("serialize")
            .contains("Async agent launched"));
    }

    fn many_prompts(n: usize) -> Vec<String> {
        (0..n)
            .map(|i| {
                format!(
                    r#"{{"type":"user","uuid":"u{i}","message":{{"role":"user","content":"prompt {i}"}}}}"#
                )
            })
            .collect()
    }

    #[test]
    fn a_huge_transcript_keeps_the_newest_items_and_says_so() {
        let n = MAX_IMPORTED_ITEMS * 3 + 7;
        let lines = many_prompts(n);
        let out = claude_transcript(lines.iter().map(String::as_str));
        assert_eq!(count_items(&out), MAX_IMPORTED_ITEMS);
        let Some(Event::Notice { text }) = out.first().map(|e| &e.event) else {
            panic!("no leading notice");
        };
        assert_eq!(
            text,
            &format!(
                "{} earlier items were left out of this imported conversation.",
                n - MAX_IMPORTED_ITEMS
            )
        );
        let texts = user_texts(&out);
        assert_eq!(
            texts.last().map(String::as_str),
            Some(&*format!("prompt {}", n - 1))
        );
        assert_eq!(
            texts.first().map(String::as_str),
            Some(&*format!("prompt {}", n - MAX_IMPORTED_ITEMS))
        );
        // Turns stay balanced after the cut.
        let starts = out
            .iter()
            .filter(|e| matches!(e.event, Event::TurnStarted { .. }))
            .count();
        let ends = out
            .iter()
            .filter(|e| matches!(e.event, Event::TurnCompleted { .. }))
            .count();
        assert_eq!(starts, ends);
    }

    #[test]
    fn a_cut_inside_one_giant_turn_still_balances() {
        let mut lines = vec![
            r#"{"type":"user","uuid":"u0","message":{"role":"user","content":"start"}}"#.to_owned(),
        ];
        for i in 0..(MAX_IMPORTED_ITEMS + 10) {
            lines.push(format!(
                r#"{{"type":"assistant","uuid":"a{i}","message":{{"id":"m{i}","role":"assistant","content":[{{"type":"text","text":"t{i}"}}]}}}}"#
            ));
        }
        let out = claude_transcript(lines.iter().map(String::as_str));
        assert_eq!(count_items(&out), MAX_IMPORTED_ITEMS);
        assert!(matches!(out[0].event, Event::Notice { .. }));
        assert!(matches!(out[1].event, Event::TurnStarted { .. }));
        assert!(matches!(
            out.last().map(|e| &e.event),
            Some(Event::TurnCompleted { .. })
        ));
    }

    #[test]
    fn agy_prompts_are_turns_followed_by_one_notice() {
        let rows = vec![
            ("first".to_owned(), Some(1)),
            ("   ".to_owned(), None),
            ("second".to_owned(), Some(2)),
        ];
        let out = agy_prompts(&rows);
        assert_eq!(user_texts(&out), ["first", "second"]);
        let notices: Vec<_> = out
            .iter()
            .filter(|e| matches!(e.event, Event::Notice { .. }))
            .collect();
        assert_eq!(notices.len(), 1);
        assert!(matches!(
            out.last().map(|e| &e.event),
            Some(Event::Notice { text }) if text == AGY_REPLIES_NOTICE
        ));
        assert!(agy_prompts(&[]).is_empty());
    }
}
