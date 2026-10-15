//! Invocation keys and attestation (contract §2 "Attestation arguments", §4).
//!
//! A bridge call is attested when its key matches an unconsumed call to `agent_terminal` seen
//! in the principal's own provider stream, with the same tool and the same attestation
//! arguments. Attestation never grants authority: the caller only denies on `Unattested`.

use std::collections::VecDeque;
use std::fmt;

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

const SIP_ROUNDS_C: usize = 2;
const SIP_ROUNDS_D: usize = 4;

#[inline]
fn sip_round(v: &mut [u64; 4]) {
    v[0] = v[0].wrapping_add(v[1]);
    v[1] = v[1].rotate_left(13);
    v[1] ^= v[0];
    v[0] = v[0].rotate_left(32);
    v[2] = v[2].wrapping_add(v[3]);
    v[3] = v[3].rotate_left(16);
    v[3] ^= v[2];
    v[0] = v[0].wrapping_add(v[3]);
    v[3] = v[3].rotate_left(21);
    v[3] ^= v[0];
    v[2] = v[2].wrapping_add(v[1]);
    v[1] = v[1].rotate_left(17);
    v[1] ^= v[2];
    v[2] = v[2].rotate_left(32);
}

fn le_u64(bytes: &[u8]) -> u64 {
    let mut word = [0u8; 8];
    word[..bytes.len()].copy_from_slice(bytes);
    u64::from_le_bytes(word)
}

/// Keyed SipHash-2-4 as in the reference implementation (Aumasson & Bernstein,
/// `veorq/SipHash`), with a 64-bit (`out128 == false`) or 128-bit output, little-endian.
fn siphash24(key: &[u8; 16], msg: &[u8], out128: bool) -> Vec<u8> {
    let k0 = le_u64(&key[..8]);
    let k1 = le_u64(&key[8..]);
    let mut v = [
        k0 ^ 0x736f_6d65_7073_6575,
        k1 ^ 0x646f_7261_6e64_6f6d,
        k0 ^ 0x6c79_6765_6e65_7261,
        k1 ^ 0x7465_6462_7974_6573,
    ];
    if out128 {
        v[1] ^= 0xee;
    }
    let (words, tail) = msg.as_chunks::<8>();
    for word in words {
        let m = u64::from_le_bytes(*word);
        v[3] ^= m;
        for _ in 0..SIP_ROUNDS_C {
            sip_round(&mut v);
        }
        v[0] ^= m;
    }
    let b = ((msg.len() as u64) << 56) | le_u64(tail);
    v[3] ^= b;
    for _ in 0..SIP_ROUNDS_C {
        sip_round(&mut v);
    }
    v[0] ^= b;
    v[2] ^= if out128 { 0xee } else { 0xff };
    for _ in 0..SIP_ROUNDS_D {
        sip_round(&mut v);
    }
    let mut out = (v[0] ^ v[1] ^ v[2] ^ v[3]).to_le_bytes().to_vec();
    if out128 {
        v[1] ^= 0xdd;
        for _ in 0..SIP_ROUNDS_D {
            sip_round(&mut v);
        }
        out.extend_from_slice(&(v[0] ^ v[1] ^ v[2] ^ v[3]).to_le_bytes());
    }
    out
}

/// SipHash-2-4-128 of `msg` under `key`.
pub(crate) fn siphash24_128(key: &[u8; 16], msg: &[u8]) -> [u8; 16] {
    let mut out = [0u8; 16];
    out.copy_from_slice(&siphash24(key, msg, true));
    out
}

/// One principal generation's observed native calls to `agent_terminal`, newest last. Each
/// keeps its key, its tool and a 128-bit digest of its attestation arguments: keyed
/// SipHash-2-4-128 (hand-rolled to the reference specification), key = the caller's 16-byte
/// salt. Never persisted or logged.
pub struct ObservedCalls {
    salt: [u8; 16],
    cap: usize,
    calls: VecDeque<(InvocationKey, ToolName, [u8; 16])>,
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
    /// `salt`: 16 random bytes from the app (the SipHash key), one per principal generation.
    pub fn new(salt: [u8; 16], cap: usize) -> Self {
        Self {
            salt,
            cap: cap.max(1),
            calls: VecDeque::new(),
        }
    }

    fn digest(&self, args: &AttestationArgs) -> [u8; 16] {
        siphash24_128(&self.salt, args.0.as_bytes())
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
        let mut o = ObservedCalls::new([3; 16], 4);
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
        let mut o = ObservedCalls::new([3; 16], 2);
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
        let o1 = ObservedCalls::new([1; 16], 1);
        let o2 = ObservedCalls::new([2; 16], 1);
        assert_ne!(o1.digest(&a), o2.digest(&a));
        assert!(!format!("{o1:?}").contains("salt"));
    }

    /// Reference vectors (`veorq/SipHash` `vectors.h`, also the paper's appendix): key
    /// 00..0f, message 00..(n-1).
    #[test]
    fn siphash_matches_the_reference_vectors() {
        let key: [u8; 16] = std::array::from_fn(|i| i as u8);
        let msg: Vec<u8> = (0u8..64).collect();
        let sip64: [(usize, [u8; 8]); 6] = [
            (0, [0x31, 0x0e, 0x0e, 0xdd, 0x47, 0xdb, 0x6f, 0x72]),
            (1, [0xfd, 0x67, 0xdc, 0x93, 0xc5, 0x39, 0xf8, 0x74]),
            (2, [0x5a, 0x4f, 0xa9, 0xd9, 0x09, 0x80, 0x6c, 0x0d]),
            (3, [0x2d, 0x7e, 0xfb, 0xd7, 0x96, 0x66, 0x67, 0x85]),
            // The paper prints this one as the u64 0xa129ca6149be45e5; vectors.h holds its
            // little-endian bytes.
            (15, [0xe5, 0x45, 0xbe, 0x49, 0x61, 0xca, 0x29, 0xa1]),
            (63, [0x72, 0x45, 0x06, 0xeb, 0x4c, 0x32, 0x8a, 0x95]),
        ];
        for (n, expected) in sip64 {
            assert_eq!(
                siphash24(&key, &msg[..n], false),
                expected.to_vec(),
                "64-bit, n={n}"
            );
        }
        let sip128: [(usize, [u8; 16]); 3] = [
            (
                0,
                [
                    0xa3, 0x81, 0x7f, 0x04, 0xba, 0x25, 0xa8, 0xe6, 0x6d, 0xf6, 0x72, 0x14, 0xc7,
                    0x55, 0x02, 0x93,
                ],
            ),
            (
                1,
                [
                    0xda, 0x87, 0xc1, 0xd8, 0x6b, 0x99, 0xaf, 0x44, 0x34, 0x76, 0x59, 0x11, 0x9b,
                    0x22, 0xfc, 0x45,
                ],
            ),
            (
                2,
                [
                    0x81, 0x77, 0x22, 0x8d, 0xa4, 0xa4, 0x5d, 0xc7, 0xfc, 0xa3, 0x8b, 0xde, 0xf6,
                    0x0a, 0xff, 0xe4,
                ],
            ),
        ];
        for (n, expected) in sip128 {
            assert_eq!(siphash24_128(&key, &msg[..n]), expected, "128-bit, n={n}");
        }
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
