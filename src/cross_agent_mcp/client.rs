//! A bounded, multiplexed Unix IPC client. A dropped MCP request releases its supervisor
//! wait via `CancelOnDrop`; it never retries a mutation after losing the transport.
use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use agent_core::delegation::env::{ENV_APP_PID, ENV_CAP, ENV_ENDPOINT, ENV_INSTANCE};
use agent_core::delegation::ipc::{
    encode_frame, ApiRange, BridgeInfo, Call, CallOutcome, Frame, Hello, Welcome, MAX_FRAME,
    RECONNECT_DEADLINE_MS, WELCOME_DEADLINE_MS,
};
use agent_core::delegation::{CapabilityToken, ErrorCode, InstanceNonce, ToolError, API_VERSION};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::net::UnixStream;
use tokio::sync::{mpsc, oneshot, watch};

use super::unavailable;

const IO_DEADLINE: Duration = Duration::from_secs(5);
const MAX_INFLIGHT: usize = 4;
const COMMAND_QUEUE: usize = 16;

// Deliberately no Debug: this holds the capability for the single reconnect.
pub(super) struct Config {
    endpoint: PathBuf,
    hello: Hello,
}

impl Config {
    pub(super) fn from_environment() -> Result<Self, ToolError> {
        let read = |key: &str| {
            std::env::var(key)
                .ok()
                .filter(|value| !value.trim().is_empty())
        };
        let endpoint = read(ENV_ENDPOINT)
            .map(PathBuf::from)
            .ok_or_else(|| unavailable("delegation is available only inside agent-terminal"))?;
        if !endpoint.is_absolute() || endpoint.as_os_str().len() > 107 {
            return Err(unavailable("the delegation endpoint is invalid"));
        }
        let app_pid = read(ENV_APP_PID).and_then(|pid| pid.parse::<u32>().ok());
        let parent = parent_pid();
        let grandparent = parent.and_then(|pid| read_ppid(pid).ok());
        if app_pid.is_none() || app_pid == Some(0) || grandparent != app_pid {
            // Claude/Codex process chains are unverified, so their peer checks are advisory.
            // Unknown launchers fail closed, so they cannot turn agy's check into advisory.
            if !parent.is_some_and(known_advisory_parent) {
                return Err(unavailable("the delegation bridge lineage check failed"));
            }
            eprintln!("agent-terminal: delegation bridge lineage check was advisory");
        }
        let instance = read(ENV_INSTANCE)
            .and_then(|value| InstanceNonce::parse(&value).ok())
            .ok_or_else(|| unavailable("the delegation instance is missing or invalid"))?;
        let cap = read(ENV_CAP)
            .and_then(|value| CapabilityToken::parse(&value).ok())
            .ok_or_else(|| unavailable("the delegation capability is missing or invalid"))?;
        Ok(Self {
            endpoint,
            hello: Hello {
                api: ApiRange {
                    min: API_VERSION,
                    max: API_VERSION,
                },
                instance,
                cap,
                bridge: BridgeInfo {
                    version: env!("CARGO_PKG_VERSION").to_owned(),
                    pid: std::process::id(),
                },
            },
        })
    }
}

fn parent_pid() -> Option<u32> {
    // SAFETY: getppid has no preconditions.
    u32::try_from(unsafe { libc::getppid() }).ok()
}

pub(super) fn parse_ppid(stat: &str) -> Option<u32> {
    // comm can contain spaces and parentheses. The state and ppid follow the last ')'.
    stat.rsplit_once(')')?
        .1
        .split_whitespace()
        .nth(1)?
        .parse()
        .ok()
}

fn read_ppid(pid: u32) -> Result<u32, ()> {
    let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).map_err(|_| ())?;
    parse_ppid(&stat).ok_or(())
}

fn known_advisory_parent(pid: u32) -> bool {
    let Ok(executable) = std::fs::read_link(format!("/proc/{pid}/exe")) else {
        return false;
    };
    if executable
        .file_name()
        .is_some_and(|name| matches!(name.to_str(), Some("claude" | "codex")))
    {
        return true;
    }
    if !executable
        .file_name()
        .is_some_and(|name| matches!(name.to_str(), Some("node" | "nodejs")))
    {
        return false;
    }
    // Claude can be a JS entrypoint run by node. Check the executable/script position only,
    // never prompt or profile arguments (which could contain the word 'claude').
    let Ok(argv) = std::fs::read(format!("/proc/{pid}/cmdline")) else {
        return false;
    };
    argv.split(|byte| *byte == 0).nth(1).is_some_and(|arg| {
        let Ok(arg) = std::str::from_utf8(arg) else {
            return false;
        };
        let path = std::path::Path::new(arg);
        path.file_name().is_some_and(|name| name == "cli.js")
            && path
                .components()
                .any(|part| part.as_os_str() == "claude-code")
    })
}

#[derive(Clone)]
enum State {
    Connecting,
    Ready,
    Degraded(ToolError),
}

type Reply = oneshot::Sender<Result<CallOutcome, ToolError>>;

enum Command {
    Call(Call, Reply),
    Cancel(u64),
}

#[derive(Clone)]
pub(super) struct Client {
    commands: mpsc::Sender<Command>,
    state: watch::Receiver<State>,
    next: Arc<AtomicU64>,
}

impl Client {
    pub(super) fn start(config: Result<Config, ToolError>, started: Instant) -> Self {
        let (commands, receiver) = mpsc::channel(COMMAND_QUEUE);
        let (state_tx, state) = watch::channel(State::Connecting);
        tokio::spawn(async move {
            match config {
                Ok(config) => actor(config, started, state_tx, receiver).await,
                Err(error) => {
                    state_tx.send_replace(State::Degraded(error));
                }
            }
        });
        Self {
            commands,
            state,
            next: Arc::new(AtomicU64::new(1)),
        }
    }

    pub(super) fn next_id(&self) -> u64 {
        self.next.fetch_add(1, Ordering::Relaxed)
    }

    pub(super) async fn ready(&self) -> Result<(), ToolError> {
        let mut state = self.state.clone();
        loop {
            match state.borrow_and_update().clone() {
                State::Ready => return Ok(()),
                State::Degraded(error) => return Err(error),
                State::Connecting => {}
            }
            if state.changed().await.is_err() {
                return Err(unavailable("the delegation connection is unavailable"));
            }
        }
    }

    pub(super) async fn call(&self, call: Call) -> Result<CallOutcome, ToolError> {
        let timeout = Duration::from_millis(u64::from(call.timeout_ms));
        tokio::time::timeout(timeout, async {
            self.ready().await?;
            let id = call.id;
            let (reply, result) = oneshot::channel();
            self.commands.try_send(Command::Call(call, reply)).map_err(|_| {
                ToolError::new(ErrorCode::CapacityReached, "the delegation bridge is busy")
            })?;
            let _cancel = CancelOnDrop { id, commands: self.commands.clone() };
            result.await.map_err(|_| lost())?
        }).await.map_err(|_| {
            ToolError::new(ErrorCode::DeadlineExceeded, "the delegation call deadline expired; retry mutations with the same idempotency key")
        })?
    }
}

struct CancelOnDrop {
    id: u64,
    commands: mpsc::Sender<Command>,
}

impl Drop for CancelOnDrop {
    fn drop(&mut self) {
        // At most four calls are admitted; the bounded 16-entry queue has room for their
        // cancellation notices even while a frame write is in progress.
        let _ = self.commands.try_send(Command::Cancel(self.id));
    }
}

fn lost() -> ToolError {
    ToolError::new(
        ErrorCode::TransportLost,
        "the delegation connection was lost; retry mutations with the same idempotency key",
    )
}

async fn connect(config: &Config, deadline: Instant) -> Result<(UnixStream, Welcome), ToolError> {
    tokio::time::timeout_at(tokio::time::Instant::from_std(deadline), async {
        let mut socket = UnixStream::connect(&config.endpoint)
            .await
            .map_err(|_| unavailable("cannot connect to the delegation runtime"))?;
        write_frame(&mut socket, &Frame::Hello(config.hello.clone())).await?;
        match read_frame(&mut socket).await? {
            Frame::Welcome(welcome)
                if welcome.api == API_VERSION
                    && welcome.limits.max_inflight > 0
                    && welcome.limits.max_frame > 0
                    && welcome.limits.max_frame as usize <= MAX_FRAME =>
            {
                Ok((socket, welcome))
            }
            Frame::Refused { error } | Frame::Revoked { error } => Err(error),
            _ => Err(ToolError::new(
                ErrorCode::ProtocolIncompatible,
                "the delegation runtime sent an invalid welcome",
            )),
        }
    })
    .await
    .map_err(|_| unavailable("the delegation welcome deadline expired"))?
}

async fn actor(
    config: Config,
    started: Instant,
    state: watch::Sender<State>,
    mut commands: mpsc::Receiver<Command>,
) {
    let mut deadline = started + Duration::from_millis(WELCOME_DEADLINE_MS);
    let mut reconnect_used = false;
    loop {
        let (socket, welcome) = match connect(&config, deadline).await {
            Ok(connected) => connected,
            Err(error) => {
                state.send_replace(State::Degraded(error));
                return;
            }
        };
        state.send_replace(State::Ready);
        // Agy's capability is single use: spent by this hello, it can never
        // authenticate again, so a reconnect would only present a used credential to be refused.
        let single_use = welcome.principal.driver == agent_core::adapter::Driver::Agy;
        let max_inflight = (welcome.limits.max_inflight as usize).min(MAX_INFLIGHT);
        let (mut reader, mut writer) = socket.into_split();
        let (incoming_tx, mut incoming) = mpsc::channel(1);
        let read_task = tokio::spawn(async move {
            loop {
                let frame = read_frame(&mut reader).await;
                let stop = frame.is_err();
                if incoming_tx.send(frame).await.is_err() || stop {
                    break;
                }
            }
        });
        let mut pending: BTreeMap<u64, Reply> = BTreeMap::new();
        let mut revoked = None;
        let mut shutting_down = false;
        loop {
            tokio::select! {
                command = commands.recv() => match command {
                    None => { shutting_down = true; break; }
                    Some(Command::Call(call, reply)) => {
                        if reply.is_closed() { continue; }
                        if pending.len() >= max_inflight {
                            let _ = reply.send(Err(ToolError::new(ErrorCode::CapacityReached, "four delegation calls are already in flight")));
                            continue;
                        }
                        let id = call.id;
                        pending.insert(id, reply);
                        if write_frame(&mut writer, &Frame::Call(call)).await.is_err() { break; }
                    }
                    Some(Command::Cancel(id)) => {
                        if pending.contains_key(&id)
                            && write_frame(&mut writer, &Frame::Cancel { id }).await.is_err() { break; }
                    }
                },
                frame = incoming.recv() => match frame {
                    Some(Ok(Frame::Result(result))) => {
                        if let Some(reply) = pending.remove(&result.id) {
                            let _ = reply.send(Ok(result.outcome));
                        } else {
                            // A result for an unrecognised id is a protocol violation.
                            break;
                        }
                    }
                    Some(Ok(Frame::Revoked { error })) | Some(Ok(Frame::Refused { error })) => {
                        revoked = Some(error);
                        break;
                    }
                    _ => break,
                }
            }
        }
        read_task.abort();
        drop(writer);
        for (_, reply) in pending {
            let _ = reply.send(Err(lost()));
        }
        if shutting_down {
            return;
        }
        if let Some(error) = revoked {
            state.send_replace(State::Degraded(error));
            return;
        }
        if single_use {
            state.send_replace(State::Degraded(unavailable(
                "the delegation connection was lost; its single-use capability cannot reconnect, \
                 so delegation returns when the agent is rebound before its next turn",
            )));
            return;
        }
        if reconnect_used {
            state.send_replace(State::Degraded(unavailable(
                "the delegation connection was lost after its one reconnect",
            )));
            return;
        }
        reconnect_used = true;
        state.send_replace(State::Connecting);
        deadline = Instant::now() + Duration::from_millis(RECONNECT_DEADLINE_MS);
    }
}

async fn read_frame(reader: &mut (impl AsyncRead + Unpin)) -> Result<Frame, ToolError> {
    let mut header = [0u8; 4];
    reader.read_exact(&mut header).await.map_err(|_| lost())?;
    let length = u32::from_be_bytes(header) as usize;
    if length == 0 || length > MAX_FRAME {
        return Err(lost());
    }
    let mut body = vec![0; length];
    tokio::time::timeout(IO_DEADLINE, reader.read_exact(&mut body))
        .await
        .map_err(|_| lost())?
        .map_err(|_| lost())?;
    serde_json::from_slice(&body).map_err(|_| lost())
}

async fn write_frame(
    writer: &mut (impl AsyncWrite + Unpin),
    frame: &Frame,
) -> Result<(), ToolError> {
    let bytes = encode_frame(frame).map_err(|_| lost())?;
    tokio::time::timeout(IO_DEADLINE, writer.write_all(&bytes))
        .await
        .map_err(|_| lost())?
        .map_err(|_| lost())
}

#[cfg(test)]
pub(super) mod test_support {
    use super::*;

    pub(in crate::cross_agent_mcp) fn config(endpoint: PathBuf) -> Config {
        Config {
            endpoint,
            hello: Hello {
                api: ApiRange { min: 1, max: 1 },
                instance: InstanceNonce::from_bytes([4; 16]),
                cap: CapabilityToken::from_random([7; 32]),
                bridge: BridgeInfo {
                    version: "test".into(),
                    pid: 123,
                },
            },
        }
    }

    pub(in crate::cross_agent_mcp) async fn read(reader: &mut (impl AsyncRead + Unpin)) -> Frame {
        read_frame(reader).await.expect("frame")
    }

    pub(in crate::cross_agent_mcp) async fn write(
        writer: &mut (impl AsyncWrite + Unpin),
        frame: &Frame,
    ) {
        write_frame(writer, frame).await.expect("write")
    }
}
