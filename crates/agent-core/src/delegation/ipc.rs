//! The bridge ↔ supervisor IPC: frames, a bounded decoder, and the server's
//! pure protocol checks. The app owns the socket, the timers and the authentication.

use std::collections::BTreeSet;
use std::fmt;

use serde::{Deserialize, Serialize};
use serde_json::Value;

use super::authority::TaskMode;
use super::correlation::InvocationKey;
use super::error::{ErrorCode, ToolError};
use super::ids::WorkerId;
use super::token::{CapabilityToken, InstanceNonce};
use super::tools::ToolName;
use super::API_VERSION;
use crate::adapter::Driver;

/// The largest frame body, in bytes.
pub const MAX_FRAME: usize = 1 << 20;
/// Bridge side: connect, hello and welcome within this of process start.
pub const WELCOME_DEADLINE_MS: u64 = 2_000;
/// Bridge side: the one reconnect's own deadline, counted from the detected loss.
pub const RECONNECT_DEADLINE_MS: u64 = 2_000;

/// The frame length prefix.
const HEADER: usize = 4;

/// One frame, tagged by `t`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "t", rename_all = "snake_case")]
pub enum Frame {
    /// Bridge → app, first frame.
    Hello(Hello),
    /// App → bridge.
    Welcome(Welcome),
    /// App → bridge, then close.
    Refused { error: ToolError },
    /// Bridge → app.
    Call(Call),
    /// App → bridge.
    Result(CallResult),
    /// Bridge → app: the MCP client cancelled the call (releases a wait; never undoes a
    /// mutation).
    Cancel { id: u64 },
    /// App → bridge: the principal was revoked; then close.
    Revoked { error: ToolError },
}

impl Frame {
    fn tag(&self) -> &'static str {
        match self {
            Frame::Hello(_) => "hello",
            Frame::Welcome(_) => "welcome",
            Frame::Refused { .. } => "refused",
            Frame::Call(_) => "call",
            Frame::Result(_) => "result",
            Frame::Cancel { .. } => "cancel",
            Frame::Revoked { .. } => "revoked",
        }
    }
}

/// The API versions a bridge speaks.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct ApiRange {
    pub min: u32,
    pub max: u32,
}

/// The bridge's hello. Its `Debug` hides the token (through [`CapabilityToken`]'s).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Hello {
    pub api: ApiRange,
    pub instance: InstanceNonce,
    pub cap: CapabilityToken,
    pub bridge: BridgeInfo,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BridgeInfo {
    pub version: String,
    pub pid: u32,
}

/// Who a connection speaks for. `task_mode_ceiling` is informational: the supervisor
/// recomputes it on every call.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Principal {
    pub worker_id: WorkerId,
    pub root_id: WorkerId,
    pub driver: Driver,
    pub depth: u8,
    pub can_spawn: bool,
    pub task_mode_ceiling: Vec<TaskMode>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Welcome {
    pub api: u32,
    pub connection: String,
    pub principal: Principal,
    pub authority_epoch: u64,
    pub limits: IpcLimits,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct IpcLimits {
    pub max_frame: u32,
    pub max_inflight: u32,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Call {
    pub id: u64,
    pub tool: ToolName,
    pub args: Value,
    pub timeout_ms: u32,
    #[serde(default)]
    pub invocation: Option<InvocationKey>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CallResult {
    pub id: u64,
    #[serde(flatten)]
    pub outcome: CallOutcome,
}

/// Serialised as an `"ok"` or `"err"` key.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum CallOutcome {
    #[serde(rename = "ok")]
    Ok(Value),
    #[serde(rename = "err")]
    Err(ToolError),
}

/// Why a frame could not be encoded or decoded. After a decode error the connection closes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FrameError {
    /// A zero length.
    Empty,
    /// A length over [`MAX_FRAME`].
    TooLarge(u32),
    Utf8,
    /// The JSON category and position only, never the input (it may hold the token).
    Json(String),
}

impl fmt::Display for FrameError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            FrameError::Empty => f.write_str("empty frame"),
            FrameError::TooLarge(n) => write!(f, "frame of {n} bytes exceeds {MAX_FRAME}"),
            FrameError::Utf8 => f.write_str("frame is not UTF-8"),
            FrameError::Json(e) => write!(f, "frame is not a valid frame: {e}"),
        }
    }
}

impl std::error::Error for FrameError {}

fn json_error(e: &serde_json::Error) -> FrameError {
    FrameError::Json(format!(
        "{:?} at line {} column {}",
        e.classify(),
        e.line(),
        e.column()
    ))
}

/// The frame as bytes: a big-endian `u32` length, then the JSON. Refuses a body over
/// [`MAX_FRAME`].
pub fn encode_frame(frame: &Frame) -> Result<Vec<u8>, FrameError> {
    let body = serde_json::to_vec(frame).map_err(|e| json_error(&e))?;
    if body.is_empty() {
        return Err(FrameError::Empty);
    }
    if body.len() > MAX_FRAME {
        return Err(FrameError::TooLarge(
            u32::try_from(body.len()).unwrap_or(u32::MAX),
        ));
    }
    let len = u32::try_from(body.len()).map_err(|_| FrameError::TooLarge(u32::MAX))?;
    let mut out = Vec::with_capacity(HEADER + body.len());
    out.extend_from_slice(&len.to_be_bytes());
    out.extend_from_slice(&body);
    Ok(out)
}

/// A decoder that buffers at most one frame: a 4-byte header and one bounded body. The length
/// is checked before anything is reserved; after an error it is poisoned (the caller closes).
/// Its `Debug` shows lengths and state only: a buffered body may be a hello holding the token.
#[derive(Default)]
pub struct FrameDecoder {
    header: [u8; HEADER],
    header_len: usize,
    /// The body length, once the header is complete and accepted.
    body_len: Option<usize>,
    body: Vec<u8>,
    poisoned: Option<FrameError>,
}

impl fmt::Debug for FrameDecoder {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("FrameDecoder")
            .field("header_len", &self.header_len)
            .field("body_len", &self.body_len)
            .field("buffered_body", &self.body.len())
            .field("poisoned", &self.poisoned.is_some())
            .finish()
    }
}

impl FrameDecoder {
    pub fn new() -> Self {
        Self::default()
    }

    fn complete(&self) -> bool {
        self.body_len == Some(self.body.len())
    }

    /// How many more bytes the decoder takes now: 0 while a complete frame is pending (the
    /// caller stops reading until [`FrameDecoder::next_frame`]) or after an error.
    pub fn wants_bytes(&self) -> usize {
        if self.poisoned.is_some() {
            return 0;
        }
        match self.body_len {
            None => HEADER - self.header_len,
            Some(n) => n - self.body.len(),
        }
    }

    /// Buffered bytes (never more than `4 + MAX_FRAME`).
    pub fn buffered(&self) -> usize {
        self.header_len + self.body.len()
    }

    /// Takes bytes up to the end of one frame at most; returns how many were taken.
    pub fn push(&mut self, bytes: &[u8]) -> Result<usize, FrameError> {
        if let Some(e) = &self.poisoned {
            return Err(e.clone());
        }
        let mut taken = 0;
        while taken < bytes.len() && !self.complete() {
            match self.body_len {
                None => {
                    let n = (HEADER - self.header_len).min(bytes.len() - taken);
                    self.header[self.header_len..self.header_len + n]
                        .copy_from_slice(&bytes[taken..taken + n]);
                    self.header_len += n;
                    taken += n;
                    if self.header_len == HEADER {
                        let len = u32::from_be_bytes(self.header);
                        if len == 0 {
                            return Err(self.poison(FrameError::Empty));
                        }
                        if len as usize > MAX_FRAME {
                            return Err(self.poison(FrameError::TooLarge(len)));
                        }
                        // Checked above: at most MAX_FRAME is reserved.
                        self.body = Vec::with_capacity(len as usize);
                        self.body_len = Some(len as usize);
                    }
                }
                Some(len) => {
                    let n = (len - self.body.len()).min(bytes.len() - taken);
                    self.body.extend_from_slice(&bytes[taken..taken + n]);
                    taken += n;
                }
            }
        }
        Ok(taken)
    }

    fn poison(&mut self, e: FrameError) -> FrameError {
        self.poisoned = Some(e.clone());
        self.body = Vec::new();
        self.body_len = None;
        e
    }

    /// The pending frame, once complete.
    pub fn next_frame(&mut self) -> Option<Result<Frame, FrameError>> {
        if self.poisoned.is_some() || !self.complete() {
            return None;
        }
        let body = std::mem::take(&mut self.body);
        self.body_len = None;
        self.header_len = 0;
        let text = match String::from_utf8(body) {
            Ok(t) => t,
            Err(_) => return Some(Err(self.poison(FrameError::Utf8))),
        };
        Some(serde_json::from_str::<Frame>(&text).map_err(|e| self.poison(json_error(&e))))
    }
}

/// Where a server-side connection is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConnPhase {
    PreAuth,
    Authenticated,
}

/// A frame that closes the connection.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProtocolViolation {
    FrameBeforeHello,
    SecondHello,
    DuplicateCallId(u64),
    UnexpectedFrame(&'static str),
}

/// What the server does with an accepted frame.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FrameAction {
    /// Check the hello (instance, token, peer) and answer welcome or refused.
    Authenticate,
    /// Dispatch the call.
    Admit(u64),
    /// Answer `capacity_reached` at once; the call is not queued.
    RejectCapacity(u64),
    CancelCall(u64),
}

/// Pure server-side protocol checks for one connection.
#[derive(Debug, Clone)]
pub struct ConnState {
    phase: ConnPhase,
    pending: BTreeSet<u64>,
}

impl Default for ConnState {
    fn default() -> Self {
        Self::new()
    }
}

impl ConnState {
    pub fn new() -> Self {
        Self {
            phase: ConnPhase::PreAuth,
            pending: BTreeSet::new(),
        }
    }

    pub fn phase(&self) -> ConnPhase {
        self.phase
    }

    pub fn on_frame(
        &mut self,
        f: &Frame,
        inflight_max: u32,
    ) -> Result<FrameAction, ProtocolViolation> {
        match (self.phase, f) {
            (ConnPhase::PreAuth, Frame::Hello(_)) => {
                self.phase = ConnPhase::Authenticated;
                Ok(FrameAction::Authenticate)
            }
            (ConnPhase::PreAuth, _) => Err(ProtocolViolation::FrameBeforeHello),
            (ConnPhase::Authenticated, Frame::Hello(_)) => Err(ProtocolViolation::SecondHello),
            (ConnPhase::Authenticated, Frame::Call(c)) => {
                if self.pending.contains(&c.id) {
                    return Err(ProtocolViolation::DuplicateCallId(c.id));
                }
                if self.pending.len() >= inflight_max as usize {
                    return Ok(FrameAction::RejectCapacity(c.id));
                }
                self.pending.insert(c.id);
                Ok(FrameAction::Admit(c.id))
            }
            (ConnPhase::Authenticated, Frame::Cancel { id }) => Ok(FrameAction::CancelCall(*id)),
            (ConnPhase::Authenticated, other) => {
                Err(ProtocolViolation::UnexpectedFrame(other.tag()))
            }
        }
    }

    /// The call's result was sent: its id may be used again.
    pub fn on_result_sent(&mut self, id: u64) {
        self.pending.remove(&id);
    }
}

/// The negotiated version: 1 if the bridge's range holds it, else `ProtocolIncompatible`.
pub fn negotiate(hello: &ApiRange) -> Result<u32, ErrorCode> {
    if hello.min <= API_VERSION && API_VERSION <= hello.max {
        Ok(API_VERSION)
    } else {
        Err(ErrorCode::ProtocolIncompatible)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn token() -> CapabilityToken {
        CapabilityToken::from_random([7; 32])
    }

    fn hello() -> Frame {
        Frame::Hello(Hello {
            api: ApiRange { min: 1, max: 1 },
            instance: InstanceNonce::from_bytes([1; 16]),
            cap: token(),
            bridge: BridgeInfo {
                version: "3.6.0".into(),
                pid: 1234,
            },
        })
    }

    fn call(id: u64) -> Frame {
        Frame::Call(Call {
            id,
            tool: ToolName::CrossAgentSpawn,
            args: json!({"a": 1}),
            timeout_ms: 30_000,
            invocation: Some(InvocationKey::Claude {
                tool_use_id: "toolu_1".into(),
            }),
        })
    }

    fn all_frames() -> Vec<Frame> {
        let principal = Principal {
            worker_id: WorkerId::parse("w_abcdefgh").expect("valid"),
            root_id: WorkerId::parse("w_abcdefgh").expect("valid"),
            driver: Driver::Agy,
            depth: 0,
            can_spawn: true,
            task_mode_ceiling: vec![TaskMode::Ask, TaskMode::Plan],
        };
        vec![
            hello(),
            Frame::Welcome(Welcome {
                api: 1,
                connection: "cn_1".into(),
                principal,
                authority_epoch: 4,
                limits: IpcLimits {
                    max_frame: MAX_FRAME as u32,
                    max_inflight: 4,
                },
            }),
            Frame::Refused {
                error: ToolError::new(ErrorCode::AuthenticationFailed, "no"),
            },
            call(7),
            Frame::Result(CallResult {
                id: 7,
                outcome: CallOutcome::Ok(json!({"x": 1})),
            }),
            Frame::Result(CallResult {
                id: 8,
                outcome: CallOutcome::Err(ToolError::new(ErrorCode::NotFound, "gone")),
            }),
            Frame::Cancel { id: 7 },
            Frame::Revoked {
                error: ToolError::new(ErrorCode::RuntimeUnavailable, "bye"),
            },
        ]
    }

    #[test]
    fn frames_round_trip_with_their_documented_shapes() {
        for f in all_frames() {
            let bytes = encode_frame(&f).expect("encode");
            let mut d = FrameDecoder::new();
            assert_eq!(d.push(&bytes), Ok(bytes.len()));
            assert_eq!(d.next_frame(), Some(Ok(f)));
        }
        let ok = serde_json::to_value(Frame::Result(CallResult {
            id: 7,
            outcome: CallOutcome::Ok(json!({})),
        }))
        .expect("ser");
        assert_eq!(ok, json!({"t": "result", "id": 7, "ok": {}}));
        let h = serde_json::to_value(hello()).expect("ser");
        assert_eq!(h["t"], "hello");
        assert_eq!(h["api"], json!({"min": 1, "max": 1}));
        assert_eq!(h["cap"], json!(token().expose_secret()));
        let c = serde_json::to_value(call(7)).expect("ser");
        assert_eq!(
            c["invocation"],
            json!({"provider": "claude", "tool_use_id": "toolu_1"})
        );
    }

    #[test]
    fn decoder_handles_byte_splits_and_several_frames() {
        let a = encode_frame(&call(1)).expect("a");
        let b = encode_frame(&Frame::Cancel { id: 1 }).expect("b");
        let mut d = FrameDecoder::new();
        for byte in &a {
            assert!(d.wants_bytes() > 0);
            assert_eq!(d.push(std::slice::from_ref(byte)), Ok(1));
        }
        assert_eq!(d.wants_bytes(), 0);
        assert_eq!(d.next_frame(), Some(Ok(call(1))));
        let mut both = a.clone();
        both.extend_from_slice(&b);
        let taken = d.push(&both).expect("push");
        assert_eq!(taken, a.len(), "takes only the first frame");
        assert_eq!(d.wants_bytes(), 0);
        assert_eq!(
            d.push(&both[taken..]),
            Ok(0),
            "nothing more while one is pending"
        );
        assert_eq!(d.next_frame(), Some(Ok(call(1))));
        assert_eq!(d.push(&both[taken..]), Ok(b.len()));
        assert_eq!(d.next_frame(), Some(Ok(Frame::Cancel { id: 1 })));
        assert_eq!(d.next_frame(), None);
    }

    #[test]
    fn zero_and_oversized_lengths_poison_before_any_allocation() {
        let mut d = FrameDecoder::new();
        assert_eq!(d.push(&[0, 0, 0, 0]), Err(FrameError::Empty));
        assert_eq!(d.wants_bytes(), 0);
        assert!(d.push(&[1]).is_err());
        let too_big = (MAX_FRAME as u32 + 1).to_be_bytes();
        let mut d = FrameDecoder::new();
        assert_eq!(
            d.push(&too_big),
            Err(FrameError::TooLarge(MAX_FRAME as u32 + 1))
        );
        assert_eq!(d.body.capacity(), 0, "no buffer grew");
        assert_eq!(d.next_frame(), None);
        assert!(d.push(b"{}").is_err(), "poisoned");
        // The largest legal frame buffers at most 4 + MAX_FRAME.
        let mut d = FrameDecoder::new();
        let mut max = (MAX_FRAME as u32).to_be_bytes().to_vec();
        max.extend(std::iter::repeat_n(b' ', MAX_FRAME + 10));
        let taken = d.push(&max).expect("push");
        assert_eq!(taken, HEADER + MAX_FRAME);
        assert!(d.buffered() <= HEADER + MAX_FRAME);
        assert!(d.body.capacity() <= MAX_FRAME);
    }

    #[test]
    fn encode_refuses_an_oversized_frame() {
        let big = Frame::Result(CallResult {
            id: 1,
            outcome: CallOutcome::Ok(json!("x".repeat(MAX_FRAME))),
        });
        assert!(matches!(encode_frame(&big), Err(FrameError::TooLarge(_))));
    }

    #[test]
    fn invalid_utf8_and_json_are_errors() {
        let mut d = FrameDecoder::new();
        let mut bytes = 2u32.to_be_bytes().to_vec();
        bytes.extend_from_slice(&[0xff, 0xfe]);
        d.push(&bytes).expect("push");
        assert_eq!(d.next_frame(), Some(Err(FrameError::Utf8)));
        assert_eq!(d.wants_bytes(), 0);
        let secret = token();
        let body = format!("{{\"t\":\"hello\",\"cap\":\"{}\"", secret.expose_secret());
        let mut bytes = (body.len() as u32).to_be_bytes().to_vec();
        bytes.extend_from_slice(body.as_bytes());
        let mut d = FrameDecoder::new();
        d.push(&bytes).expect("push");
        match d.next_frame() {
            Some(Err(FrameError::Json(msg))) => {
                assert!(!msg.contains(&secret.expose_secret()[5..]))
            }
            other => panic!("expected a JSON error, got {other:?}"),
        }
        let unknown = br#"{"t":"bogus"}"#;
        let mut bytes = (unknown.len() as u32).to_be_bytes().to_vec();
        bytes.extend_from_slice(unknown);
        let mut d = FrameDecoder::new();
        d.push(&bytes).expect("push");
        assert!(matches!(d.next_frame(), Some(Err(FrameError::Json(_)))));
    }

    #[test]
    fn hello_debug_hides_the_token() {
        let dbg = format!("{:?}", hello());
        assert!(!dbg.contains(&token().expose_secret()[5..]));
        assert!(dbg.contains("atdc_****"));
    }

    /// No representation of the token's bytes may appear: not the text, not the bytes as
    /// decimal numbers.
    fn leaks_token(dbg: &str) -> bool {
        let secret = token();
        let s = secret.expose_secret();
        let decimal: Vec<String> = s.bytes().take(12).map(|b| b.to_string()).collect();
        dbg.contains(&s[5..]) || dbg.contains(&decimal.join(", ")) || dbg.contains("atdc")
    }

    #[test]
    fn decoder_debug_never_shows_a_buffered_hello() {
        let bytes = encode_frame(&hello()).expect("encode");
        let mut partial = FrameDecoder::new();
        partial.push(&bytes[..bytes.len() - 3]).expect("push");
        let dbg = format!("{partial:?}");
        assert!(!leaks_token(&dbg), "{dbg}");
        assert!(dbg.contains("buffered_body"));
        let mut complete = FrameDecoder::new();
        complete.push(&bytes).expect("push");
        assert_eq!(complete.wants_bytes(), 0);
        let dbg = format!("{complete:?}");
        assert!(!leaks_token(&dbg), "{dbg}");
    }

    #[test]
    fn negotiation_accepts_only_version_one() {
        assert_eq!(negotiate(&ApiRange { min: 1, max: 1 }), Ok(1));
        assert_eq!(negotiate(&ApiRange { min: 0, max: 3 }), Ok(1));
        assert_eq!(
            negotiate(&ApiRange { min: 2, max: 3 }),
            Err(ErrorCode::ProtocolIncompatible)
        );
        assert_eq!(
            negotiate(&ApiRange { min: 3, max: 1 }),
            Err(ErrorCode::ProtocolIncompatible)
        );
    }

    #[test]
    fn conn_state_enforces_the_protocol() {
        let mut s = ConnState::new();
        assert_eq!(
            s.on_frame(&call(1), 4),
            Err(ProtocolViolation::FrameBeforeHello)
        );
        assert_eq!(s.phase(), ConnPhase::PreAuth);
        assert_eq!(s.on_frame(&hello(), 4), Ok(FrameAction::Authenticate));
        assert_eq!(s.on_frame(&hello(), 4), Err(ProtocolViolation::SecondHello));
        for id in 1..=4 {
            assert_eq!(s.on_frame(&call(id), 4), Ok(FrameAction::Admit(id)));
        }
        assert_eq!(
            s.on_frame(&call(2), 4),
            Err(ProtocolViolation::DuplicateCallId(2))
        );
        assert_eq!(s.on_frame(&call(5), 4), Ok(FrameAction::RejectCapacity(5)));
        assert_eq!(
            s.on_frame(&Frame::Cancel { id: 3 }, 4),
            Ok(FrameAction::CancelCall(3))
        );
        s.on_result_sent(2);
        assert_eq!(s.on_frame(&call(2), 4), Ok(FrameAction::Admit(2)));
        let welcome = all_frames().remove(1);
        assert_eq!(
            s.on_frame(&welcome, 4),
            Err(ProtocolViolation::UnexpectedFrame("welcome"))
        );
    }
}
