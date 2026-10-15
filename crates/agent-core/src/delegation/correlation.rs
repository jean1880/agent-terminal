//! Invocation keys and attestation (contract §2 "Attestation arguments", §4).
//!
//! A bridge call is attested when its key matches an unconsumed call to `agent_terminal` seen
//! in the principal's own provider stream, with the same tool and the same attestation
//! arguments. Attestation never grants authority: the caller only denies on `Unattested`.

use std::collections::VecDeque;
use std::fmt;
use std::hash::{DefaultHasher, Hasher};

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

use super::error::ToolError;
use super::limits::Limits;
use super::tools::ToolName;
use super::wire::{canonical_json, parse_request};
use crate::adapter::Driver;

/// Longest invocation key field, in bytes.
pub const MAX_KEY_FIELD_BYTES: usize = 256;

/// How a provider identifies one tool call (copied from `_meta`, nothing else).
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(tag = "provider", rename_all = "snake_case")]
pub enum InvocationKey {
    /// `_meta["claudecode/toolUseId"]` = the stream's `tool_use.id`.
    Claude { tool_use_id: String },
    /// `(_meta.threadId, _meta.callId)` = the `mcpToolCall` item.
    Codex { thread_id: String, call_id: String },
    /// `_meta["antigravity.google/conversation_id"]` and the `<n>` of `progressToken`
    /// `"<uuid>:<n>"` = `step_update.step_index`.
    Agy {
        conversation_id: String,
        step_index: u64,
    },
}

fn key_field<'a>(meta: &'a Map<String, Value>, name: &str) -> Option<&'a str> {
    match meta.get(name) {
        Some(Value::String(s)) if !s.is_empty() && s.len() <= MAX_KEY_FIELD_BYTES => Some(s),
        _ => None,
    }
}

/// `"<id>:<n>"` → n.
fn agy_step(progress_token: &str) -> Option<u64> {
    let (id, n) = progress_token.rsplit_once(':')?;
    if id.is_empty() || n.is_empty() || !n.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    n.parse().ok()
}

/// The whitelisted keys only (contract §4); every other `_meta` entry, Codex
/// `x-codex-turn-metadata` included, is ignored. A present but malformed or over-long value
/// gives `None`.
pub fn invocation_from_meta(meta: &Map<String, Value>) -> Option<InvocationKey> {
    if meta.contains_key("claudecode/toolUseId") {
        return key_field(meta, "claudecode/toolUseId").map(|id| InvocationKey::Claude {
            tool_use_id: id.to_owned(),
        });
    }
    if meta.contains_key("threadId") || meta.contains_key("callId") {
        let thread_id = key_field(meta, "threadId")?;
        let call_id = key_field(meta, "callId")?;
        return Some(InvocationKey::Codex {
            thread_id: thread_id.to_owned(),
            call_id: call_id.to_owned(),
        });
    }
    if meta.contains_key("antigravity.google/conversation_id") {
        let conversation_id = key_field(meta, "antigravity.google/conversation_id")?;
        let token = key_field(meta, "progressToken")?;
        return Some(InvocationKey::Agy {
            conversation_id: conversation_id.to_owned(),
            step_index: agy_step(token)?,
        });
    }
    None
}

/// The attestation form of a call's arguments. Its `Debug` prints only the length: it may
/// hold un-redacted text.
#[derive(Clone, PartialEq, Eq)]
pub struct AttestationArgs(String);

impl fmt::Debug for AttestationArgs {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "AttestationArgs({} bytes)", self.0.len())
    }
}

impl AttestationArgs {
    pub fn len(&self) -> usize {
        self.0.len()
    }

    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }
}

/// The ONE helper for both the stream step (`record`) and the bridge call (`attest`).
/// `raw` may be an object or a JSON string holding one. Validated by the tool's schema,
/// syntax-normalised (defaults filled), every input field kept (`idempotency_key` and
/// `expected_authority_epoch` included), NOT scrubbed, sorted-key JSON. Distinct from
/// `operation::canonical_request`, which is scrubbed and drops the key and epoch.
pub fn attestation_arguments(
    tool: ToolName,
    raw: &Value,
    limits: &Limits,
) -> Result<AttestationArgs, ToolError> {
    let parsed;
    let object = match raw {
        Value::String(s) => {
            parsed = serde_json::from_str::<Value>(s)
                .map_err(|_| ToolError::invalid("arguments", "not_json"))?;
            &parsed
        }
        other => other,
    };
    let req = parse_request(tool, object, limits)?;
    Ok(AttestationArgs(canonical_json(
        &req.normalised().args_value(),
    )))
}

/// Whether a call by a `driver` principal must be attested. agy: every tool, always.
/// Claude and Codex: every tool, once their gate (G-C5 / G-X5) passed.
pub fn attestation_required(driver: Driver, tool: ToolName, gate_passed: bool) -> bool {
    let _every_tool = tool;
    match driver {
        Driver::Agy => true,
        Driver::Claude | Driver::Codex => gate_passed,
    }
}

/// The outcome of [`ObservedCalls::attest`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Attestation {
    Attested,
    Unattested,
}

/// One principal generation's observed native calls to `agent_terminal`, newest last. Each
/// keeps its key, its tool and a salted 128-bit digest of its attestation arguments: two
/// std SipHash passes, each over half the salt and then the arguments (the same construction
/// as `redact::stream`'s key digest, seeded from the salt). Never persisted or logged.
pub struct ObservedCalls {
    salt: [u8; 32],
    cap: usize,
    calls: VecDeque<(InvocationKey, ToolName, [u64; 2])>,
}

impl fmt::Debug for ObservedCalls {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ObservedCalls")
            .field("cap", &self.cap)
            .field("len", &self.calls.len())
            .finish_non_exhaustive()
    }
}

impl ObservedCalls {
    /// `salt`: 32 random bytes from the app, one per principal generation.
    pub fn new(salt: [u8; 32], cap: usize) -> Self {
        Self {
            salt,
            cap: cap.max(1),
            calls: VecDeque::new(),
        }
    }

    fn digest(&self, args: &AttestationArgs) -> [u64; 2] {
        let pass = |half: &[u8]| {
            let mut h = DefaultHasher::new();
            h.write(half);
            h.write(args.0.as_bytes());
            h.finish()
        };
        [pass(&self.salt[..16]), pass(&self.salt[16..])]
    }

    pub fn len(&self) -> usize {
        self.calls.len()
    }

    pub fn is_empty(&self) -> bool {
        self.calls.is_empty()
    }

    /// Records a call seen in the stream; past `cap`, the oldest is dropped.
    pub fn record(
        &mut self,
        key: InvocationKey,
        tool: ToolName,
        attestation_args: &AttestationArgs,
    ) {
        let digest = self.digest(attestation_args);
        if self.calls.len() >= self.cap {
            self.calls.pop_front();
        }
        self.calls.push_back((key, tool, digest));
    }

    /// Consumes the matching observed call, if any. Never grants authority.
    pub fn attest(
        &mut self,
        key: Option<&InvocationKey>,
        tool: ToolName,
        attestation_args: &AttestationArgs,
    ) -> Attestation {
        let Some(key) = key else {
            return Attestation::Unattested;
        };
        let digest = self.digest(attestation_args);
        match self
            .calls
            .iter()
            .position(|(k, t, d)| k == key && *t == tool && *d == digest)
        {
            Some(i) => {
                self.calls.remove(i);
                Attestation::Attested
            }
            None => Attestation::Unattested,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::redact::redact;
    use serde_json::json;

    fn meta(v: Value) -> Map<String, Value> {
        match v {
            Value::Object(m) => m,
            _ => panic!("not an object"),
        }
    }

    /// Scrubbed `_meta` from tests/fixtures/real/delegation/claude/d1_basic/mcp-server.ndjson.
    fn claude_meta() -> Map<String, Value> {
        meta(json!({"claudecode/toolUseId": "toolu_01JqmgHtuKFaLatCTWfyf523", "progressToken": 2}))
    }

    /// Scrubbed `_meta` from tests/fixtures/real/delegation/codex/d3-d6-mcp-approval.mcp-server.jsonl.
    fn codex_meta() -> Map<String, Value> {
        meta(json!({
            "callId": "exec-c520e370-5e14-491f-82f2-1dee3b60ab00",
            "x-codex-turn-metadata": {
                "session_id": "01a13d76-0b94-7f21-9e07-616ff4bcf552",
                "thread_id": "01a13d76-0b94-7f21-9e07-616ff4bcf552",
                "reasoning_effort": "low",
                "turn_id": "01a13d76-0d33-7f73-8796-dab30b29cb51",
                "model": "gpt-5.6-luna",
                "sandbox": "seccomp",
                "sandbox_mode": "read-only",
                "auto_review_enabled": false,
                "node_repl_auto_review_required": false,
                "node_repl_disabled": false,
                "workspaces": {"/home/user/git/agent-terminal": {
                    "associated_remote_urls": {"origin": "<EMAIL>:owner/agent-terminal.git"},
                    "latest_git_commit_hash": "<sha>",
                    "has_changes": true
                }},
                "turn_started_at_unix_ms": 1792032509236u64,
                "codex_version": "0.161.0"
            },
            "threadId": "01a13d76-0b94-7f21-9e07-616ff4bcf552",
            "sessionId": "01a13d76-0b94-7f21-9e07-616ff4bcf552",
            "windowId": "01a13d76-0b94-7f21-9e07-616ff4bcf552:0",
            "itemId": "ctc_061b9b50a33835a1016ad03f02b700819398ce760cf8bdcbc8",
            "progressToken": 1
        }))
    }

    /// Scrubbed `_meta` from tests/fixtures/real/delegation/agy/d1-d3-mcp-exchange.global.mcp-server.jsonl.
    fn agy_meta() -> Map<String, Value> {
        meta(json!({
            "antigravity.google/artifacts_dir": "$TMP_HOME/.gemini/antigravity-cli/brain/00000000-0000-4000-8000-000000000001",
            "antigravity.google/conversation_id": "00000000-0000-4000-8000-000000000001",
            "progressToken": "00000000-0000-4000-8000-000000000002:2"
        }))
    }

    #[test]
    fn invocation_keys_come_from_the_recorded_meta() {
        assert_eq!(
            invocation_from_meta(&claude_meta()),
            Some(InvocationKey::Claude {
                tool_use_id: "toolu_01JqmgHtuKFaLatCTWfyf523".into()
            })
        );
        let codex = invocation_from_meta(&codex_meta());
        assert_eq!(
            codex,
            Some(InvocationKey::Codex {
                thread_id: "01a13d76-0b94-7f21-9e07-616ff4bcf552".into(),
                call_id: "exec-c520e370-5e14-491f-82f2-1dee3b60ab00".into()
            })
        );
        // x-codex-turn-metadata (git remote, HEAD) never reaches the key.
        let json = serde_json::to_string(&codex).expect("ser");
        assert!(!json.contains("associated_remote_urls"));
        assert!(!json.contains("gpt-5.6-luna"));
        assert_eq!(
            invocation_from_meta(&agy_meta()),
            Some(InvocationKey::Agy {
                conversation_id: "00000000-0000-4000-8000-000000000001".into(),
                step_index: 2
            })
        );
        assert_eq!(invocation_from_meta(&Map::new()), None);
    }

    #[test]
    fn malformed_and_over_long_values_give_none() {
        for bad in [
            "no-colon",
            "uuid:",
            ":3",
            "uuid:-1",
            "uuid:x2",
            "uuid:99999999999999999999999",
        ] {
            let mut m = agy_meta();
            m.insert("progressToken".into(), json!(bad));
            assert_eq!(invocation_from_meta(&m), None, "{bad}");
        }
        let mut m = agy_meta();
        m.insert("progressToken".into(), json!(5));
        assert_eq!(invocation_from_meta(&m), None);
        let long = "x".repeat(MAX_KEY_FIELD_BYTES + 1);
        let mut c = claude_meta();
        c.insert("claudecode/toolUseId".into(), json!(long));
        assert_eq!(invocation_from_meta(&c), None);
        let mut x = codex_meta();
        x.insert("callId".into(), json!(long));
        assert_eq!(invocation_from_meta(&x), None);
        let mut x = codex_meta();
        x.remove("threadId");
        assert_eq!(invocation_from_meta(&x), None);
        let ok = "y".repeat(MAX_KEY_FIELD_BYTES);
        let mut c = claude_meta();
        c.insert("claudecode/toolUseId".into(), json!(ok));
        assert!(invocation_from_meta(&c).is_some());
    }

    fn send_args(extra: Value) -> Value {
        let mut base = json!({
            "target": "w_abcdefgh",
            "text": "please continue",
            "kind": "instruction",
            "delivery": "auto",
            "idempotency_key": "k1"
        });
        if let (Value::Object(b), Value::Object(e)) = (&mut base, extra) {
            b.extend(e);
        }
        base
    }

    fn args(tool: ToolName, raw: &Value) -> AttestationArgs {
        attestation_arguments(tool, raw, &Limits::default()).expect("valid")
    }

    #[test]
    fn attestation_arguments_normalise_syntax_only() {
        let read_stream =
            json!({"after": "c1", "acknowledge": ["m_aaaaaaaa"], "idempotency_key": "k"});
        let read_bridge = json!({"idempotency_key": "k", "max_bytes": 65536, "max_events": 100,
                                 "acknowledge": ["m_aaaaaaaa"], "after": "c1"});
        let from_stream = args(ToolName::CrossAgentRead, &read_stream);
        // The stream may carry the arguments as a JSON string.
        let as_string = Value::String(read_stream.to_string());
        assert_eq!(args(ToolName::CrossAgentRead, &as_string), from_stream);
        assert_eq!(args(ToolName::CrossAgentRead, &read_bridge), from_stream);
        let spawn = json!({"profile_id": "codex", "name": "n", "task": "t", "task_mode": "plan",
                           "workspace": {"kind": "isolated_worktree"}, "idempotency_key": "k"});
        let mut explicit = spawn.clone();
        explicit["workspace"]["base"] = json!("HEAD");
        assert_eq!(
            args(ToolName::CrossAgentSpawn, &spawn),
            args(ToolName::CrossAgentSpawn, &explicit)
        );
    }

    #[test]
    fn attestation_arguments_keep_every_field_unscrubbed() {
        let base = args(ToolName::CrossAgentSend, &send_args(json!({})));
        let changed_key = args(
            ToolName::CrossAgentSend,
            &send_args(json!({"idempotency_key": "k2"})),
        );
        assert_ne!(base, changed_key);
        let with_epoch = args(
            ToolName::CrossAgentSend,
            &send_args(json!({"expected_authority_epoch": 3})),
        );
        assert_ne!(base, with_epoch);
        let other_epoch = args(
            ToolName::CrossAgentSend,
            &send_args(json!({"expected_authority_epoch": 4})),
        );
        assert_ne!(with_epoch, other_epoch);
        // Two secrets that redact scrubs to the same text still differ.
        // Built at run time so no credential-shaped literal sits in the source.
        let s1 = format!(
            "token {}{}WXYZ end",
            "ghp",
            "_".to_owned() + &"a".repeat(32)
        );
        let s2 = format!(
            "token {}{}WXYZ end",
            "ghp",
            "_".to_owned() + &"b".repeat(32)
        );
        assert_eq!(redact(&s1), redact(&s2));
        let a1 = args(ToolName::CrossAgentSend, &send_args(json!({"text": s1})));
        let a2 = args(ToolName::CrossAgentSend, &send_args(json!({"text": s2})));
        assert_ne!(a1, a2);
        let dbg = format!("{a1:?}");
        assert!(!dbg.contains("ghp_"));
        assert!(!dbg.contains("token"));
        // An invalid call cannot be attested.
        assert!(attestation_arguments(
            ToolName::CrossAgentSend,
            &json!({"text": "x"}),
            &Limits::default()
        )
        .is_err());
        assert!(attestation_arguments(
            ToolName::CrossAgentSend,
            &json!("not json {"),
            &Limits::default()
        )
        .is_err());
    }

    #[test]
    fn observed_calls_attest_once_and_only_the_same_call() {
        let key = InvocationKey::Claude {
            tool_use_id: "toolu_1".into(),
        };
        let a = args(ToolName::CrossAgentSend, &send_args(json!({})));
        let other = args(
            ToolName::CrossAgentSend,
            &send_args(json!({"idempotency_key": "k9"})),
        );
        let mut o = ObservedCalls::new([3; 32], 4);
        o.record(key.clone(), ToolName::CrossAgentSend, &a);
        assert_eq!(
            o.attest(Some(&key), ToolName::CrossAgentCancel, &a),
            Attestation::Unattested
        );
        assert_eq!(
            o.attest(Some(&key), ToolName::CrossAgentSend, &other),
            Attestation::Unattested
        );
        assert_eq!(
            o.attest(None, ToolName::CrossAgentSend, &a),
            Attestation::Unattested
        );
        assert_eq!(
            o.attest(Some(&key), ToolName::CrossAgentSend, &a),
            Attestation::Attested
        );
        assert_eq!(
            o.attest(Some(&key), ToolName::CrossAgentSend, &a),
            Attestation::Unattested,
            "consumed"
        );
        // Eviction: the oldest is dropped past the cap.
        let mut o = ObservedCalls::new([3; 32], 2);
        let k = |n: u64| InvocationKey::Agy {
            conversation_id: "c".into(),
            step_index: n,
        };
        for n in 0..3 {
            o.record(k(n), ToolName::CrossAgentList, &a);
        }
        assert_eq!(o.len(), 2);
        assert_eq!(
            o.attest(Some(&k(0)), ToolName::CrossAgentList, &a),
            Attestation::Unattested
        );
        assert_eq!(
            o.attest(Some(&k(2)), ToolName::CrossAgentList, &a),
            Attestation::Attested
        );
        // A different salt gives a different digest of the same arguments.
        let o1 = ObservedCalls::new([1; 32], 1);
        let o2 = ObservedCalls::new([2; 32], 1);
        assert_ne!(o1.digest(&a), o2.digest(&a));
        assert!(!format!("{o1:?}").contains("salt"));
    }

    #[test]
    fn agy_attests_every_tool() {
        for t in ToolName::ALL {
            assert!(attestation_required(Driver::Agy, t, false));
            assert!(!attestation_required(Driver::Claude, t, false));
            assert!(!attestation_required(Driver::Codex, t, false));
            assert!(attestation_required(Driver::Claude, t, true));
            assert!(attestation_required(Driver::Codex, t, true));
        }
    }
}
