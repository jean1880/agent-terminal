//! Typeahead logic for the composer (pure; no GTK).
//!
//! The popover itself is a plain `gtk4::Popover` (see `composer.rs`); everything it decides is
//! here so it can be tested without a display: key handling, applying a completion to the
//! buffer text, recognising a typed built-in command line, and discarding stale `@file` replies.

use std::ops::Range;

use agent_core::adapter::{Driver, Mode};
use agent_core::caps::Capabilities;
use agent_core::commands::{resolve_alias, Builtin};

/// Keys the typeahead cares about; anything else is `Other`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Key {
    Up,
    Down,
    Tab,
    Enter,
    Escape,
    Other,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KeyOutcome {
    /// Not for the popup: the composer handles the key as usual.
    NotHandled,
    /// The selection moved; the key is consumed.
    Moved(usize),
    /// Accept this row; the key is consumed.
    Accept(usize),
    /// The popup closed; the key is consumed.
    Dismiss,
}

/// Selection state of the popup list.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Popup {
    len: usize,
    selected: usize,
    open: bool,
    /// Esc closes the popup until the trigger text changes.
    dismissed_for: Option<String>,
}

impl Popup {
    pub fn is_open(&self) -> bool {
        self.open && self.len > 0
    }

    #[cfg_attr(not(test), allow(dead_code))] // exercised by tests; kept as API
    pub fn selected(&self) -> usize {
        self.selected
    }

    /// New candidate list for `token` (the trigger text, sigil included). Selection resets.
    /// Returns whether the popup should be shown.
    pub fn set_items(&mut self, len: usize, token: &str) -> bool {
        self.len = len;
        self.selected = 0;
        if self.dismissed_for.as_deref() != Some(token) {
            self.dismissed_for = None;
        }
        self.open = len > 0 && self.dismissed_for.is_none();
        self.open
    }

    pub fn close(&mut self) {
        self.open = false;
    }

    /// Mouse selection.
    pub fn select(&mut self, index: usize) {
        if index < self.len {
            self.selected = index;
        }
    }

    pub fn key(&mut self, key: Key, token: &str) -> KeyOutcome {
        if !self.is_open() {
            return KeyOutcome::NotHandled;
        }
        match key {
            Key::Up => {
                self.selected = (self.selected + self.len - 1) % self.len;
                KeyOutcome::Moved(self.selected)
            }
            Key::Down => {
                self.selected = (self.selected + 1) % self.len;
                KeyOutcome::Moved(self.selected)
            }
            Key::Tab | Key::Enter => KeyOutcome::Accept(self.selected),
            Key::Escape => {
                self.open = false;
                self.dismissed_for = Some(token.to_owned());
                KeyOutcome::Dismiss
            }
            Key::Other => KeyOutcome::NotHandled,
        }
    }
}

/// Replaces `range` (byte offsets) of `text` with `insert`. Returns the new text and the cursor
/// byte offset just after the insertion. An invalid range leaves the text alone.
pub fn apply_completion(text: &str, range: Range<usize>, insert: &str) -> (String, usize) {
    if range.start > range.end
        || range.end > text.len()
        || !text.is_char_boundary(range.start)
        || !text.is_char_boundary(range.end)
    {
        return (text.to_owned(), text.len());
    }
    let mut out = String::with_capacity(text.len() + insert.len());
    out.push_str(&text[..range.start]);
    out.push_str(insert);
    let cursor = out.len();
    out.push_str(&text[range.end..]);
    (out, cursor)
}

/// A whole prompt that is a built-in command (`/model`, `/mode plan`), run locally on send.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommandLine {
    pub builtin: &'static Builtin,
    pub args: String,
}

/// Recognises a prompt that is a built-in command line. Agent commands are not matched: those
/// are sent as text.
pub fn parse_command_line(text: &str, caps: &Capabilities) -> Option<CommandLine> {
    let text = text.trim();
    let rest = text.strip_prefix('/')?;
    let (name, args) = match rest.split_once(char::is_whitespace) {
        Some((n, a)) => (n, a.trim()),
        None => (rest, ""),
    };
    if name.is_empty() {
        return None;
    }
    let builtin = resolve_alias(name, caps)?;
    Some(CommandLine {
        builtin,
        args: args.to_owned(),
    })
}

/// `/mode` argument → mode. Accepts the Claude names too.
pub fn parse_mode(arg: &str) -> Option<Mode> {
    match arg.trim().to_lowercase().replace(['_', ' '], "-").as_str() {
        "plan" => Some(Mode::Plan),
        "default" | "ask" => Some(Mode::Ask),
        "accept-edits" | "acceptedits" | "edits" | "edit" => Some(Mode::AcceptEdits),
        _ => None,
    }
}

/// `/handoff` argument → agent.
pub fn parse_driver(arg: &str) -> Option<Driver> {
    match arg.trim().to_lowercase().as_str() {
        "claude" => Some(Driver::Claude),
        "agy" | "antigravity" | "gemini" => Some(Driver::Agy),
        _ => None,
    }
}

/// Whether a chosen built-in runs at once (`/model`) or first needs an argument typed after it
/// (`/mode plan`, `/handoff agy`).
pub fn runs_immediately(builtin: &Builtin) -> bool {
    builtin.hint.is_none()
}

/// Tracks the newest `@file` request so replies to superseded queries are dropped.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct LatestRequest {
    pending: Option<String>,
}

impl LatestRequest {
    /// A new request supersedes every earlier one.
    pub fn issue(&mut self, id: String) {
        self.pending = Some(id);
    }

    /// Whether a reply with `id` is the one we are waiting for; consumes it if so.
    pub fn accept(&mut self, id: &str) -> bool {
        if self.pending.as_deref() == Some(id) {
            self.pending = None;
            true
        } else {
            false
        }
    }

    /// The query changed or the popup closed: any in-flight reply is stale.
    pub fn cancel(&mut self) {
        self.pending = None;
    }

    pub fn is_waiting_for(&self, id: &str) -> bool {
        self.pending.as_deref() == Some(id)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use agent_core::commands::BuiltinAction;

    #[test]
    fn closed_popup_handles_nothing() {
        let mut p = Popup::default();
        assert_eq!(p.key(Key::Down, "/"), KeyOutcome::NotHandled);
        assert!(!p.set_items(0, "/"));
        assert_eq!(p.key(Key::Enter, "/"), KeyOutcome::NotHandled);
    }

    #[test]
    fn up_down_wrap_and_enter_tab_accept() {
        let mut p = Popup::default();
        assert!(p.set_items(3, "/m"));
        assert_eq!(p.key(Key::Down, "/m"), KeyOutcome::Moved(1));
        assert_eq!(p.key(Key::Down, "/m"), KeyOutcome::Moved(2));
        assert_eq!(p.key(Key::Down, "/m"), KeyOutcome::Moved(0));
        assert_eq!(p.key(Key::Up, "/m"), KeyOutcome::Moved(2));
        assert_eq!(p.key(Key::Tab, "/m"), KeyOutcome::Accept(2));
        assert_eq!(p.key(Key::Enter, "/m"), KeyOutcome::Accept(2));
        assert_eq!(p.key(Key::Other, "/m"), KeyOutcome::NotHandled);
    }

    #[test]
    fn new_items_reset_the_selection() {
        let mut p = Popup::default();
        p.set_items(3, "/");
        p.key(Key::Down, "/");
        p.set_items(2, "/m");
        assert_eq!(p.selected(), 0);
    }

    #[test]
    fn escape_dismisses_until_the_token_changes() {
        let mut p = Popup::default();
        p.set_items(3, "/m");
        assert_eq!(p.key(Key::Escape, "/m"), KeyOutcome::Dismiss);
        assert!(!p.is_open());
        // Same token (e.g. the cursor moved): stays closed, and Enter goes to the composer.
        assert!(!p.set_items(3, "/m"));
        assert_eq!(p.key(Key::Enter, "/m"), KeyOutcome::NotHandled);
        // Typing reopens it.
        assert!(p.set_items(1, "/mo"));
    }

    #[test]
    fn mouse_select_is_bounded() {
        let mut p = Popup::default();
        p.set_items(2, "/");
        p.select(5);
        assert_eq!(p.selected(), 0);
        p.select(1);
        assert_eq!(p.key(Key::Enter, "/"), KeyOutcome::Accept(1));
    }

    #[test]
    fn completion_replaces_the_range() {
        assert_eq!(
            apply_completion("see @src/ma now", 4..11, "@src/main.rs "),
            ("see @src/main.rs  now".to_owned(), 17)
        );
        assert_eq!(
            apply_completion("/mo", 0..3, "/model "),
            ("/model ".to_owned(), 7)
        );
        // Bad ranges leave the text unchanged.
        assert_eq!(apply_completion("é", 1..2, "x"), ("é".to_owned(), 2));
        assert_eq!(apply_completion("ab", 1..9, "x"), ("ab".to_owned(), 2));
    }

    #[test]
    fn command_lines() {
        let caps = Capabilities::claude();
        let cl = parse_command_line("  /mode plan ", &caps).expect("mode");
        assert_eq!(cl.builtin.action, BuiltinAction::SetMode);
        assert_eq!(cl.args, "plan");
        let cl = parse_command_line("/compress", &caps).expect("alias");
        assert_eq!(cl.builtin.action, BuiltinAction::Compact);
        assert!(
            parse_command_line("/review 12", &caps).is_none(),
            "agent command"
        );
        assert!(parse_command_line("hello /model", &caps).is_none());
        assert!(parse_command_line("/", &caps).is_none());
        // Hidden for this agent: sent as text instead.
        assert!(parse_command_line("/mcp", &Capabilities::agy()).is_none());
    }

    #[test]
    fn mode_and_driver_arguments() {
        assert_eq!(parse_mode("plan"), Some(Mode::Plan));
        assert_eq!(parse_mode("Accept-Edits"), Some(Mode::AcceptEdits));
        assert_eq!(parse_mode("acceptEdits"), Some(Mode::AcceptEdits));
        assert_eq!(parse_mode("default"), Some(Mode::Ask));
        assert_eq!(parse_mode("yolo"), None);
        assert_eq!(parse_driver("Antigravity"), Some(Driver::Agy));
        assert_eq!(parse_driver("claude"), Some(Driver::Claude));
        assert_eq!(parse_driver("codex"), None);
    }

    #[test]
    fn builtins_with_arguments_wait_for_them() {
        let caps = Capabilities::claude();
        let get = |n: &str| resolve_alias(n, &caps).expect("builtin");
        assert!(runs_immediately(get("model")));
        assert!(!runs_immediately(get("mode")));
        assert!(!runs_immediately(get("handoff")));
    }

    #[test]
    fn stale_file_replies_are_dropped() {
        let mut l = LatestRequest::default();
        l.issue("a".into());
        l.issue("b".into());
        assert!(!l.accept("a"), "superseded");
        assert!(l.is_waiting_for("b"));
        assert!(l.accept("b"));
        assert!(!l.accept("b"), "consumed");
        l.issue("c".into());
        l.cancel();
        assert!(!l.accept("c"));
    }
}
