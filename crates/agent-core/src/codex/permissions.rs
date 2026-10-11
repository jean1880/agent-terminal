//! Codex permission requests (`item/permissions/requestApproval`), read into a typed,
//! display-ready model and the grant that model stands for.
//!
//! The view shows [`Parsed::input`]; the adapter answers Allow with [`Parsed::grant`], which is
//! rebuilt from the same typed entries, never by echoing the request. So what is granted is
//! exactly what was shown. Anything that cannot be typed exactly (an unknown special kind, an
//! unknown access mode or field, a path or pattern with control, bidi or zero-width characters,
//! a relative path with no usable `cwd`) makes the request unrenderable: it can only be denied.
//!
//! The `input` shape (the view lane renders it):
//!
//! ```json
//! { "cwd": "<abs>", "reason": "…" | null, "renderable": true,
//!   "entries": [ {"access": "read"|"write"|"deny",
//!                 "target": {"kind": "path", "path": "<abs>"}
//!                         | {"kind": "glob", "pattern": "<str>"}
//!                         | {"kind": "special", "name": "project"|"temp"|"slash_tmp"|"root"|"minimal",
//!                            "path": "<abs>" | null}} ],
//!   "network": true | false | null, "session_allowed": true, "warning": null,
//!   "requested": <the raw permissions object> }
//! ```
//!
//! `session_allowed` and `warning` are the session's to narrow (it knows the home folder and the
//! sensitive-path list); the adapter sets them to "allowed" and none for a renderable request.

use std::path::{Component, Path, PathBuf};

use serde_json::{json, Map, Value};

/// What a request is read into.
#[derive(Debug, Clone, PartialEq)]
pub(super) struct Parsed {
    /// The card's title, composed from what is asked.
    pub title: String,
    /// The display model (see the module docs).
    pub input: Value,
    /// Every entry could be typed exactly.
    pub renderable: bool,
    /// The `GrantedPermissionProfile` an Allow sends; `{}` when unrenderable.
    pub grant: Value,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Access {
    Read,
    Write,
    Deny,
}

impl Access {
    fn wire(self) -> &'static str {
        match self {
            Access::Read => "read",
            Access::Write => "write",
            Access::Deny => "deny",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Special {
    Project,
    Temp,
    SlashTmp,
    Root,
    Minimal,
}

impl Special {
    fn name(self) -> &'static str {
        match self {
            Special::Project => "project",
            Special::Temp => "temp",
            Special::SlashTmp => "slash_tmp",
            Special::Root => "root",
            Special::Minimal => "minimal",
        }
    }

    fn wire(self) -> &'static str {
        match self {
            Special::Project => "project_roots",
            Special::Temp => "tmpdir",
            Special::SlashTmp => "slash_tmp",
            Special::Root => "root",
            Special::Minimal => "minimal",
        }
    }

    fn words(self) -> &'static str {
        match self {
            Special::Project => "the project",
            Special::Temp => "the temp folder",
            Special::SlashTmp => "/tmp",
            Special::Root => "the whole filesystem",
            Special::Minimal => "system libraries",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum Target {
    /// Absolute and lexically normal (relative paths resolved against `cwd`).
    Path(String),
    Glob(String),
    Special {
        kind: Special,
        /// A `project_roots` subpath: relative, no `..` (checked).
        subpath: Option<String>,
        /// Where it resolves, when known.
        path: Option<String>,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct Entry {
    access: Access,
    target: Target,
    /// From the legacy `read` / `write` arrays: granted back in that form.
    legacy: bool,
}

/// The typed request, or `None` when any part of it cannot be typed exactly.
#[derive(Debug, Default)]
struct Typed {
    entries: Vec<Entry>,
    network: Option<bool>,
    glob_depth: Option<u64>,
}

/// Reads a permission request's params.
pub(super) fn parse(params: &Value) -> Parsed {
    let requested = params.get("permissions").cloned().unwrap_or(Value::Null);
    let reason = params
        .get("reason")
        .and_then(Value::as_str)
        .map(str::to_owned);
    let cwd = params
        .get("cwd")
        .and_then(Value::as_str)
        .filter(|c| displayable(c))
        .map(Path::new)
        .filter(|c| c.is_absolute())
        .map(normal);
    let typed = typed(&requested, cwd.as_deref());
    let cwd_text = cwd
        .as_ref()
        .map(|c| Value::String(c.to_string_lossy().into_owned()))
        .unwrap_or(Value::Null);
    match typed {
        Some(typed) => Parsed {
            title: title(&typed),
            input: json!({
                "cwd": cwd_text,
                "reason": reason,
                "renderable": true,
                "entries": typed.entries.iter().map(entry_view).collect::<Vec<_>>(),
                "network": typed.network,
                "session_allowed": true,
                "warning": Value::Null,
                "requested": requested,
            }),
            renderable: true,
            grant: grant(&typed),
        },
        None => Parsed {
            title: "Codex asks for access agent-terminal cannot show exactly".to_owned(),
            input: json!({
                "cwd": cwd_text,
                "reason": reason,
                "renderable": false,
                "entries": [],
                "network": Value::Null,
                "session_allowed": false,
                "warning": "agent-terminal cannot read every part of this request, so it can only be denied.",
                "requested": requested,
            }),
            renderable: false,
            grant: json!({}),
        },
    }
}

/// Text that reads as what it is: not empty, and no control, bidi, zero-width or tag
/// characters (which could make a path look like another).
fn displayable(text: &str) -> bool {
    !text.is_empty() && !text.chars().any(hidden_char)
}

fn hidden_char(c: char) -> bool {
    c.is_control()
        || matches!(c,
            '\u{00AD}' | '\u{061C}' | '\u{180E}'
            | '\u{200B}'..='\u{200F}'
            | '\u{2028}'..='\u{202E}'
            | '\u{2060}'..='\u{2069}'
            | '\u{FEFF}'
            | '\u{FFF9}'..='\u{FFFB}'
            | '\u{E0000}'..='\u{E007F}')
}

/// `path` with `.` dropped and `..` applied lexically (`/a/b/../c` is `/a/c`; `..` at the root
/// stays there). Only called on absolute paths.
fn normal(path: &Path) -> PathBuf {
    let mut out = PathBuf::from("/");
    for part in path.components() {
        match part {
            Component::Normal(name) => out.push(name),
            Component::ParentDir => {
                out.pop();
            }
            Component::RootDir | Component::CurDir | Component::Prefix(_) => {}
        }
    }
    out
}

/// Keys outside `known`, or a value of the wrong kind, make the object untypeable.
fn only_keys(obj: &Map<String, Value>, known: &[&str]) -> bool {
    obj.keys().all(|k| known.contains(&k.as_str()))
}

fn typed(requested: &Value, cwd: Option<&Path>) -> Option<Typed> {
    let mut out = Typed::default();
    let profile = requested.as_object()?;
    if !only_keys(profile, &["fileSystem", "network"]) {
        return None;
    }
    match profile.get("network") {
        None | Some(Value::Null) => {}
        Some(Value::Object(network)) => {
            if !only_keys(network, &["enabled"]) {
                return None;
            }
            out.network = match network.get("enabled") {
                None | Some(Value::Null) => None,
                Some(Value::Bool(b)) => Some(*b),
                Some(_) => return None,
            };
        }
        Some(_) => return None,
    }
    match profile.get("fileSystem") {
        None | Some(Value::Null) => {}
        Some(Value::Object(fs)) => {
            if !only_keys(fs, &["read", "write", "entries", "globScanMaxDepth"]) {
                return None;
            }
            for (key, access) in [("read", Access::Read), ("write", Access::Write)] {
                for path in array(fs.get(key))? {
                    let path = resolve(path.as_str()?, cwd)?;
                    out.entries.push(Entry {
                        access,
                        target: Target::Path(path),
                        legacy: true,
                    });
                }
            }
            for entry in array(fs.get("entries"))? {
                out.entries.push(sandbox_entry(entry, cwd)?);
            }
            out.glob_depth = match fs.get("globScanMaxDepth") {
                None | Some(Value::Null) => None,
                Some(v) => Some(v.as_u64().filter(|n| *n >= 1)?),
            };
        }
        Some(_) => return None,
    }
    Some(out)
}

/// The items of an optional array field: absent or `null` is none; anything else fails.
fn array(value: Option<&Value>) -> Option<&[Value]> {
    match value {
        None | Some(Value::Null) => Some(&[]),
        Some(Value::Array(items)) => Some(items),
        Some(_) => None,
    }
}

/// An absolute, lexically normal path, from a path that is absolute or relative to `cwd`.
fn resolve(path: &str, cwd: Option<&Path>) -> Option<String> {
    if !displayable(path) || path.starts_with('~') {
        return None;
    }
    let p = Path::new(path);
    let full = if p.is_absolute() {
        normal(p)
    } else {
        normal(&cwd?.join(p))
    };
    Some(full.to_string_lossy().into_owned())
}

fn sandbox_entry(entry: &Value, cwd: Option<&Path>) -> Option<Entry> {
    let obj = entry.as_object()?;
    if !only_keys(obj, &["access", "path"]) {
        return None;
    }
    let access = match obj.get("access")?.as_str()? {
        "read" => Access::Read,
        "write" => Access::Write,
        "deny" => Access::Deny,
        _ => return None,
    };
    let path = obj.get("path")?.as_object()?;
    let target = match path.get("type")?.as_str()? {
        "path" => {
            if !only_keys(path, &["type", "path"]) {
                return None;
            }
            Target::Path(resolve(path.get("path")?.as_str()?, cwd)?)
        }
        "glob_pattern" => {
            if !only_keys(path, &["type", "pattern"]) {
                return None;
            }
            let pattern = path.get("pattern")?.as_str()?;
            if !displayable(pattern) {
                return None;
            }
            Target::Glob(pattern.to_owned())
        }
        "special" => {
            if !only_keys(path, &["type", "value"]) {
                return None;
            }
            special(path.get("value")?.as_object()?, cwd)?
        }
        _ => return None,
    };
    Some(Entry {
        access,
        target,
        legacy: false,
    })
}

fn special(value: &Map<String, Value>, cwd: Option<&Path>) -> Option<Target> {
    let kind = match value.get("kind")?.as_str()? {
        "project_roots" => Special::Project,
        "tmpdir" => Special::Temp,
        "slash_tmp" => Special::SlashTmp,
        "root" => Special::Root,
        "minimal" => Special::Minimal,
        // `unknown` carries a free path Codex itself could not name: never typeable.
        _ => return None,
    };
    let subpath = if kind == Special::Project {
        if !only_keys(value, &["kind", "subpath"]) {
            return None;
        }
        match value.get("subpath") {
            None | Some(Value::Null) => None,
            Some(v) => {
                let sub = v.as_str()?;
                let p = Path::new(sub);
                // A subpath is shown resolved inside the project; one that climbs out of it or
                // names an absolute path would grant somewhere other than what is shown.
                if !displayable(sub)
                    || p.is_absolute()
                    || sub.starts_with('~')
                    || p.components().any(|c| !matches!(c, Component::Normal(_)))
                {
                    return None;
                }
                Some(sub.to_owned())
            }
        }
    } else {
        if !only_keys(value, &["kind"]) {
            return None;
        }
        None
    };
    let path = match kind {
        Special::Project => cwd.map(|c| {
            let full = match &subpath {
                Some(sub) => c.join(sub),
                None => c.to_owned(),
            };
            normal(&full).to_string_lossy().into_owned()
        }),
        Special::SlashTmp => Some("/tmp".to_owned()),
        Special::Root => Some("/".to_owned()),
        Special::Temp | Special::Minimal => None,
    };
    Some(Target::Special {
        kind,
        subpath,
        path,
    })
}

fn entry_view(entry: &Entry) -> Value {
    let target = match &entry.target {
        Target::Path(path) => json!({"kind": "path", "path": path}),
        Target::Glob(pattern) => json!({"kind": "glob", "pattern": pattern}),
        Target::Special { kind, path, .. } => {
            json!({"kind": "special", "name": kind.name(), "path": path})
        }
    };
    json!({"access": entry.access.wire(), "target": target})
}

/// The grant for an Allow: the typed entries in the form each was asked in, the network flag
/// and the glob depth. Paths are the resolved ones the card showed.
fn grant(typed: &Typed) -> Value {
    let mut fs = Map::new();
    for (key, access) in [("read", Access::Read), ("write", Access::Write)] {
        let legacy: Vec<Value> = typed
            .entries
            .iter()
            .filter(|e| e.legacy && e.access == access)
            .filter_map(|e| match &e.target {
                Target::Path(p) => Some(json!(p)),
                _ => None,
            })
            .collect();
        if !legacy.is_empty() {
            fs.insert(key.to_owned(), Value::Array(legacy));
        }
    }
    let entries: Vec<Value> = typed
        .entries
        .iter()
        .filter(|e| !e.legacy)
        .map(|e| {
            let path = match &e.target {
                Target::Path(p) => json!({"type": "path", "path": p}),
                Target::Glob(pattern) => json!({"type": "glob_pattern", "pattern": pattern}),
                Target::Special { kind, subpath, .. } => {
                    let mut value = Map::new();
                    value.insert("kind".to_owned(), json!(kind.wire()));
                    if let Some(sub) = subpath {
                        value.insert("subpath".to_owned(), json!(sub));
                    }
                    json!({"type": "special", "value": value})
                }
            };
            json!({"access": e.access.wire(), "path": path})
        })
        .collect();
    if !entries.is_empty() {
        fs.insert("entries".to_owned(), Value::Array(entries));
    }
    if let Some(depth) = typed.glob_depth {
        fs.insert("globScanMaxDepth".to_owned(), json!(depth));
    }
    let mut grant = Map::new();
    if !fs.is_empty() {
        grant.insert("fileSystem".to_owned(), Value::Object(fs));
    }
    if let Some(enabled) = typed.network {
        grant.insert("network".to_owned(), json!({"enabled": enabled}));
    }
    Value::Object(grant)
}

/// One target in words: its path, its pattern, or the special place's name.
fn words(target: &Target) -> String {
    match target {
        Target::Path(path) => path.clone(),
        Target::Glob(pattern) => format!("files matching {pattern}"),
        Target::Special {
            kind: Special::Project,
            subpath: Some(sub),
            ..
        } => format!("{sub} in the project"),
        Target::Special { kind, .. } => kind.words().to_owned(),
    }
}

/// One target when there is one, else a count.
fn places(entries: &[&Entry]) -> String {
    match entries {
        [one] => words(&one.target),
        many => format!("{} locations", many.len()),
    }
}

/// "a", "a and b", "a, b and c".
fn sentence(parts: &[String]) -> String {
    match parts {
        [] => String::new(),
        [one] => one.clone(),
        [rest @ .., last] => format!("{} and {last}", rest.join(", ")),
    }
}

/// "Codex asks to write to 2 locations, read /etc/hosts and use the network". A read-only or
/// network-only request never says "write"; deny entries are what stays blocked.
fn title(typed: &Typed) -> String {
    let of = |access| {
        typed
            .entries
            .iter()
            .filter(|e| e.access == access)
            .collect::<Vec<_>>()
    };
    let (writes, reads, denies) = (of(Access::Write), of(Access::Read), of(Access::Deny));
    let mut parts = Vec::new();
    if !writes.is_empty() {
        parts.push(format!("write to {}", places(&writes)));
    }
    if !reads.is_empty() {
        parts.push(format!("read {}", places(&reads)));
    }
    if typed.network == Some(true) {
        parts.push("use the network".to_owned());
    }
    if !parts.is_empty() {
        format!("Codex asks to {}", sentence(&parts))
    } else if !denies.is_empty() {
        format!("Codex asks to keep {} blocked", places(&denies))
    } else {
        "Codex sent a permission request that asks for nothing".to_owned()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn req(permissions: Value) -> Value {
        json!({"threadId": "th", "turnId": "tu", "itemId": "it", "startedAtMs": 1,
               "cwd": "/work/repo", "reason": "needs it", "permissions": permissions})
    }

    fn entry(access: &str, path: Value) -> Value {
        json!({"access": access, "path": path})
    }

    #[test]
    fn entries_and_legacy_arrays_type_resolve_and_grant_what_is_shown() {
        let p = parse(&req(json!({
            "fileSystem": {
                "read": ["docs/../README.md"],
                "write": ["/work/repo/out"],
                "entries": [
                    entry("write", json!({"type": "path", "path": "target"})),
                    entry("read", json!({"type": "glob_pattern", "pattern": "/data/**/*.csv"})),
                    entry("write", json!({"type": "special", "value": {"kind": "project_roots", "subpath": "build"}})),
                    entry("deny", json!({"type": "special", "value": {"kind": "slash_tmp"}})),
                ],
                "globScanMaxDepth": 3,
            },
            "network": {"enabled": true},
        })));
        assert!(p.renderable);
        assert_eq!(p.input["cwd"], "/work/repo");
        assert_eq!(p.input["reason"], "needs it");
        assert_eq!(p.input["network"], true);
        assert_eq!(p.input["session_allowed"], true);
        assert_eq!(
            p.input["entries"],
            json!([
                {"access": "read", "target": {"kind": "path", "path": "/work/repo/README.md"}},
                {"access": "write", "target": {"kind": "path", "path": "/work/repo/out"}},
                {"access": "write", "target": {"kind": "path", "path": "/work/repo/target"}},
                {"access": "read", "target": {"kind": "glob", "pattern": "/data/**/*.csv"}},
                {"access": "write", "target": {"kind": "special", "name": "project", "path": "/work/repo/build"}},
                {"access": "deny", "target": {"kind": "special", "name": "slash_tmp", "path": "/tmp"}},
            ])
        );
        assert_eq!(
            p.grant,
            json!({
                "fileSystem": {
                    "read": ["/work/repo/README.md"],
                    "write": ["/work/repo/out"],
                    "entries": [
                        {"access": "write", "path": {"type": "path", "path": "/work/repo/target"}},
                        {"access": "read", "path": {"type": "glob_pattern", "pattern": "/data/**/*.csv"}},
                        {"access": "write", "path": {"type": "special", "value": {"kind": "project_roots", "subpath": "build"}}},
                        {"access": "deny", "path": {"type": "special", "value": {"kind": "slash_tmp"}}},
                    ],
                    "globScanMaxDepth": 3,
                },
                "network": {"enabled": true},
            })
        );
        assert_eq!(
            p.title,
            "Codex asks to write to 3 locations, read 2 locations and use the network"
        );
    }

    #[test]
    fn titles_name_only_what_is_asked() {
        let title = |perms: Value| parse(&req(perms)).title;
        assert_eq!(
            title(json!({"network": {"enabled": true}})),
            "Codex asks to use the network"
        );
        assert_eq!(
            title(json!({"fileSystem": {"read": ["/etc/hosts"]}})),
            "Codex asks to read /etc/hosts"
        );
        assert_eq!(
            title(
                json!({"fileSystem": {"entries": [entry("write", json!({"type": "special", "value": {"kind": "root"}}))]}})
            ),
            "Codex asks to write to the whole filesystem"
        );
        assert_eq!(
            title(
                json!({"fileSystem": {"entries": [entry("deny", json!({"type": "path", "path": "/secret"}))]}})
            ),
            "Codex asks to keep /secret blocked"
        );
        assert_eq!(
            title(json!({"network": {"enabled": false}})),
            "Codex sent a permission request that asks for nothing"
        );
        for t in [
            title(json!({"network": {"enabled": true}})),
            title(json!({"fileSystem": {"read": ["/a", "/b"]}})),
        ] {
            assert!(!t.contains("write") && !t.contains("sandbox"), "{t}");
        }
    }

    #[test]
    fn anything_not_typeable_exactly_is_unrenderable_and_grants_nothing() {
        let special = |value: Value| json!({"fileSystem": {"entries": [entry("write", json!({"type": "special", "value": value}))]}});
        for (why, perms) in [
            (
                "unknown special with a free path",
                special(json!({"kind": "unknown", "path": "/etc", "subpath": null})),
            ),
            (
                "project subpath climbing out",
                special(json!({"kind": "project_roots", "subpath": "../.."})),
            ),
            (
                "absolute project subpath",
                special(json!({"kind": "project_roots", "subpath": "/etc"})),
            ),
            (
                "extra field on a special",
                special(json!({"kind": "root", "subpath": "x"})),
            ),
            (
                "bidi in a path",
                json!({"fileSystem": {"write": ["/work/\u{202e}txt.exe"]}}),
            ),
            (
                "zero-width in a pattern",
                json!({"fileSystem": {"entries": [entry("read", json!({"type": "glob_pattern", "pattern": "/a\u{200b}/**"}))]}}),
            ),
            ("control char", json!({"fileSystem": {"read": ["/a\nb"]}})),
            (
                "unknown access",
                json!({"fileSystem": {"entries": [entry("execute", json!({"type": "path", "path": "/a"}))]}}),
            ),
            (
                "unknown entry field",
                json!({"fileSystem": {"entries": [{"access": "read", "path": {"type": "path", "path": "/a"}, "recursive": true}]}}),
            ),
            (
                "unknown path type",
                json!({"fileSystem": {"entries": [entry("read", json!({"type": "url", "path": "/a"}))]}}),
            ),
            (
                "unknown top-level field",
                json!({"network": {"enabled": true}, "macos": {"preferences": true}}),
            ),
            (
                "unknown fileSystem field",
                json!({"fileSystem": {"read": ["/a"], "mounts": ["/b"]}}),
            ),
            (
                "unknown network field",
                json!({"network": {"enabled": true, "hosts": ["*"]}}),
            ),
            ("home shorthand", json!({"fileSystem": {"read": ["~/x"]}})),
            ("empty path", json!({"fileSystem": {"read": [""]}})),
            ("non-string path", json!({"fileSystem": {"read": [3]}})),
            (
                "zero glob depth",
                json!({"fileSystem": {"globScanMaxDepth": 0}}),
            ),
            ("not an object", json!(["read"])),
        ] {
            let p = parse(&req(perms));
            assert!(!p.renderable, "{why}");
            assert_eq!(p.input["renderable"], false, "{why}");
            assert_eq!(p.input["session_allowed"], false, "{why}");
            assert_eq!(p.input["entries"], json!([]), "{why}");
            assert_eq!(p.grant, json!({}), "{why}");
            assert!(p.input["warning"].is_string(), "{why}");
        }
    }

    #[test]
    fn a_relative_path_needs_an_absolute_cwd() {
        let mut params = req(json!({"fileSystem": {"read": ["src"]}}));
        params["cwd"] = json!("relative/dir");
        assert!(!parse(&params).renderable);
        params["cwd"] = json!("/work/./repo/");
        let p = parse(&params);
        assert!(p.renderable);
        assert_eq!(p.input["entries"][0]["target"]["path"], "/work/repo/src");
        // An absolute path needs no cwd at all.
        params["cwd"] = Value::Null;
        params["permissions"] = json!({"fileSystem": {"read": ["/srv/x/../y"]}});
        let p = parse(&params);
        assert!(p.renderable);
        assert_eq!(p.grant, json!({"fileSystem": {"read": ["/srv/y"]}}));
    }

    #[test]
    fn deny_only_and_empty_requests_render() {
        let p = parse(&req(json!({"fileSystem": {"entries": [
            entry("deny", json!({"type": "glob_pattern", "pattern": "**/.env"}))
        ]}})));
        assert!(p.renderable);
        assert_eq!(p.title, "Codex asks to keep files matching **/.env blocked");
        let p = parse(&req(json!({})));
        assert!(p.renderable);
        assert_eq!(p.grant, json!({}));
        assert_eq!(p.input["network"], Value::Null);
    }
}
