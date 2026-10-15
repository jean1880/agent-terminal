//! Lane C: one connection's state machine (contract §4, P4 §4.2): the hello deadline, bounded
//! frame reads, the outbox and its write deadline.
//!
//! ```text
//! PreAuth --hello ≤ hello deadline--> Authenticating --Welcome--> Authenticated
//!    |                                     `--Refused (flush ≤ write deadline) → Closed
//!    `-- other frame / deadline / bad length → Closed
//! Authenticated --revoke--> Draining (flush `revoked` ≤ write deadline) → Closed
//! ```
//!
//! Reads take at most what the P1 [`FrameDecoder`] wants, so a length is checked before any
//! body is buffered and at most one frame is ever held. Each connection has a reader task, a
//! hello timer, and a write pump while its outbox is non-empty; closing aborts all three.

use std::cell::{Cell, RefCell};
use std::collections::VecDeque;
use std::rc::Rc;
use std::time::{Duration, Instant};

use agent_core::delegation::ipc::{
    encode_frame, CallOutcome, CallResult, ConnState, Frame, FrameAction, FrameDecoder,
};
use agent_core::delegation::{ErrorCode, ToolError};
use gtk4::gio;
use gtk4::gio::prelude::*;
use gtk4::glib;
use tracing::debug;

use super::IpcServer;
use crate::delegation::seam::{ConnId, PeerFacts};

/// The most read from the socket at once.
const READ_CHUNK: usize = 64 * 1024;
/// The frame length prefix (P1 `ipc::HEADER`).
const HEADER: usize = 4;
const PRIORITY: glib::Priority = glib::Priority::DEFAULT;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Phase {
    PreAuth,
    /// The hello is with the dispatcher; nothing more is read until it answers.
    Authenticating,
    Authenticated,
    /// A final `refused` or `revoked` is being flushed; nothing more is read or queued.
    Draining,
    Closed,
}

/// Why an [`Outbox`] refused a frame: the connection closes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct Overflow;

/// The outbound queue: at most `max_frames` frames and `max_bytes` bytes.
#[derive(Debug)]
pub(super) struct Outbox {
    frames: VecDeque<Vec<u8>>,
    bytes: usize,
    max_frames: usize,
    max_bytes: usize,
}

impl Outbox {
    pub(super) fn new(max_frames: u32, max_bytes: u32) -> Self {
        Self {
            frames: VecDeque::new(),
            bytes: 0,
            max_frames: max_frames as usize,
            max_bytes: max_bytes as usize,
        }
    }

    pub(super) fn push(&mut self, frame: Vec<u8>) -> Result<(), Overflow> {
        if self.frames.len() >= self.max_frames
            || self.bytes.saturating_add(frame.len()) > self.max_bytes
        {
            return Err(Overflow);
        }
        self.bytes += frame.len();
        self.frames.push_back(frame);
        Ok(())
    }

    pub(super) fn pop(&mut self) -> Option<Vec<u8>> {
        let f = self.frames.pop_front()?;
        self.bytes -= f.len();
        Some(f)
    }

    fn clear(&mut self) {
        self.frames.clear();
        self.bytes = 0;
    }
}

pub(super) struct Conn {
    id: ConnId,
    io: gio::SocketConnection,
    server: std::rc::Weak<IpcServer>,
    peer: PeerFacts,
    phase: Cell<Phase>,
    proto: RefCell<ConnState>,
    /// The dispatcher has seen this connection (its hello went to `authenticate`), so it is
    /// told when the connection closes.
    known: Cell<bool>,
    outbox: RefCell<Outbox>,
    reader: RefCell<Option<glib::JoinHandle<()>>>,
    hello_timer: RefCell<Option<glib::JoinHandle<()>>>,
    pump: RefCell<Option<glib::JoinHandle<()>>>,
}

impl Conn {
    /// Counted against the pre-auth cap until its welcome is queued.
    pub(super) fn unauthenticated(&self) -> bool {
        matches!(self.phase.get(), Phase::PreAuth | Phase::Authenticating)
    }

    fn abort_tasks(&self) {
        for slot in [&self.reader, &self.hello_timer, &self.pump] {
            if let Some(task) = slot.borrow_mut().take() {
                task.abort();
            }
        }
    }

    /// Closes without telling anyone (the server is being dropped).
    pub(super) fn teardown(&self) {
        if self.phase.replace(Phase::Closed) == Phase::Closed {
            return;
        }
        self.abort_tasks();
        self.outbox.borrow_mut().clear();
        close_io(&self.io);
    }
}

/// Shuts the socket down and closes it. Never blocks: a socket close is a local fd operation.
pub(super) fn close_io(io: &gio::SocketConnection) {
    let socket = io.socket();
    let _ = socket.shutdown(true, true);
    if let Err(e) = socket.close() {
        debug!(error = %e.message(), "delegation socket close failed");
    }
}

/// The peer's pid from `SO_PEERCRED`, through `gio::Credentials`.
fn peer_facts(io: &gio::SocketConnection) -> PeerFacts {
    let pid = io
        .socket()
        .credentials()
        .ok()
        .and_then(|c| c.unix_pid().ok())
        .and_then(|p| u32::try_from(p).ok())
        .filter(|p| *p != 0);
    PeerFacts { pid }
}

/// Registers an admitted connection and starts its reader and hello timer.
pub(super) fn start(server: &Rc<IpcServer>, id: ConnId, io: gio::SocketConnection) {
    let limits = server.limits;
    let conn = Rc::new(Conn {
        id,
        peer: peer_facts(&io),
        io,
        server: Rc::downgrade(server),
        phase: Cell::new(Phase::PreAuth),
        proto: RefCell::new(ConnState::new()),
        known: Cell::new(false),
        outbox: RefCell::new(Outbox::new(
            limits.outbound_queue_frames,
            limits.outbound_queue_bytes,
        )),
        reader: RefCell::new(None),
        hello_timer: RefCell::new(None),
        pump: RefCell::new(None),
    });
    debug!(conn = id.0, "delegation connection accepted");
    server.conns.borrow_mut().insert(id, conn.clone());
    let reader = glib::spawn_future_local(read_loop(conn.clone()));
    *conn.reader.borrow_mut() = Some(reader);
    let weak = Rc::downgrade(&conn);
    let hello = Duration::from_millis(limits.hello_deadline_ms);
    let timer = glib::spawn_future_local(async move {
        glib::timeout_future(hello).await;
        if let Some(conn) = weak.upgrade() {
            if conn.phase.get() == Phase::PreAuth {
                close(&conn, "no hello before the deadline");
            }
        }
    });
    *conn.hello_timer.borrow_mut() = Some(timer);
}

/// Closes the connection now: aborts its tasks, drops its outbox, removes it from the server,
/// and tells the dispatcher if it ever saw it. Idempotent.
fn close(conn: &Rc<Conn>, why: &'static str) {
    if conn.phase.replace(Phase::Closed) == Phase::Closed {
        return;
    }
    debug!(conn = conn.id.0, why, "delegation connection closed");
    conn.abort_tasks();
    conn.outbox.borrow_mut().clear();
    close_io(&conn.io);
    if let Some(server) = conn.server.upgrade() {
        server.conns.borrow_mut().remove(&conn.id);
        if conn.known.get() {
            server.dispatcher.closed(conn.id);
        }
    }
}

/// Sends `revoked`, then closes once it is flushed (or the write deadline passes).
pub(super) fn revoke(_server: &IpcServer, conn: &Rc<Conn>, error: ToolError) {
    drain(conn, Frame::Revoked { error });
}

/// Stops reading, queues one final frame, and closes when the outbox is empty.
fn drain(conn: &Rc<Conn>, last: Frame) {
    if matches!(conn.phase.get(), Phase::Closed | Phase::Draining) {
        return;
    }
    conn.phase.set(Phase::Draining);
    for slot in [&conn.reader, &conn.hello_timer] {
        if let Some(task) = slot.borrow_mut().take() {
            task.abort();
        }
    }
    let Some(bytes) = encode(last) else {
        return close(conn, "final frame could not be encoded");
    };
    if conn.outbox.borrow_mut().push(bytes).is_err() {
        return close(conn, "outbound queue overflow");
    }
    ensure_pump(conn);
}

/// Queues a frame on an authenticated connection; an overflow closes it. False when the
/// connection is (now) closed or draining.
fn send(conn: &Rc<Conn>, frame: Frame) -> bool {
    if conn.phase.get() != Phase::Authenticated {
        return false;
    }
    let Some(bytes) = encode(frame) else {
        close(conn, "frame could not be encoded");
        return false;
    };
    if conn.outbox.borrow_mut().push(bytes).is_err() {
        close(conn, "outbound queue overflow");
        return false;
    }
    ensure_pump(conn);
    true
}

/// The frame's bytes. A result too large to frame becomes an `internal_error` result: the
/// call still gets an answer.
fn encode(frame: Frame) -> Option<Vec<u8>> {
    match encode_frame(&frame) {
        Ok(b) => Some(b),
        Err(e) => {
            debug!(error = %e, "delegation frame could not be encoded");
            let Frame::Result(CallResult { id, .. }) = frame else {
                return None;
            };
            encode_frame(&Frame::Result(CallResult {
                id,
                outcome: CallOutcome::Err(ToolError::new(
                    ErrorCode::InternalError,
                    "the result could not be framed",
                )),
            }))
            .ok()
        }
    }
}

fn ensure_pump(conn: &Rc<Conn>) {
    if conn.pump.borrow().is_some() {
        return;
    }
    let Some(server) = conn.server.upgrade() else {
        return close(conn, "server gone");
    };
    let deadline = Duration::from_millis(server.limits.write_deadline_ms);
    drop(server);
    let task = glib::spawn_future_local(pump(conn.clone(), deadline));
    *conn.pump.borrow_mut() = Some(task);
}

/// Writes the outbox in order, each frame within `deadline`. A failure or a missed deadline
/// closes the connection; a draining connection closes once the outbox is empty.
async fn pump(conn: Rc<Conn>, deadline: Duration) {
    let out = conn.io.output_stream();
    loop {
        let next = conn.outbox.borrow_mut().pop();
        let Some(bytes) = next else { break };
        match glib::future_with_timeout(deadline, out.write_all_future(bytes, PRIORITY)).await {
            Ok(Ok((bytes, written, None))) if written == bytes.len() => {}
            Ok(_) => return close(&conn, "write failed"),
            Err(_) => return close(&conn, "write deadline missed"),
        }
    }
    // Detach this task's own handle; a later frame starts a new pump.
    drop(conn.pump.borrow_mut().take());
    if conn.phase.get() == Phase::Draining {
        close(&conn, "final frame flushed");
    }
}

/// Reads frames one at a time and acts on each. Ends by closing the connection, or when a
/// final frame is draining.
async fn read_loop(conn: Rc<Conn>) {
    let input = conn.io.input_stream();
    let mut decoder = FrameDecoder::new();
    let mut buf: Vec<u8> = Vec::new();
    let mut body_deadline: Option<Instant> = None;
    loop {
        if decoder.wants_bytes() == 0 {
            body_deadline = None;
            let frame = match decoder.next_frame() {
                Some(Ok(frame)) => frame,
                Some(Err(e)) => {
                    debug!(conn = conn.id.0, error = %e, "bad delegation frame");
                    return close(&conn, "bad frame");
                }
                None => return close(&conn, "decoder poisoned"),
            };
            if !on_frame(&conn, frame).await {
                return;
            }
            continue;
        }
        let Some(server) = conn.server.upgrade() else {
            return conn.teardown();
        };
        let body_limit = Duration::from_millis(server.limits.frame_body_deadline_ms);
        drop(server);
        let mut chunk = std::mem::take(&mut buf);
        chunk.resize(decoder.wants_bytes().min(READ_CHUNK), 0);
        let read = input.read_future(chunk, PRIORITY);
        let result = match body_deadline {
            Some(at) => {
                let left = at.saturating_duration_since(Instant::now());
                match glib::future_with_timeout(left, read).await {
                    Ok(r) => r,
                    Err(_) => return close(&conn, "frame body deadline missed"),
                }
            }
            None => read.await,
        };
        match result {
            Ok((_, 0)) => return close(&conn, "peer closed"),
            Ok((b, n)) => {
                buf = b;
                if let Err(e) = decoder.push(&buf[..n]) {
                    debug!(conn = conn.id.0, error = %e, "bad delegation frame length");
                    return close(&conn, "bad frame length");
                }
                if body_deadline.is_none()
                    && decoder.buffered() >= HEADER
                    && decoder.wants_bytes() > 0
                {
                    body_deadline = Some(Instant::now() + body_limit);
                }
            }
            Err((_, e)) => {
                debug!(conn = conn.id.0, error = %e.message(), "delegation read failed");
                return close(&conn, "read failed");
            }
        }
    }
}

/// Acts on one decoded frame. False: stop reading (closed or draining).
async fn on_frame(conn: &Rc<Conn>, frame: Frame) -> bool {
    let Some(server) = conn.server.upgrade() else {
        conn.teardown();
        return false;
    };
    let action = conn
        .proto
        .borrow_mut()
        .on_frame(&frame, server.limits.max_inflight);
    let action = match action {
        Ok(a) => a,
        Err(v) => {
            debug!(conn = conn.id.0, violation = ?v, "delegation protocol violation");
            drop(server);
            close(conn, "protocol violation");
            return false;
        }
    };
    match (action, frame) {
        (FrameAction::Authenticate, Frame::Hello(hello)) => {
            conn.phase.set(Phase::Authenticating);
            if let Some(timer) = conn.hello_timer.borrow_mut().take() {
                timer.abort();
            }
            conn.known.set(true);
            let verdict = server.dispatcher.authenticate(conn.id, hello, conn.peer);
            // Never hold the server across an await: its `Drop` must stay possible.
            drop(server);
            let verdict = verdict.await;
            if conn.phase.get() != Phase::Authenticating {
                return false;
            }
            match verdict {
                Ok(welcome) => {
                    conn.phase.set(Phase::Authenticated);
                    send(conn, Frame::Welcome(welcome))
                }
                Err(error) => {
                    drain(conn, Frame::Refused { error });
                    false
                }
            }
        }
        (FrameAction::Admit(id), Frame::Call(call)) => {
            let outcome = server.dispatcher.call(conn.id, call);
            drop(server);
            let weak = Rc::downgrade(conn);
            glib::spawn_future_local(async move {
                let outcome = outcome.await;
                // A closed connection drops the result; the call itself has done its work.
                let Some(conn) = weak.upgrade() else { return };
                if conn.phase.get() != Phase::Authenticated {
                    return;
                }
                conn.proto.borrow_mut().on_result_sent(id);
                send(&conn, Frame::Result(CallResult { id, outcome }));
            });
            conn.phase.get() == Phase::Authenticated
        }
        (FrameAction::RejectCapacity(id), _) => {
            drop(server);
            let error = ToolError::new(
                ErrorCode::CapacityReached,
                "too many calls in flight on this connection",
            )
            .with_detail("limit", "max_inflight");
            send(
                conn,
                Frame::Result(CallResult {
                    id,
                    outcome: CallOutcome::Err(error),
                }),
            )
        }
        (FrameAction::CancelCall(id), _) => {
            let dispatcher = server.dispatcher.clone();
            drop(server);
            dispatcher.cancel(conn.id, id);
            conn.phase.get() == Phase::Authenticated
        }
        // `ConnState` pairs each action with its frame; anything else is a broken invariant.
        _ => {
            drop(server);
            close(conn, "frame and action disagree");
            false
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn outbox_bounds_frames_and_bytes() {
        let mut o = Outbox::new(2, 10);
        assert_eq!(o.push(vec![0; 4]), Ok(()));
        assert_eq!(o.push(vec![0; 6]), Ok(()));
        assert_eq!(o.push(vec![0; 1]), Err(Overflow), "frame cap");
        assert_eq!(o.pop().map(|f| f.len()), Some(4));
        assert_eq!(o.push(vec![0; 5]), Err(Overflow), "byte cap: 6 + 5 > 10");
        assert_eq!(o.push(vec![0; 4]), Ok(()));
        assert_eq!(o.pop().map(|f| f.len()), Some(6));
        assert_eq!(o.pop().map(|f| f.len()), Some(4));
        assert_eq!(o.pop(), None);
        assert_eq!(o.bytes, 0);
    }

    #[test]
    fn an_unframeable_result_becomes_an_internal_error() {
        let big = Frame::Result(CallResult {
            id: 9,
            outcome: CallOutcome::Ok(serde_json::json!(
                "x".repeat(agent_core::delegation::ipc::MAX_FRAME)
            )),
        });
        let bytes = encode(big).expect("fallback");
        let mut d = FrameDecoder::new();
        d.push(&bytes).expect("push");
        match d.next_frame() {
            Some(Ok(Frame::Result(CallResult {
                id: 9,
                outcome: CallOutcome::Err(e),
            }))) => assert_eq!(e.code, ErrorCode::InternalError),
            other => panic!("unexpected {other:?}"),
        }
    }
}
