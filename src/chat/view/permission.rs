//! A Codex permission request as its card says it (pure: no GTK).
//!
//! The adapter hands the view a typed, display-ready input for an approval whose tool is
//! [`TOOL`]: the working folder, the entries asked for (a path, a glob or a named place, each
//! read, write or deny), network access, whether the grant may last the session, and a warning.
//! This module turns that into the words on the card, the shelf's one line and the "Extra access"
//! chip's list. It fails closed: anything it cannot read exactly makes the request
//! unrenderable, which the card shows raw and lets the user only refuse.

use serde_json::Value;

/// The approval tool name a Codex permission request carries.
pub const TOOL: &str = agent_core::codex::PERMISSIONS_TOOL;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Access {
    Read,
    Write,
    Deny,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Target {
    Path(String),
    Glob(String),
    /// A named place (`project`, `temp`, `slash_tmp`, `root`, `minimal`), with the folder it
    /// resolves to when the adapter knew it.
    Special {
        name: String,
        path: Option<String>,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Entry {
    pub access: Access,
    pub target: Target,
    /// Where the target really is, when it reaches there through a symlink.
    pub resolved: Option<String>,
}

impl Entry {
    /// How the place reads in a list: "<target> → <resolved>" when it resolves elsewhere.
    pub fn words(&self) -> String {
        match &self.resolved {
            Some(real) => format!("{} → {real}", self.target.words()),
            None => self.target.words(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Request {
    pub cwd: String,
    pub reason: Option<String>,
    pub entries: Vec<Entry>,
    pub network: Option<bool>,
    /// "Allow for this session" may be offered.
    pub session_allowed: bool,
    /// The adapter's own plain warning (a broad grant), if any.
    pub warning: Option<String>,
    /// The adapter's own one-line summary, preferred for the shelf.
    pub summary: Option<String>,
    /// The card can say all of it. False: show the raw request and offer only a refusal.
    pub renderable: bool,
}

/// Whether `s` is safe to print as a line: no control, bidi or zero-width characters.
pub(super) fn plain(s: &str) -> bool {
    !agent_core::text::has_control_or_bidi(s, &[])
}

/// How a Codex special place (`project`, `temp`, `slash_tmp`, `root`, `minimal`) reads, the same
/// on the card and in the policy's warnings; `None` for a name it does not know, which each
/// caller words in its own way.
pub(crate) fn special_words(name: &str) -> Option<&'static str> {
    Some(match name {
        "project" => "the project",
        "temp" => "the temp folder",
        "slash_tmp" => "/tmp",
        "root" => "the whole filesystem",
        "minimal" => "system libraries",
        _ => return None,
    })
}

impl Target {
    /// How the place reads in a list.
    pub fn words(&self) -> String {
        match self {
            Self::Path(p) => p.clone(),
            Self::Glob(g) => format!("files matching {g}"),
            Self::Special { name, path } => {
                let words = special_words(name).unwrap_or("an unknown place");
                match path {
                    Some(p) if !p.is_empty() => format!("{words} ({p})"),
                    _ => words.to_owned(),
                }
            }
        }
    }

    fn reaches_everything(&self) -> bool {
        match self {
            Self::Path(p) => p == "/",
            Self::Special { name, .. } => name == "root",
            Self::Glob(_) => false,
        }
    }
}

impl Request {
    /// Reads the adapter's input. A shape it does not know exactly gives an unrenderable
    /// request (never a partial summary of one).
    pub fn parse(input: &Value) -> Self {
        let mut request = Self {
            cwd: String::new(),
            reason: None,
            entries: Vec::new(),
            network: None,
            session_allowed: false,
            warning: None,
            summary: None,
            renderable: false,
        };
        let Some(object) = input.as_object() else {
            return request;
        };
        let text = |key: &str| {
            object
                .get(key)
                .and_then(Value::as_str)
                .filter(|s| !s.trim().is_empty())
                .map(str::to_owned)
        };
        request.cwd = text("cwd").unwrap_or_default();
        request.reason = text("reason");
        request.warning = text("warning");
        request.summary = text("summary");
        request.network = object.get("network").and_then(Value::as_bool);
        request.session_allowed = object
            .get("session_allowed")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        let mut exact = object.get("renderable").and_then(Value::as_bool) == Some(true);
        let entries = object.get("entries").and_then(Value::as_array);
        exact &= entries.is_some();
        for raw in entries.into_iter().flatten() {
            match parse_entry(raw) {
                Some(entry) => request.entries.push(entry),
                None => exact = false,
            }
        }
        // A request that asks for nothing at all is not one the user can judge.
        exact &= !request.entries.is_empty() || request.network == Some(true);
        exact &= [
            Some(&request.cwd),
            request.reason.as_ref(),
            request.warning.as_ref(),
            request.summary.as_ref(),
        ]
        .into_iter()
        .flatten()
        .all(|s| plain(s));
        request.renderable = exact;
        request
    }

    fn of(&self, access: Access) -> impl Iterator<Item = &Entry> {
        self.entries.iter().filter(move |e| e.access == access)
    }

    /// Places it would read or write (not the denied ones).
    fn granted(&self) -> impl Iterator<Item = &Entry> {
        self.entries.iter().filter(|e| e.access != Access::Deny)
    }

    pub fn asks_network(&self) -> bool {
        self.network == Some(true)
    }

    /// A grant that reaches the whole filesystem: Allow is no longer the suggested button.
    pub fn reaches_everything(&self) -> bool {
        self.granted().any(|e| e.target.reaches_everything())
    }

    /// Words that must be shown prominently, whoever wrote them: the adapter's warning, and the
    /// whole-filesystem notice this view adds itself.
    pub fn warnings(&self) -> Vec<String> {
        let mut warnings = Vec::new();
        if self.reaches_everything() {
            warnings.push(
                "This reaches the whole filesystem, not only this project. Allow only if you \
                 trust what Codex is doing."
                    .to_owned(),
            );
        }
        if let Some(w) = &self.warning {
            if !warnings.contains(w) {
                warnings.push(w.clone());
            }
        }
        warnings
    }

    /// Whether the plain "Allow for this turn" is the suggested action.
    pub fn allow_is_calm(&self) -> bool {
        self.renderable && self.warnings().is_empty()
    }

    /// The card's title when the event brought none: what is asked, in words.
    pub fn title(&self) -> String {
        let count = |access: Access| self.of(access).count();
        let places = |n: usize| match n {
            1 => "1 place".to_owned(),
            n => format!("{n} places"),
        };
        let mut parts = Vec::new();
        if count(Access::Write) > 0 {
            parts.push(format!("write to {}", places(count(Access::Write))));
        }
        if count(Access::Read) > 0 {
            parts.push(format!("read {}", places(count(Access::Read))));
        }
        if self.asks_network() {
            parts.push("use the network".to_owned());
        }
        match parts.len() {
            0 => "Codex asks for extra permissions".to_owned(),
            1 => format!("Codex asks to {}", parts[0]),
            _ => {
                let last = parts.pop().unwrap_or_default();
                format!("Codex asks to {} and {last}", parts.join(", "))
            }
        }
    }

    /// The grouped list the card shows, one place per line.
    pub fn lines(&self) -> Vec<String> {
        let mut lines = Vec::new();
        for (heading, access) in [
            ("Read", Access::Read),
            ("Write", Access::Write),
            ("Still blocked", Access::Deny),
        ] {
            let mut places = self.of(access).peekable();
            if places.peek().is_none() {
                continue;
            }
            lines.push(heading.to_owned());
            lines.extend(places.map(|e| format!("  {}", e.words())));
        }
        match self.network {
            Some(true) => lines.push("Use the network".to_owned()),
            Some(false) => lines.push("Network access stays off".to_owned()),
            None => {}
        }
        lines
    }

    /// The shelf's single line.
    pub fn one_line(&self) -> String {
        if let Some(summary) = &self.summary {
            return summary.clone();
        }
        let group = |label: &str, access: Access| {
            let words: Vec<String> = self.of(access).map(|e| e.words()).collect();
            (!words.is_empty()).then(|| format!("{label} {}", words.join(", ")))
        };
        let mut parts: Vec<String> = [group("Write", Access::Write), group("Read", Access::Read)]
            .into_iter()
            .flatten()
            .collect();
        if self.asks_network() {
            parts.push("network".to_owned());
        }
        if parts.is_empty() {
            "Review this request".to_owned()
        } else {
            parts.join("; ")
        }
    }

    /// What a session grant adds, for the "Extra access" chip's tooltip.
    pub fn grant_lines(&self) -> Vec<String> {
        let mut lines: Vec<String> = self
            .granted()
            .map(|e| {
                let verb = if e.access == Access::Write {
                    "Write"
                } else {
                    "Read"
                };
                format!("{verb}: {}", e.words())
            })
            .collect();
        if self.asks_network() {
            lines.push("Network access".to_owned());
        }
        lines
    }
}

fn parse_entry(raw: &Value) -> Option<Entry> {
    let object = raw.as_object()?;
    let access = match object.get("access")?.as_str()? {
        "read" => Access::Read,
        "write" => Access::Write,
        "deny" => Access::Deny,
        _ => return None,
    };
    let target = object.get("target")?.as_object()?;
    let string = |key: &str| {
        target
            .get(key)
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty() && plain(s))
            .map(str::to_owned)
    };
    let target = match target.get("kind")?.as_str()? {
        "path" => Target::Path(string("path")?),
        "glob" => Target::Glob(string("pattern")?),
        "special" => {
            let name = string("name")?;
            special_words(&name)?;
            let path = match target.get("path") {
                None | Some(Value::Null) => None,
                Some(_) => Some(string("path")?),
            };
            Target::Special { name, path }
        }
        _ => return None,
    };
    // A resolved location must be as readable as the target itself, or the entry is not exact.
    let resolved = match object.get("resolved") {
        None | Some(Value::Null) => None,
        Some(Value::String(s)) if !s.is_empty() && plain(s) => Some(s.clone()),
        Some(_) => return None,
    };
    Some(Entry {
        access,
        target,
        resolved,
    })
}

#[cfg(test)]
pub(super) mod tests {
    use super::*;
    use serde_json::json;

    /// A Codex permission request as the adapter hands it to the card: `entries` and `network`
    /// in the workspace `/w`, offering "Allow for session" when `session_allowed`.
    pub(in crate::chat::view) fn request_input(
        entries: Value,
        network: Value,
        session_allowed: bool,
    ) -> Value {
        json!({
            "cwd": "/w", "reason": "The build writes its cache", "renderable": true,
            "entries": entries, "network": network, "session_allowed": session_allowed,
            "warning": null, "requested": {"fileSystem": {"write": ["/w/out"]}}
        })
    }

    fn input(entries: Value, network: Value) -> Value {
        request_input(entries, network, true)
    }

    /// One entry: `access` to the path `p`.
    pub(in crate::chat::view) fn path(access: &str, p: &str) -> Value {
        json!({"access": access, "target": {"kind": "path", "path": p}})
    }

    #[test]
    fn every_entry_kind_reads_as_words() {
        let request = Request::parse(&input(
            json!([
                path("write", "/w/out"),
                {"access": "read", "target": {"kind": "glob", "pattern": "/w/**/*.rs"}},
                {"access": "read", "target": {"kind": "special", "name": "project", "path": null}},
                {"access": "read", "target": {"kind": "special", "name": "temp", "path": "/tmp/x"}},
                {"access": "write", "target": {"kind": "special", "name": "slash_tmp", "path": null}},
                {"access": "read", "target": {"kind": "special", "name": "minimal", "path": null}},
                {"access": "write", "target": {"kind": "special", "name": "root", "path": null}},
                path("deny", "/w/.git"),
            ]),
            json!(true),
        ));
        assert!(request.renderable);
        assert_eq!(
            request.lines(),
            [
                "Read",
                "  files matching /w/**/*.rs",
                "  the project",
                "  the temp folder (/tmp/x)",
                "  system libraries",
                "Write",
                "  /w/out",
                "  /tmp",
                "  the whole filesystem",
                "Still blocked",
                "  /w/.git",
                "Use the network",
            ]
        );
        assert!(request.reaches_everything());
        assert!(!request.allow_is_calm());
        assert!(request.warnings()[0].contains("whole filesystem"));
    }

    #[test]
    fn the_title_says_only_what_is_asked() {
        let title =
            |entries: Value, network: Value| Request::parse(&input(entries, network)).title();
        assert_eq!(
            title(
                json!([path("write", "/a"), path("write", "/b")]),
                json!(true)
            ),
            "Codex asks to write to 2 places and use the network"
        );
        assert_eq!(
            title(json!([path("read", "/a")]), json!(null)),
            "Codex asks to read 1 place",
            "a read-only request never says write"
        );
        assert_eq!(
            title(json!([]), json!(true)),
            "Codex asks to use the network"
        );
        assert_eq!(
            title(
                json!([path("write", "/a"), path("read", "/b")]),
                json!(true)
            ),
            "Codex asks to write to 1 place, read 1 place and use the network"
        );
        assert!(!title(json!([path("read", "/a")]), json!(null)).contains("sandbox"));
    }

    #[test]
    fn anything_it_cannot_type_exactly_is_unrenderable() {
        let bad = [
            json!([{"access": "execute", "target": {"kind": "path", "path": "/a"}}]),
            json!([{"access": "read", "target": {"kind": "special", "name": "unknown", "path": null}}]),
            json!([{"access": "read", "target": {"kind": "mystery"}}]),
            json!([path("read", "/a\u{202E}txt")]),
            json!([path("read", "/a\nb")]),
            json!([path("read", "")]),
        ];
        for entries in bad {
            let request = Request::parse(&input(entries.clone(), json!(null)));
            assert!(!request.renderable, "{entries}");
        }
        // The adapter's own verdict stands too, and a missing one means no.
        let mut unrenderable = input(json!([path("read", "/a")]), json!(null));
        unrenderable["renderable"] = json!(false);
        assert!(!Request::parse(&unrenderable).renderable);
        unrenderable.as_object_mut().map(|o| o.remove("renderable"));
        assert!(!Request::parse(&unrenderable).renderable);
        assert!(!Request::parse(&json!("not an object")).renderable);
        assert!(!Request::parse(&Value::Null).renderable);
        // Asking for nothing is not a request the user can judge.
        assert!(!Request::parse(&input(json!([]), json!(null))).renderable);
    }

    #[test]
    fn the_shelf_line_and_the_chip_list() {
        let request = Request::parse(&input(
            json!([path("write", "/a"), path("read", "/b"), path("deny", "/c")]),
            json!(true),
        ));
        assert_eq!(request.one_line(), "Write /a; Read /b; network");
        assert_eq!(
            request.grant_lines(),
            ["Write: /a", "Read: /b", "Network access"],
            "a blocked place is not an extra access"
        );
        assert_eq!(
            Request::parse(&Value::Null).one_line(),
            "Review this request"
        );
    }

    #[test]
    fn a_symlinked_target_shows_where_it_really_is_and_the_adapters_summary_wins() {
        let mut data = input(
            json!([{"access": "write", "resolved": "/real/out",
                    "target": {"kind": "path", "path": "/w/link"}}]),
            json!(null),
        );
        let request = Request::parse(&data);
        assert!(request.renderable);
        assert_eq!(request.lines(), ["Write", "  /w/link → /real/out"]);
        assert_eq!(request.grant_lines(), ["Write: /w/link → /real/out"]);
        assert_eq!(request.one_line(), "Write /w/link → /real/out");
        data["summary"] = json!("Write to /w/link (a symlink)");
        assert_eq!(
            Request::parse(&data).one_line(),
            "Write to /w/link (a symlink)"
        );
        // A resolved location or warning that is not plain text is not exact.
        data["entries"][0]["resolved"] = json!("/real/\u{202E}out");
        assert!(!Request::parse(&data).renderable);
        data["entries"][0]["resolved"] = json!(7);
        assert!(!Request::parse(&data).renderable);
        data["entries"][0]["resolved"] = json!(null);
        data["warning"] = json!("careful\u{200B}");
        assert!(!Request::parse(&data).renderable);
    }

    #[test]
    fn a_calm_request_keeps_allow_suggested() {
        let calm = Request::parse(&input(json!([path("write", "/w/out")]), json!(null)));
        assert!(calm.allow_is_calm());
        let mut warned = input(json!([path("write", "/w/out")]), json!(null));
        warned["warning"] = json!("This includes your home folder.");
        let warned = Request::parse(&warned);
        assert!(!warned.allow_is_calm());
        assert_eq!(warned.warnings(), ["This includes your home folder."]);
    }
}
