//! IPC server tests: real sockets in a temp runtime directory, a fake dispatcher, and the test's
//! own main context driven by `pump_until`. The client is a non-blocking `UnixStream` on the
//! test thread, so nothing waits on the loop it is meant to drive.

use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::io::{ErrorKind, Read, Write};
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::UnixStream;
use std::path::Path;
use std::rc::Rc;
use std::time::Duration;

use agent_core::adapter::Driver;
use agent_core::delegation::ipc::{
    encode_frame, ApiRange, BridgeInfo, Call, CallOutcome, CallResult, Frame, FrameDecoder, Hello,
    IpcLimits, Principal, Welcome, MAX_FRAME,
};
use agent_core::delegation::{
    CapabilityToken, ErrorCode, InstanceNonce, Limits, ToolError, ToolName, WorkerId,
};
use gtk4::glib;
use serde_json::json;

use super::{leftover_verdict, Found, IpcServer, Leftover};
use crate::delegation::seam::{CallDispatcher, ConnId, LocalFuture, PeerFacts, RuntimeDir};
use crate::testutil::{in_loop, pump_until};

// ---- fake dispatcher ---------------------------------------------------------------------------

/// How the fake answers a call.
#[derive(Clone)]
enum Answer {
    Echo,
    /// A result of about this many bytes.
    Big(usize),
    /// Never answers until [`Fake::release`].
    Hold,
}

type Slot = Rc<RefCell<Option<CallOutcome>>>;

struct Fake {
    refuse: Cell<bool>,
    answer: RefCell<Answer>,
    peers: RefCell<Vec<(ConnId, PeerFacts)>>,
    calls: RefCell<Vec<(ConnId, u64)>>,
    cancels: RefCell<Vec<(ConnId, u64)>>,
    closed: RefCell<Vec<ConnId>>,
    held: RefCell<HashMap<u64, Slot>>,
}

impl Fake {
    fn new() -> Rc<Self> {
        Rc::new(Self {
            refuse: Cell::new(false),
            answer: RefCell::new(Answer::Echo),
            peers: RefCell::default(),
            calls: RefCell::default(),
            cancels: RefCell::default(),
            closed: RefCell::default(),
            held: RefCell::default(),
        })
    }

    fn release(&self, id: u64) {
        if let Some(slot) = self.held.borrow_mut().remove(&id) {
            *slot.borrow_mut() = Some(CallOutcome::Ok(json!({"released": id})));
        }
    }

    fn conn(&self) -> ConnId {
        self.peers
            .borrow()
            .last()
            .expect("an authenticated connection")
            .0
    }
}

fn welcome() -> Welcome {
    let w = WorkerId::parse("w_abcdefgh").expect("worker id");
    Welcome {
        api: 1,
        connection: "cn_1".into(),
        principal: Principal {
            worker_id: w.clone(),
            root_id: w,
            driver: Driver::Claude,
            depth: 0,
            can_spawn: true,
            task_mode_ceiling: vec![],
        },
        authority_epoch: 1,
        limits: IpcLimits {
            max_frame: MAX_FRAME as u32,
            max_inflight: 4,
        },
    }
}

impl CallDispatcher for Fake {
    fn authenticate(
        &self,
        conn: ConnId,
        _hello: Hello,
        peer: PeerFacts,
    ) -> LocalFuture<Result<Welcome, ToolError>> {
        self.peers.borrow_mut().push((conn, peer));
        let refuse = self.refuse.get();
        Box::pin(async move {
            if refuse {
                Err(ToolError::new(ErrorCode::AuthenticationFailed, "no"))
            } else {
                Ok(welcome())
            }
        })
    }

    fn call(&self, conn: ConnId, call: Call) -> LocalFuture<CallOutcome> {
        self.calls.borrow_mut().push((conn, call.id));
        match self.answer.borrow().clone() {
            Answer::Echo => Box::pin(async move { CallOutcome::Ok(json!({"echo": call.id})) }),
            Answer::Big(n) => Box::pin(async move { CallOutcome::Ok(json!("y".repeat(n))) }),
            Answer::Hold => {
                let slot: Slot = Rc::default();
                self.held.borrow_mut().insert(call.id, slot.clone());
                Box::pin(async move {
                    loop {
                        if let Some(o) = slot.borrow_mut().take() {
                            return o;
                        }
                        glib::timeout_future(Duration::from_millis(5)).await;
                    }
                })
            }
        }
    }

    fn cancel(&self, conn: ConnId, id: u64) {
        self.cancels.borrow_mut().push((conn, id));
    }

    fn closed(&self, conn: ConnId) {
        self.closed.borrow_mut().push(conn);
    }
}

// ---- client ------------------------------------------------------------------------------------

struct Client {
    s: UnixStream,
    decoder: FrameDecoder,
}

impl Client {
    fn connect(path: &Path) -> Self {
        let s = UnixStream::connect(path).expect("connect");
        s.set_nonblocking(true).expect("nonblocking");
        Self {
            s,
            decoder: FrameDecoder::new(),
        }
    }

    fn raw(&mut self, bytes: &[u8]) {
        self.s.write_all(bytes).expect("send");
    }

    fn send(&mut self, frame: &Frame) {
        let bytes = encode_frame(frame).expect("encode");
        self.raw(&bytes);
    }

    fn hello(&mut self) {
        self.send(&Frame::Hello(Hello {
            api: ApiRange { min: 1, max: 1 },
            instance: InstanceNonce::from_bytes([1; 16]),
            cap: CapabilityToken::from_random([7; 32]),
            bridge: BridgeInfo {
                version: "test".into(),
                pid: std::process::id(),
            },
        }));
    }

    fn call(&mut self, id: u64) {
        self.send(&Frame::Call(Call {
            id,
            tool: ToolName::CrossAgentList,
            args: json!({}),
            timeout_ms: 30_000,
            invocation: None,
        }));
    }

    /// One read step: `Some(true)` on EOF or reset.
    fn poll(&mut self) -> Option<bool> {
        let want = self.decoder.wants_bytes();
        if want == 0 {
            return None;
        }
        let mut buf = vec![0; want.min(64 * 1024)];
        match self.s.read(&mut buf) {
            Ok(0) => Some(true),
            Ok(n) => {
                self.decoder.push(&buf[..n]).expect("server frame");
                None
            }
            Err(e) if e.kind() == ErrorKind::WouldBlock => None,
            Err(_) => Some(true),
        }
    }

    /// The next frame from the server, pumping `ctx`; `None` on EOF or timeout.
    fn recv(&mut self, ctx: &glib::MainContext) -> Option<Frame> {
        let got: RefCell<Option<Frame>> = RefCell::new(None);
        let eof = Cell::new(false);
        pump_until(ctx, 10, || {
            if let Some(Ok(f)) = self.decoder.next_frame() {
                *got.borrow_mut() = Some(f);
                return true;
            }
            if self.poll() == Some(true) {
                eof.set(true);
            }
            eof.get()
        });
        got.into_inner()
    }

    /// Pumps until the server closes this connection (frames before it are discarded).
    fn closed_within(&mut self, ctx: &glib::MainContext, secs: u64) -> bool {
        pump_until(ctx, secs, || {
            let _ = self.decoder.next_frame();
            self.poll() == Some(true)
        })
    }
}

fn welcomed(c: &mut Client, ctx: &glib::MainContext) {
    c.hello();
    match c.recv(ctx) {
        Some(Frame::Welcome(w)) => assert_eq!(w, welcome()),
        other => panic!("expected welcome, got {other:?}"),
    }
}

fn result(c: &mut Client, ctx: &glib::MainContext) -> CallResult {
    match c.recv(ctx) {
        Some(Frame::Result(r)) => r,
        other => panic!("expected a result, got {other:?}"),
    }
}

// ---- fixture -----------------------------------------------------------------------------------

struct Setup {
    _tmp: tempfile::TempDir,
    dir: RuntimeDir,
    fake: Rc<Fake>,
}

fn setup() -> Setup {
    use std::os::unix::fs::DirBuilderExt;
    let tmp = tempfile::tempdir().expect("tmp");
    let dir = tmp.path().join("agent-terminal");
    std::fs::DirBuilder::new()
        .mode(0o700)
        .create(&dir)
        .expect("rundir");
    Setup {
        _tmp: tmp,
        dir: RuntimeDir::from_verified(dir),
        fake: Fake::new(),
    }
}

impl Setup {
    fn bind(&self, limits: Limits) -> Rc<IpcServer> {
        IpcServer::bind(&self.dir, 4242, limits, self.fake.clone()).expect("bind")
    }
}

fn fast() -> Limits {
    Limits {
        hello_deadline_ms: 2_000,
        frame_body_deadline_ms: 2_000,
        write_deadline_ms: 2_000,
        ..Limits::default()
    }
}

// ---- tests -------------------------------------------------------------------------------------

#[test]
fn binds_a_private_socket_and_round_trips_a_call_with_the_peer_pid() {
    in_loop(|ctx| {
        let s = setup();
        let server = s.bind(fast());
        let path = server.socket_path().to_owned();
        assert_eq!(path, s.dir.path().join("delegation-4242.sock"));
        let mode = |p: &Path| std::fs::metadata(p).expect("meta").permissions().mode() & 0o777;
        assert_eq!(mode(&path), 0o600);
        assert_eq!(mode(s.dir.path()), 0o700);

        let mut c = Client::connect(&path);
        assert!(pump_until(ctx, 5, || server.counts() == (1, 1)));
        welcomed(&mut c, ctx);
        assert_eq!(server.counts(), (1, 0));
        let (_, peer) = s.fake.peers.borrow()[0];
        assert_eq!(peer.pid, Some(std::process::id()), "SO_PEERCRED pid");

        c.call(7);
        let r = result(&mut c, ctx);
        assert_eq!(r.id, 7);
        assert_eq!(r.outcome, CallOutcome::Ok(json!({"echo": 7})));
        // The id is free again once answered.
        c.call(7);
        assert_eq!(result(&mut c, ctx).id, 7);
        c.send(&Frame::Cancel { id: 7 });
        assert!(pump_until(ctx, 5, || s.fake.cancels.borrow().len() == 1));
        assert_eq!(s.fake.cancels.borrow()[0], (s.fake.conn(), 7));

        drop(c);
        assert!(pump_until(ctx, 5, || s.fake.closed.borrow().len() == 1));
        assert_eq!(server.counts(), (0, 0));
    });
}

#[test]
fn an_oversized_length_closes_the_connection_before_any_body_is_read() {
    in_loop(|ctx| {
        let s = setup();
        let server = s.bind(fast());
        let mut c = Client::connect(server.socket_path());
        let mut bytes = (MAX_FRAME as u32 + 1).to_be_bytes().to_vec();
        // The body that would follow is never read: the length alone closes the connection.
        bytes.extend_from_slice(&[b'{'; 1024]);
        c.raw(&bytes);
        assert!(c.closed_within(ctx, 5));
        assert!(pump_until(ctx, 5, || server.counts() == (0, 0)));
        assert!(
            s.fake.peers.borrow().is_empty(),
            "never reached authenticate"
        );
        assert!(
            s.fake.closed.borrow().is_empty(),
            "the dispatcher never knew it"
        );

        let mut zero = Client::connect(server.socket_path());
        zero.raw(&[0, 0, 0, 0]);
        assert!(zero.closed_within(ctx, 5), "a zero length closes too");
    });
}

#[test]
fn no_hello_before_the_deadline_closes() {
    in_loop(|ctx| {
        let s = setup();
        let server = s.bind(Limits {
            hello_deadline_ms: 150,
            ..fast()
        });
        let mut c = Client::connect(server.socket_path());
        assert!(pump_until(ctx, 5, || server.counts() == (1, 1)));
        assert!(c.closed_within(ctx, 5));
        assert!(pump_until(ctx, 5, || server.counts() == (0, 0)));

        // A hello in time is not cut off by the timer afterwards.
        let mut ok = Client::connect(server.socket_path());
        welcomed(&mut ok, ctx);
        assert!(
            !ok.closed_within(ctx, 1),
            "authenticated past the hello deadline"
        );
        assert_eq!(server.counts(), (1, 0));
    });
}

#[test]
fn a_frame_before_hello_or_an_unknown_tag_closes() {
    in_loop(|ctx| {
        let s = setup();
        let server = s.bind(fast());
        let mut early = Client::connect(server.socket_path());
        early.call(1);
        assert!(early.closed_within(ctx, 5));

        let mut bogus = Client::connect(server.socket_path());
        welcomed(&mut bogus, ctx);
        let body = br#"{"t":"bogus"}"#;
        let mut bytes = (body.len() as u32).to_be_bytes().to_vec();
        bytes.extend_from_slice(body);
        bogus.raw(&bytes);
        assert!(bogus.closed_within(ctx, 5));
        assert!(s.fake.calls.borrow().is_empty());
    });
}

#[test]
fn connections_past_the_preauth_or_total_cap_close_at_once() {
    in_loop(|ctx| {
        let s = setup();
        let server = s.bind(Limits {
            ipc_connections: 3,
            ipc_preauth: 2,
            ..fast()
        });
        let path = server.socket_path().to_owned();
        let mut a = Client::connect(&path);
        let mut b = Client::connect(&path);
        assert!(pump_until(ctx, 5, || server.counts() == (2, 2)));
        let mut third = Client::connect(&path);
        assert!(third.closed_within(ctx, 5), "third unauthenticated");
        assert_eq!(server.counts(), (2, 2));

        // Authenticated connections leave the pre-auth count but stay in the total.
        welcomed(&mut a, ctx);
        welcomed(&mut b, ctx);
        assert_eq!(server.counts(), (2, 0));
        let mut c = Client::connect(&path);
        welcomed(&mut c, ctx);
        assert_eq!(server.counts(), (3, 0));
        let mut fourth = Client::connect(&path);
        assert!(fourth.closed_within(ctx, 5), "fourth past the total cap");
        assert_eq!(server.counts(), (3, 0));
    });
}

#[test]
fn a_call_past_max_inflight_gets_capacity_reached_and_is_never_dispatched() {
    in_loop(|ctx| {
        let s = setup();
        *s.fake.answer.borrow_mut() = Answer::Hold;
        let server = s.bind(Limits {
            max_inflight: 2,
            ..fast()
        });
        let mut c = Client::connect(server.socket_path());
        welcomed(&mut c, ctx);
        c.call(1);
        c.call(2);
        c.call(3);
        let r = result(&mut c, ctx);
        assert_eq!(r.id, 3);
        match r.outcome {
            CallOutcome::Err(e) => assert_eq!(e.code, ErrorCode::CapacityReached),
            other => panic!("expected capacity_reached, got {other:?}"),
        }
        let ids: Vec<u64> = s.fake.calls.borrow().iter().map(|(_, id)| *id).collect();
        assert_eq!(ids, vec![1, 2]);
        // Answering one frees a slot.
        s.fake.release(1);
        assert_eq!(result(&mut c, ctx).id, 1);
        c.call(4);
        assert!(pump_until(ctx, 5, || s.fake.calls.borrow().len() == 3));
    });
}

#[test]
fn a_duplicate_pending_call_id_closes_and_tells_the_dispatcher() {
    in_loop(|ctx| {
        let s = setup();
        *s.fake.answer.borrow_mut() = Answer::Hold;
        let server = s.bind(fast());
        let mut c = Client::connect(server.socket_path());
        welcomed(&mut c, ctx);
        c.call(5);
        c.call(5);
        assert!(c.closed_within(ctx, 5));
        assert!(pump_until(ctx, 5, || s.fake.closed.borrow().len() == 1));
        assert_eq!(s.fake.calls.borrow().len(), 1);
        assert_eq!(server.counts(), (0, 0));
    });
}

#[test]
fn a_second_hello_closes() {
    in_loop(|ctx| {
        let s = setup();
        let server = s.bind(fast());
        let mut c = Client::connect(server.socket_path());
        welcomed(&mut c, ctx);
        c.hello();
        assert!(c.closed_within(ctx, 5));
        assert_eq!(s.fake.peers.borrow().len(), 1, "authenticated once");
        assert!(pump_until(ctx, 5, || s.fake.closed.borrow().len() == 1));
    });
}

#[test]
fn a_refused_hello_is_answered_then_closed() {
    in_loop(|ctx| {
        let s = setup();
        s.fake.refuse.set(true);
        let server = s.bind(fast());
        let mut c = Client::connect(server.socket_path());
        c.hello();
        match c.recv(ctx) {
            Some(Frame::Refused { error }) => {
                assert_eq!(error.code, ErrorCode::AuthenticationFailed)
            }
            other => panic!("expected refused, got {other:?}"),
        }
        assert!(c.closed_within(ctx, 5));
        assert!(pump_until(ctx, 5, || server.counts() == (0, 0)));
    });
}

#[test]
fn a_frame_body_that_stalls_past_its_deadline_closes() {
    in_loop(|ctx| {
        let s = setup();
        let server = s.bind(Limits {
            frame_body_deadline_ms: 150,
            ..fast()
        });
        let mut c = Client::connect(server.socket_path());
        welcomed(&mut c, ctx);
        let mut partial = 100u32.to_be_bytes().to_vec();
        partial.extend_from_slice(b"{\"t\":");
        c.raw(&partial);
        assert!(c.closed_within(ctx, 5));
        assert!(pump_until(ctx, 5, || s.fake.closed.borrow().len() == 1));
    });
}

#[test]
fn a_reader_that_never_reads_misses_the_write_deadline_and_is_closed() {
    in_loop(|ctx| {
        let s = setup();
        // Four ~900 KB results: far more than a Unix socket buffers.
        *s.fake.answer.borrow_mut() = Answer::Big(900 * 1024);
        let server = s.bind(Limits {
            write_deadline_ms: 200,
            ..fast()
        });
        let mut c = Client::connect(server.socket_path());
        welcomed(&mut c, ctx);
        for id in 1..=4 {
            c.call(id);
        }
        // The client reads nothing more: the blocked write times out.
        assert!(pump_until(ctx, 10, || s.fake.closed.borrow().len() == 1));
        assert_eq!(server.counts(), (0, 0));
    });
}

#[test]
fn an_outbox_overflow_closes() {
    in_loop(|ctx| {
        let s = setup();
        *s.fake.answer.borrow_mut() = Answer::Big(900 * 1024);
        let server = s.bind(Limits {
            outbound_queue_bytes: 1024 * 1024,
            ..fast()
        });
        let mut c = Client::connect(server.socket_path());
        welcomed(&mut c, ctx);
        for id in 1..=4 {
            c.call(id);
        }
        assert!(pump_until(ctx, 10, || s.fake.closed.borrow().len() == 1));
        assert_eq!(server.counts(), (0, 0));
    });
}

#[test]
fn revoke_sends_revoked_then_closes() {
    in_loop(|ctx| {
        let s = setup();
        *s.fake.answer.borrow_mut() = Answer::Hold;
        let server = s.bind(fast());
        let mut c = Client::connect(server.socket_path());
        welcomed(&mut c, ctx);
        c.call(1);
        assert!(pump_until(ctx, 5, || s.fake.calls.borrow().len() == 1));
        server.revoke(
            s.fake.conn(),
            ToolError::new(ErrorCode::RuntimeUnavailable, "revoked"),
        );
        match c.recv(ctx) {
            Some(Frame::Revoked { error }) => assert_eq!(error.code, ErrorCode::RuntimeUnavailable),
            other => panic!("expected revoked, got {other:?}"),
        }
        assert!(c.closed_within(ctx, 5));
        assert!(pump_until(ctx, 5, || s.fake.closed.borrow().len() == 1));
        assert_eq!(server.counts(), (0, 0));
        // A late result for the closed connection goes nowhere.
        s.fake.release(1);
        assert!(pump_until(ctx, 1, || s.fake.held.borrow().is_empty()));
        // Revoking an unknown connection does nothing.
        server.revoke(
            ConnId(999),
            ToolError::new(ErrorCode::RuntimeUnavailable, "x"),
        );
    });
}

#[test]
fn close_all_revokes_everyone_and_removes_the_socket() {
    in_loop(|ctx| {
        let s = setup();
        let server = s.bind(fast());
        let path = server.socket_path().to_owned();
        let mut a = Client::connect(&path);
        welcomed(&mut a, ctx);
        let mut b = Client::connect(&path);
        assert!(pump_until(ctx, 5, || server.counts() == (2, 1)));
        server.close_all(ToolError::new(ErrorCode::RuntimeUnavailable, "shutdown"));
        assert!(!path.exists(), "socket removed");
        assert!(matches!(a.recv(ctx), Some(Frame::Revoked { .. })));
        assert!(a.closed_within(ctx, 5));
        assert!(b.closed_within(ctx, 5));
        assert!(pump_until(ctx, 5, || server.counts() == (0, 0)));
        assert!(UnixStream::connect(&path).is_err(), "nothing listens");
    });
}

#[test]
fn dropping_the_server_removes_the_socket() {
    in_loop(|ctx| {
        let s = setup();
        let server = s.bind(fast());
        let path = server.socket_path().to_owned();
        let mut c = Client::connect(&path);
        welcomed(&mut c, ctx);
        drop(server);
        assert!(!path.exists());
        assert!(c.closed_within(ctx, 5));
    });
}

#[test]
fn a_stale_socket_of_ours_is_replaced() {
    in_loop(|ctx| {
        let s = setup();
        let path = s.dir.path().join("delegation-4242.sock");
        drop(std::os::unix::net::UnixListener::bind(&path).expect("stale"));
        assert!(path.exists(), "left behind");
        let server = s.bind(fast());
        let mut c = Client::connect(server.socket_path());
        welcomed(&mut c, ctx);
    });
}

#[test]
fn a_symlink_or_a_regular_file_at_the_socket_path_refuses_the_bind() {
    in_loop(|_| {
        let s = setup();
        let path = s.dir.path().join("delegation-4242.sock");
        let target = s.dir.path().join("elsewhere");
        std::fs::write(&target, b"keep").expect("target");
        std::os::unix::fs::symlink(&target, &path).expect("link");
        let err = IpcServer::bind(&s.dir, 4242, fast(), s.fake.clone())
            .err()
            .expect("refused");
        assert!(err.contains("symlink"), "{err}");
        assert_eq!(std::fs::read(&target).expect("kept"), b"keep");
        assert!(std::fs::symlink_metadata(&path).is_ok(), "link left alone");

        std::fs::remove_file(&path).expect("unlink");
        std::fs::write(&path, b"x").expect("file");
        let err = IpcServer::bind(&s.dir, 4242, fast(), s.fake.clone())
            .err()
            .expect("refused");
        assert!(err.contains("not a socket"), "{err}");
    });
}

#[test]
fn another_users_socket_is_never_removed() {
    let socket = |uid| {
        Some(Found {
            symlink: false,
            socket: true,
            uid,
        })
    };
    assert_eq!(leftover_verdict(None, 1000), Ok(Leftover::Absent));
    assert_eq!(leftover_verdict(socket(1000), 1000), Ok(Leftover::Stale));
    let err = leftover_verdict(socket(0), 1000).expect_err("root's socket");
    assert!(err.contains("another user"), "{err}");
    let link = Some(Found {
        symlink: true,
        socket: false,
        uid: 1000,
    });
    assert!(leftover_verdict(link, 1000).is_err());
}
