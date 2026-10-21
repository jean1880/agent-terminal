//! The store writer: one thread that owns the store's write connection.
//!
//! Every write the app makes on its main thread (an event as it streams, a prompt, a title, the
//! read mark, …) is queued here instead of run in place, so the main thread never waits on
//! SQLite: not on the disk, not on a checkpoint, not on another connection's lock. The writer runs
//! what is queued in order, many writes to one transaction ([`Store::begin_batch`]), each write
//! under a savepoint of its own ([`Batch::job`]): one that fails or panics leaves no partial rows
//! and does not take its neighbours with it.
//!
//! - **Order.** One queue, one connection: writes land in the order they were queued, whichever
//!   app thread queued them.
//! - **Read-after-write.** [`StoreWriter::barrier`] answers once everything queued before it is
//!   committed (or has failed); a reader waits on it, then reads on a connection of its own (WAL
//!   shows it every committed row). A reader whose barrier does not answer in time does not read
//!   ([`StoreError::Behind`]). [`StoreWriter::call`] runs a job on the writer, in order, and hands
//!   its result back once its batch is committed.
//! - **Errors.** A write that fails is logged with what it was and its thread, and counted
//!   ([`StoreWriter::failures`]) for the app to show; so is a batch SQLite rolled back on its own,
//!   for every write it held. Nothing is dropped silently.
//! - **Overload.** The queue is bounded ([`WriterConfig`]): past the bound a write is refused at
//!   once (never blocking its caller), counted as a failure and shown. The writes that are taken
//!   keep their order.
//! - **Checkpoints.** SQLite's automatic checkpoint is off on this connection: the writer
//!   checkpoints (PASSIVE) every [`WriterConfig::checkpoint_every`] while there were writes, and
//!   whenever the queue has been idle for [`WriterConfig::idle`]. With `synchronous=NORMAL` in
//!   WAL mode a commit is synced at the next checkpoint, so a power cut can lose the commits since
//!   the last one: at most about `checkpoint_every` of writes. It cannot corrupt the database. If
//!   the WAL grows past [`WAL_WARN_BYTES`] anyway (a reader pinning old pages) that is an error:
//!   the writer says so, hands checkpoints back to SQLite, and says so again each time it doubles.
//! - **Shutdown.** [`StoreWriter::shutdown`] drains the queue, checkpoints (PASSIVE: the app's
//!   readers are still open, so closing would not) and closes the connection. No TRUNCATE: it
//!   would wait for readers. It says whether the queue really drained ([`Shutdown`]).
//!
//! [`StoreHandle`] is what the app holds: the writer plus a read connection of the calling
//! thread, or (memory-only store, tests) one store that runs writes in place.

use std::panic::AssertUnwindSafe;
use std::path::Path;
use std::rc::Rc;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use agent_core::delegation::authority::RevocationEvent;
use agent_core::delegation::ids::{OperationId, WorkerId};
use agent_core::delegation::operation::OperationState;
use agent_core::event::{Envelope, Event};

use crate::store::delegation_typed::{
    Accept, Accepted, AckRequest, AckResult, Completed, Completion, DispatchFact, PublishEvent,
    Published, Reconciled, Reserved, Revoked, Verdict,
};
use crate::store::{Batch, ProviderThreadId, Result, Store, StoreError, SLOW_WRITE};

/// Past this many bytes of WAL the writer reports an error and turns SQLite's own checkpoints
/// back on.
pub const WAL_WARN_BYTES: i64 = 64 << 20;
/// SQLite's default automatic checkpoint threshold, restored by the WAL guard.
const AUTOCHECKPOINT_PAGES: i64 = 1000;
/// Most writes committed in one transaction.
const MAX_BATCH: usize = 512;
/// How long a read on the calling (main) thread waits for the writer to catch up
/// ([`StoreHandle::read`]) before it gives up rather than read stale rows.
pub const READ_FLUSH_WAIT: Duration = Duration::from_secs(5);
/// The same for a read on a worker thread, which blocks nothing on screen.
pub const WORKER_FLUSH_WAIT: Duration = Duration::from_secs(30);
/// What a write is counted as against the byte bound when its size is not worked out.
const DEFAULT_COST: usize = 256;

/// How the writer checkpoints and how much it queues.
#[derive(Debug, Clone, Copy)]
pub struct WriterConfig {
    /// At most this long between checkpoints while there are writes.
    pub checkpoint_every: Duration,
    /// A queue idle this long, with writes since the last checkpoint, checkpoints.
    pub idle: Duration,
    /// Most writes waiting at once; past it a write is refused.
    pub max_queued: usize,
    /// Most bytes (of envelopes, roughly) waiting at once; past it a write is refused.
    pub max_queued_bytes: usize,
}

impl Default for WriterConfig {
    fn default() -> Self {
        Self {
            checkpoint_every: Duration::from_secs(30),
            idle: Duration::from_secs(5),
            max_queued: 100_000,
            max_queued_bytes: 64 << 20,
        }
    }
}

/// How [`StoreWriter::shutdown`] went.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Shutdown {
    /// Everything queued was run and the connection closed.
    Drained,
    /// The writer was still busy when the wait ran out: the rest may be lost.
    TimedOut,
    /// The writer had already stopped (or died): whatever was still queued is lost.
    WriterGone,
}

type WriteJob = Box<dyn FnOnce(&Store) -> Result<()> + Send>;
/// Hands a job's answer back once its outcome is known: `true` committed, `false` not.
type Deliver = Box<dyn FnOnce(bool) + Send>;

struct Write {
    what: &'static str,
    thread: Option<String>,
    job: WriteJob,
    /// Run once the write's outcome is known (a `call`'s answer).
    deliver: Option<Deliver>,
    cost: usize,
    /// Admitted past the queue's bound (what carries a heal must never be refused).
    force: bool,
    /// The job's own `Err` is its caller's to report (a `call_result`): logged, not counted.
    caller_reports: bool,
}

/// A write that ran, waiting for its batch's outcome.
struct Done {
    what: &'static str,
    thread: Option<String>,
    deliver: Option<Deliver>,
    cost: usize,
    caller_reports: bool,
}

/// Why a write did not commit.
enum Failure<'a> {
    /// Its own job returned `Err` (its savepoint, rows and tail changes were taken back).
    Job(&'a str),
    /// Anything else: a panic, a batch that could not begin, was lost or did not commit.
    Other(&'a str),
}

/// A control write ([`StoreWriter::durable_call`]): one transaction of its own, committed under
/// `synchronous=FULL`, never batched with any other write.
struct DurableWrite {
    what: &'static str,
    job: WriteJob,
    /// Handed the outcome: `Ok` committed durably (and the level restored), else why not.
    settle: Box<dyn FnOnce(Result<()>) + Send>,
    cost: usize,
}

enum Op {
    Write(Write),
    Durable(DurableWrite),
    Barrier(Sender<()>),
    Shutdown(Sender<()>),
}

struct Shared {
    /// Set (under the lock) once the writer takes its last look at the queue: a write sent
    /// after that would be lost, so it is refused instead. Writes are sent while holding it.
    closed: Mutex<bool>,
    /// Writes queued and not yet committed (or failed).
    pending: AtomicUsize,
    /// Their rough size.
    queued_bytes: AtomicUsize,
    failures: AtomicU64,
    /// Of the failures, writes refused because the queue was full.
    overloaded: AtomicU64,
    last_failure: Mutex<Option<String>>,
    config: WriterConfig,
    /// The queue went past its bound for in-flight items (said once, until it drains).
    over_bound: std::sync::atomic::AtomicBool,
}

impl Shared {
    fn fail(&self, what: &str, thread: Option<&str>, error: &str) {
        self.failures.fetch_add(1, Ordering::SeqCst);
        tracing::error!(what, thread, error, "a store write failed");
        if let Ok(mut last) = self.last_failure.lock() {
            *last = Some(format!("Could not {what}: {error}"));
        }
    }

    fn settle(&self, cost: usize) {
        let left = self
            .pending
            .fetch_sub(1, Ordering::SeqCst)
            .saturating_sub(1);
        if left < self.config.max_queued / 2 {
            self.over_bound.store(false, Ordering::SeqCst);
        }
        self.queued_bytes.fetch_sub(cost, Ordering::SeqCst);
    }

    fn finish_ok(&self, done: Done) {
        if let Some(deliver) = done.deliver {
            deliver(true);
        }
        self.settle(done.cost);
    }

    fn finish_failed(&self, done: Done, failure: Failure<'_>) {
        match failure {
            Failure::Job(error) if done.caller_reports => {
                tracing::warn!(
                    what = done.what,
                    error,
                    "a store job failed; its caller says so"
                );
            }
            Failure::Job(error) | Failure::Other(error) => {
                self.fail(done.what, done.thread.as_deref(), error);
            }
        }
        if let Some(deliver) = done.deliver {
            deliver(false);
        }
        self.settle(done.cost);
    }
}

/// A handle on the store writer thread; cheap to clone, and `Send`.
#[derive(Clone)]
pub struct StoreWriter {
    tx: Sender<Op>,
    shared: Arc<Shared>,
}

impl StoreWriter {
    /// Opens the store at `path` on a new writer thread, blocking until it is open (the schema
    /// is migrated by whoever opened the store first, so this is one quick open).
    pub fn spawn(path: &Path) -> Result<Self> {
        Self::spawn_with(path, WriterConfig::default())
    }

    /// [`StoreWriter::spawn`] with its own checkpoint timing and queue bounds.
    pub fn spawn_with(path: &Path, config: WriterConfig) -> Result<Self> {
        let (tx, rx) = mpsc::channel();
        let (ready_tx, ready_rx) = mpsc::channel();
        let shared = Arc::new(Shared {
            closed: Mutex::new(false),
            pending: AtomicUsize::new(0),
            queued_bytes: AtomicUsize::new(0),
            failures: AtomicU64::new(0),
            overloaded: AtomicU64::new(0),
            last_failure: Mutex::new(None),
            config,
            over_bound: std::sync::atomic::AtomicBool::new(false),
        });
        let path = path.to_owned();
        let thread_shared = shared.clone();
        std::thread::Builder::new()
            .name("store-writer".into())
            .spawn(move || {
                let opened = Store::open(&path).and_then(|s| s.set_autocheckpoint(0).map(|()| s));
                match opened {
                    Ok(store) => {
                        let _ = ready_tx.send(Ok(()));
                        run(store, rx, &thread_shared);
                    }
                    Err(e) => {
                        let _ = ready_tx.send(Err(e));
                    }
                }
            })?;
        ready_rx.recv().map_err(|_| StoreError::WriterGone)??;
        Ok(Self { tx, shared })
    }

    /// Queues `job` (`what` names it in logs and the failure message: "store an event").
    /// `Err` when it was not queued: the writer is gone, or its queue is full (both counted in
    /// [`StoreWriter::failures`]). A job that fails later is counted there too.
    pub fn write(
        &self,
        what: &'static str,
        thread: Option<&str>,
        job: impl FnOnce(&Store) -> Result<()> + Send + 'static,
    ) -> Result<()> {
        self.write_sized(what, thread, DEFAULT_COST, false, job)
    }

    /// [`StoreWriter::write`] with its size counted against the byte bound; `force` admits it
    /// past the bound (a write that carries a heal must never be refused).
    pub fn write_sized(
        &self,
        what: &'static str,
        thread: Option<&str>,
        cost: usize,
        force: bool,
        job: impl FnOnce(&Store) -> Result<()> + Send + 'static,
    ) -> Result<()> {
        self.enqueue(Write {
            what,
            thread: thread.map(str::to_owned),
            job: Box::new(job),
            deliver: None,
            cost,
            force,
            caller_reports: false,
        })
    }

    fn enqueue(&self, write: Write) -> Result<()> {
        let (what, thread, cost, force) =
            (write.what, write.thread.clone(), write.cost, write.force);
        self.admit(what, thread, cost, force, Op::Write(write))
    }

    /// Queues `op` (a write or a durable write) under the queue's bounds; refused (and counted)
    /// when the writer is gone or the queue is full and `force` is not set.
    fn admit(
        &self,
        what: &'static str,
        thread: Option<String>,
        cost: usize,
        force: bool,
        op: Op,
    ) -> Result<()> {
        let config = &self.shared.config;
        let refused = match self.shared.closed.lock() {
            Ok(closed) if !*closed => {
                let pending = self.shared.pending.load(Ordering::SeqCst);
                let bytes = self.shared.queued_bytes.load(Ordering::SeqCst);
                let full = pending >= config.max_queued
                    || bytes.saturating_add(cost) > config.max_queued_bytes;
                if full && force && !self.shared.over_bound.swap(true, Ordering::SeqCst) {
                    // By design: items already in flight are never cut short, so the queue
                    // grows past its bound while the writer is stalled. Said once per stall.
                    tracing::error!(
                        pending,
                        mb = bytes >> 20,
                        "the store writer is stalled; in-flight items go past the queue's bound"
                    );
                }
                if full && !force {
                    self.shared.overloaded.fetch_add(1, Ordering::SeqCst);
                    Some((
                        StoreError::Overloaded,
                        format!(
                            "the store is too far behind ({pending} writes, {} MB waiting)",
                            bytes >> 20
                        ),
                    ))
                } else {
                    self.shared.pending.fetch_add(1, Ordering::SeqCst);
                    self.shared.queued_bytes.fetch_add(cost, Ordering::SeqCst);
                    if self.tx.send(op).is_ok() {
                        None
                    } else {
                        self.shared.settle(cost);
                        Some((StoreError::WriterGone, StoreError::WriterGone.to_string()))
                    }
                }
            }
            _ => Some((StoreError::WriterGone, StoreError::WriterGone.to_string())),
        };
        match refused {
            None => Ok(()),
            Some((error, message)) => {
                self.shared.fail(what, thread.as_deref(), &message);
                Err(error)
            }
        }
    }

    /// Runs `job` on the writer after everything queued before it, inside a batch like any
    /// write (so it may open transactions of its own, which become savepoints), and hands its
    /// result back once that batch is committed. The receiver errors when the writer is gone,
    /// the queue is full, the job panicked or its batch did not commit.
    pub fn call<T: Send + 'static>(
        &self,
        what: &'static str,
        job: impl FnOnce(&Store) -> T + Send + 'static,
    ) -> Receiver<T> {
        let (tx, rx) = mpsc::channel();
        let slot: Arc<Mutex<Option<T>>> = Arc::new(Mutex::new(None));
        let fill = slot.clone();
        let _ = self.enqueue(Write {
            what,
            thread: None,
            job: Box::new(move |store: &Store| {
                let value = job(store);
                if let Ok(mut slot) = fill.lock() {
                    *slot = Some(value);
                }
                Ok(())
            }),
            deliver: Some(Box::new(move |committed| {
                if let Some(value) = slot.lock().ok().and_then(|mut s| s.take()) {
                    if committed {
                        let _ = tx.send(value);
                    }
                }
            })),
            cost: DEFAULT_COST,
            force: false,
            caller_reports: false,
        });
        rx
    }

    /// [`StoreWriter::call`] for a job that can fail: an `Err` rolls its savepoint back (no
    /// partial rows, no tail change) and is handed back as it was, for the caller to report.
    /// `Ok` is handed back once its batch is committed; a batch that is not turns it into an
    /// `Err`. The receiver errors when the writer is gone, the queue is full or the job panicked.
    pub fn call_result<T: Send + 'static>(
        &self,
        what: &'static str,
        job: impl FnOnce(&Store) -> Result<T> + Send + 'static,
    ) -> Receiver<Result<T>> {
        self.call_result_sized(what, DEFAULT_COST, job)
    }

    /// A commit receipt with the captured payload charged to the bounded write queue.
    /// Like `call_result`, delivery occurs only after the whole batch commits.
    pub fn call_result_sized<T: Send + 'static>(
        &self,
        what: &'static str,
        cost: usize,
        job: impl FnOnce(&Store) -> Result<T> + Send + 'static,
    ) -> Receiver<Result<T>> {
        let (tx, rx) = mpsc::channel();
        let slot: Arc<Mutex<Option<Result<T>>>> = Arc::new(Mutex::new(None));
        let fill = slot.clone();
        let _ = self.enqueue(Write {
            what,
            thread: None,
            job: Box::new(move |store: &Store| {
                let result = job(store);
                let outcome = match &result {
                    Ok(_) => Ok(()),
                    Err(e) => Err(StoreError::Failed(e.to_string())),
                };
                if let Ok(mut slot) = fill.lock() {
                    *slot = Some(result);
                }
                outcome
            }),
            deliver: Some(Box::new(move |committed| {
                let answer = match slot.lock().ok().and_then(|mut s| s.take()) {
                    Some(Ok(value)) if committed => Ok(value),
                    Some(Ok(_)) => Err(StoreError::Failed(
                        "the batch it ran in was not committed".to_owned(),
                    )),
                    Some(Err(e)) => Err(e),
                    None => return, // it panicked: no answer
                };
                let _ = tx.send(answer);
            })),
            cost,
            force: false,
            caller_reports: true,
        });
        rx
    }

    /// Runs `job` as a **durable control write** on this same writer, in order after everything
    /// queued before it, and hands its result back once it is committed under
    /// `synchronous=FULL`:
    ///
    /// 1. the writes batched before it are committed (and answered) first;
    /// 2. `synchronous` is switched to FULL outside any transaction and read back;
    /// 3. `job` runs in a transaction of its own, committed with an fsync;
    /// 4. the previous level is restored and read back, before any later write is batched.
    ///
    /// It is never batched with another write, durable or not. An error at either switch refuses
    /// the operation (`StoreError::Durability`) and is counted like any failed write, as are a
    /// refused enqueue (writer gone, queue full), a failed commit and a panic. The job's own `Err`
    /// is handed back as it was, for the caller to report (not counted), like
    /// [`StoreWriter::call_result`]. The receiver always answers, a panic included.
    pub fn durable_call<T: Send + 'static>(
        &self,
        what: &'static str,
        job: impl FnOnce(&Store) -> Result<T> + Send + 'static,
    ) -> Receiver<Result<T>> {
        self.durable_call_sized(what, DEFAULT_COST, job)
    }

    /// A durable control receipt whose retained capture is charged to the queue byte cap.
    pub fn durable_call_sized<T: Send + 'static>(
        &self,
        what: &'static str,
        cost: usize,
        job: impl FnOnce(&Store) -> Result<T> + Send + 'static,
    ) -> Receiver<Result<T>> {
        let (tx, rx) = mpsc::channel();
        let slot: Arc<Mutex<Option<Result<T>>>> = Arc::new(Mutex::new(None));
        let fill = slot.clone();
        let refused_tx = tx.clone();
        let write = DurableWrite {
            what,
            job: Box::new(move |store: &Store| {
                let result = job(store);
                let outcome = match &result {
                    Ok(_) => Ok(()),
                    Err(e) => Err(StoreError::Failed(e.to_string())),
                };
                if let Ok(mut slot) = fill.lock() {
                    *slot = Some(result);
                }
                outcome
            }),
            settle: Box::new(move |outcome: Result<()>| {
                let held = slot.lock().ok().and_then(|mut s| s.take());
                let answer = match (outcome, held) {
                    (Ok(()), Some(Ok(value))) => Ok(value),
                    // The job's own error, its transaction rolled back.
                    (Err(StoreError::Failed(_)), Some(Err(e))) => Err(e),
                    (Err(e), _) => Err(e),
                    (Ok(()), _) => Err(StoreError::Failed(
                        "the durable write gave no answer".to_owned(),
                    )),
                };
                let _ = tx.send(answer);
            }),
            cost,
        };
        if let Err(e) = self.admit(what, None, cost, false, Op::Durable(write)) {
            let _ = refused_tx.send(Err(e));
        }
        rx
    }

    // ---- typed delegation control writes ----
    //
    // Each runs as one durable control write. Classify the answer with
    // `DurableOutcome::from_answer`: only `Committed` may be dispatched; a "committed" durability
    // error is `CommittedUnconfirmed` (refused, never re-dispatched).

    /// Step 1: accept a mutation ([`Store::accept_delegation`]).
    pub fn delegation_accept(&self, accept: Accept) -> Receiver<Result<Verdict<Accepted>>> {
        self.durable_call("accept a delegated operation", move |s| {
            s.accept_delegation(&accept)
        })
    }

    /// Step 2: reserve a dispatch ([`Store::reserve_delegation_dispatch`]).
    pub fn delegation_reserve(&self, op: OperationId) -> Receiver<Result<Verdict<Reserved>>> {
        self.durable_call("reserve a delegated dispatch", move |s| {
            s.reserve_delegation_dispatch(&op)
        })
    }

    /// Step 4: record a dispatch fact ([`Store::record_delegation_dispatch`]).
    pub fn delegation_record(
        &self,
        op: OperationId,
        fact: DispatchFact,
    ) -> Receiver<Result<OperationState>> {
        self.durable_call("record a delegated dispatch", move |s| {
            s.record_delegation_dispatch(&op, &fact)
        })
    }

    /// Step 5: complete a run ([`Store::complete_delegation_run`]).
    pub fn delegation_complete(&self, completion: Completion) -> Receiver<Result<Completed>> {
        self.durable_call("complete a delegated run", move |s| {
            s.complete_delegation_run(&completion)
        })
    }

    /// Step 6: revoke ([`Store::revoke_delegation`]).
    pub fn delegation_revoke(
        &self,
        root_worker: WorkerId,
        event: RevocationEvent,
    ) -> Receiver<Result<Revoked>> {
        self.durable_call("revoke delegated authority", move |s| {
            s.revoke_delegation(&root_worker, &event)
        })
    }

    /// A mailbox ACK ([`Store::acknowledge_delegation_messages`]).
    pub fn delegation_ack(&self, ack: AckRequest) -> Receiver<Result<Verdict<AckResult>>> {
        self.durable_call("acknowledge delegated messages", move |s| {
            s.acknowledge_delegation_messages(&ack)
        })
    }

    /// Startup reconciliation ([`Store::reconcile_delegation_after_restart`]).
    pub fn delegation_reconcile(&self) -> Receiver<Result<Reconciled>> {
        self.durable_call("reconcile delegation after a restart", |s| {
            s.reconcile_delegation_after_restart()
        })
    }

    /// Closing a thread's delegation before its deletion ([`Store::close_and_revoke_for_delete`]).
    pub fn delegation_close_for_delete(&self, thread: String) -> Receiver<Result<Option<Revoked>>> {
        self.durable_call("close delegation for a deletion", move |s| {
            s.close_and_revoke_for_delete(&thread)
        })
    }

    /// Publishes one event with its links ([`Store::publish_delegation_event`]): an ordinary
    /// (batched) write whose seq is handed back only once its batch committed.
    pub fn delegation_publish(&self, event: PublishEvent) -> Receiver<Result<Published>> {
        self.call_result("publish a delegated event", move |s| {
            s.publish_delegation_event(&event)
        })
    }

    /// Queues one event. An event of an item, or a turn's or session's end, is never refused:
    /// they go in past the queue's bound (a writer stall then grows the queue, said at error
    /// level once per stall). Only events of no item that end nothing may be refused.
    pub fn append_event(
        &self,
        thread: &str,
        provider_thread: Option<&str>,
        env: &Envelope,
    ) -> Result<()> {
        let cost = encoded_len(env);
        let (t, pt, owned) = (
            thread.to_owned(),
            provider_thread.map(str::to_owned),
            env.clone(),
        );
        let job = move |s: &Store| s.append_event(&t, pt.as_deref(), &owned).map(drop);
        // Never refused, full queue or not: any event of an item (refusing part of one could
        // leave half a secret stored, never healed) and a turn's or session's end. Only events
        // of no item that end nothing follow the bound.
        let force = env.item.is_some()
            || matches!(
                env.event,
                Event::TurnCompleted { .. } | Event::SessionExited { .. }
            );
        self.write_sized("store an event", Some(thread), cost, force, job)
    }

    /// Answers once everything queued before it is committed (or has failed). Errors at once
    /// when the writer is gone.
    pub fn barrier(&self) -> Receiver<()> {
        let (tx, rx) = mpsc::channel();
        let _ = self.tx.send(Op::Barrier(tx));
        rx
    }

    /// Blocks until everything queued so far is committed, at most `wait`; whether it was.
    pub fn flush(&self, wait: Duration) -> bool {
        self.barrier().recv_timeout(wait).is_ok()
    }

    /// Writes queued and not yet committed (or failed).
    pub fn pending(&self) -> usize {
        self.shared.pending.load(Ordering::SeqCst)
    }

    /// How many writes failed so far (refused ones included), and what the last one said.
    pub fn failures(&self) -> (u64, Option<String>) {
        let last = self.shared.last_failure.lock().ok().and_then(|l| l.clone());
        (self.shared.failures.load(Ordering::SeqCst), last)
    }

    /// Of [`StoreWriter::failures`], the writes refused because the queue was full.
    pub fn overloaded(&self) -> u64 {
        self.shared.overloaded.load(Ordering::SeqCst)
    }

    /// Drains the queue and stops the writer (its connection closes, which checkpoints),
    /// waiting at most `wait`, and says how it went. Writes queued afterwards are refused.
    pub fn shutdown(&self, wait: Duration) -> Shutdown {
        let (tx, rx) = mpsc::channel();
        if self.tx.send(Op::Shutdown(tx)).is_err() {
            return Shutdown::WriterGone;
        }
        match rx.recv_timeout(wait) {
            Ok(()) => Shutdown::Drained,
            Err(RecvTimeoutError::Timeout) => Shutdown::TimedOut,
            Err(RecvTimeoutError::Disconnected) => Shutdown::WriterGone,
        }
    }
}

fn run(owned: Store, rx: Receiver<Op>, shared: &Shared) {
    let store = &owned;
    let config = shared.config;
    let mut dirty = false;
    let mut last_checkpoint = Instant::now();
    let mut wal_reported = 0i64;
    let mut checkpoint = |dirty: &mut bool, last: &mut Instant| {
        do_checkpoint(store, &mut wal_reported);
        *dirty = false;
        *last = Instant::now();
    };
    loop {
        let first = match rx.recv_timeout(config.idle) {
            Ok(op) => op,
            Err(RecvTimeoutError::Timeout) => {
                if dirty {
                    checkpoint(&mut dirty, &mut last_checkpoint);
                }
                continue;
            }
            Err(RecvTimeoutError::Disconnected) => break,
        };
        let mut ops = vec![first];
        while ops.len() < MAX_BATCH {
            match rx.try_recv() {
                Ok(op) => ops.push(op),
                Err(_) => break,
            }
        }
        let mut stop = Vec::new();
        dirty |= process(store, shared, ops, &mut stop);
        if !stop.is_empty() {
            // Everything queued meanwhile goes too. Under the lock no write can slip in between
            // the last look and the close, so none is lost unseen: later ones are refused.
            let closed = shared.closed.lock();
            loop {
                let rest: Vec<Op> = rx.try_iter().take(MAX_BATCH).collect();
                if rest.is_empty() {
                    break;
                }
                process(store, shared, rest, &mut stop);
            }
            if let Ok(mut closed) = closed {
                *closed = true;
            }
            drop(rx);
            // The app's other connections are still open, so closing this one does not
            // checkpoint: do it now (PASSIVE, never waiting on a reader), so the last commits
            // do not sit in the WAL until the next run.
            do_checkpoint(store, &mut wal_reported);
            drop(owned);
            tracing::debug!("store writer: queue drained, connection closed");
            for done in stop {
                let _ = done.send(());
            }
            return;
        }
        if dirty && last_checkpoint.elapsed() >= config.checkpoint_every {
            checkpoint(&mut dirty, &mut last_checkpoint);
        }
    }
}

/// Runs `ops` in order: consecutive writes as one batch, a barrier or shutdown after the writes
/// before it. Whether anything may have been written.
fn process(store: &Store, shared: &Shared, ops: Vec<Op>, stop: &mut Vec<Sender<()>>) -> bool {
    let mut wrote = false;
    let mut writes = Vec::new();
    for op in ops {
        match op {
            Op::Write(w) => writes.push(w),
            Op::Durable(d) => {
                // The open batch is committed (and answered) first: a durable write never shares
                // a transaction, or an unacknowledged batch, with any other.
                commit(store, shared, &mut writes);
                run_durable(store, shared, d);
                wrote = true;
            }
            Op::Barrier(done) => {
                wrote |= commit(store, shared, &mut writes);
                let _ = done.send(());
            }
            Op::Shutdown(done) => {
                wrote |= commit(store, shared, &mut writes);
                stop.push(done);
            }
        }
    }
    wrote | commit(store, shared, &mut writes)
}

/// Runs `writes` as one batch (see the module docs); each write's outcome is settled.
/// Whether there was anything to write.
fn commit(store: &Store, shared: &Shared, writes: &mut Vec<Write>) -> bool {
    run_batch(store, shared, std::mem::take(writes))
}

/// The body of [`commit`], reusable by tests without a thread.
fn run_batch(store: &Store, shared: &Shared, writes: Vec<Write>) -> bool {
    if writes.is_empty() {
        return false;
    }
    let n = writes.len();
    let started = Instant::now();
    let done_of = |w: Write| -> (Done, WriteJob) {
        let Write {
            what,
            thread,
            job,
            deliver,
            cost,
            caller_reports,
            ..
        } = w;
        (
            Done {
                what,
                thread,
                deliver,
                cost,
                caller_reports,
            },
            job,
        )
    };
    // Writes that ran, waiting for the COMMIT.
    let mut held: Vec<Done> = Vec::new();
    let mut txn: Option<Batch<'_>> = None;
    let mut queue = writes.into_iter();
    while let Some(w) = queue.next() {
        let (done, job) = done_of(w);
        if txn.is_none() {
            match store.begin_batch() {
                Ok(batch) => txn = Some(batch),
                Err(e) => {
                    // `begin_batch` already retried: the rest of the batch fails with it, at
                    // once, rather than each write waiting out the lock again.
                    let error = format!("the batch could not begin: {e}");
                    shared.finish_failed(done, Failure::Other(&error));
                    for rest in queue.by_ref() {
                        shared.finish_failed(done_of(rest).0, Failure::Other(&error));
                    }
                    break;
                }
            }
        }
        let Some(batch) = txn.as_ref() else {
            shared.finish_failed(done, Failure::Other("no batch"));
            continue;
        };
        let one = Instant::now();
        let result = std::panic::catch_unwind(AssertUnwindSafe(|| batch.job(job)));
        let took = one.elapsed();
        if took > SLOW_WRITE {
            tracing::warn!(
                kind = done.what,
                ms = took.as_secs_f64() * 1000.0,
                "slow store write"
            );
        }
        match result {
            Ok(Ok(())) => held.push(done),
            Ok(Err(e)) => shared.finish_failed(done, Failure::Job(&e.to_string())),
            Err(_) => shared.finish_failed(done, Failure::Other("the write panicked")),
        }
        if !batch.alive() {
            // SQLite rolled the whole transaction back on its own: what it held is gone. The
            // batch is dropped (its tail changes taken back, its threads' tails distrusted)
            // and the rest go in a new one.
            let lost = StoreError::TransactionLost.to_string();
            for d in held.drain(..) {
                shared.finish_failed(d, Failure::Other(&lost));
            }
            txn = None;
        }
    }
    if let Some(batch) = txn {
        match batch.commit() {
            Ok(()) => {
                for d in held.drain(..) {
                    shared.finish_ok(d);
                }
            }
            Err(e) => {
                let error = format!("the batch could not be committed: {e}");
                for d in held.drain(..) {
                    shared.finish_failed(d, Failure::Other(&error));
                }
            }
        }
    }
    let took = started.elapsed();
    if took > SLOW_WRITE {
        tracing::warn!(
            kind = "batch",
            writes = n,
            ms = took.as_secs_f64() * 1000.0,
            "slow store batch"
        );
    }
    true
}

/// Runs one durable write ([`Store::durably`]) and settles it: its own `Err` goes back to its
/// caller uncounted; any other failure (a `synchronous` switch, the commit, a panic) is counted.
fn run_durable(store: &Store, shared: &Shared, d: DurableWrite) {
    let DurableWrite {
        what,
        job,
        settle,
        cost,
    } = d;
    let started = Instant::now();
    let outcome = std::panic::catch_unwind(AssertUnwindSafe(|| store.durably(job)));
    let took = started.elapsed();
    if took > SLOW_WRITE {
        tracing::debug!(
            kind = what,
            ms = took.as_secs_f64() * 1000.0,
            "durable store write (fsync at commit)"
        );
    }
    let outcome = match outcome {
        Ok(Ok(())) => Ok(()),
        Ok(Err(StoreError::Failed(error))) => {
            tracing::warn!(
                what,
                error,
                "a durable store job failed; its caller says so"
            );
            Err(StoreError::Failed(error))
        }
        Ok(Err(e)) => {
            shared.fail(what, None, &e.to_string());
            Err(e)
        }
        Err(_) => {
            shared.fail(what, None, "the write panicked");
            Err(StoreError::Failed("the durable write panicked".to_owned()))
        }
    };
    settle(outcome);
    shared.settle(cost);
}

fn do_checkpoint(store: &Store, wal_reported: &mut i64) {
    let started = Instant::now();
    match store.checkpoint() {
        Ok(stats) => {
            let took = started.elapsed();
            let ms = took.as_secs_f64() * 1000.0;
            if took > SLOW_WRITE {
                tracing::warn!(
                    kind = "checkpoint",
                    ms,
                    frames = stats.wal_frames,
                    "slow store checkpoint"
                );
            } else {
                tracing::debug!(
                    ms,
                    frames = stats.wal_frames,
                    checkpointed = stats.checkpointed,
                    busy = stats.busy,
                    "store checkpoint"
                );
            }
            let first = *wal_reported == 0 && stats.wal_bytes > WAL_WARN_BYTES;
            let doubled = *wal_reported > 0 && stats.wal_bytes > wal_reported.saturating_mul(2);
            if first || doubled {
                *wal_reported = stats.wal_bytes;
                tracing::error!(
                    wal_mb = stats.wal_bytes / (1 << 20),
                    "the store's WAL keeps growing (a reader holds old pages?); handing \
                     checkpoints back to SQLite"
                );
                if first {
                    if let Err(e) = store.set_autocheckpoint(AUTOCHECKPOINT_PAGES) {
                        tracing::error!(error = %e, "could not turn automatic checkpoints back on");
                    }
                }
            }
        }
        Err(e) => tracing::warn!(error = %e, "store checkpoint failed"),
    }
}

/// Bytes `env` encodes to, without keeping the encoding: what it is counted as in the queue.
fn encoded_len(env: &Envelope) -> usize {
    struct Count(usize);
    impl std::io::Write for Count {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            self.0 += buf.len();
            Ok(buf.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }
    let mut count = Count(0);
    match serde_json::to_writer(&mut count, env) {
        Ok(()) => count.0,
        Err(_) => DEFAULT_COST,
    }
}

/// What the app holds to reach the store from its main thread.
#[derive(Clone)]
pub enum StoreHandle {
    /// One store that runs writes in place: the memory-only fallback, the demo, tests.
    Inline(Rc<Store>),
    /// Writes go to the writer; `reader` is a connection of this thread, for reads only.
    Queued {
        reader: Rc<Store>,
        writer: StoreWriter,
    },
}

impl From<Rc<Store>> for StoreHandle {
    fn from(store: Rc<Store>) -> Self {
        Self::Inline(store)
    }
}

impl StoreHandle {
    /// The connection reads run on. No barrier: what the writer has not committed yet is not
    /// there. Use [`StoreHandle::read`] when a read must see this thread's own writes.
    pub fn reader(&self) -> &Store {
        match self {
            Self::Inline(store) => store,
            Self::Queued { reader, .. } => reader,
        }
    }

    pub fn writer(&self) -> Option<&StoreWriter> {
        match self {
            Self::Inline(_) => None,
            Self::Queued { writer, .. } => Some(writer),
        }
    }

    /// Runs `f` on the reader once every write queued so far is committed. Blocks the calling
    /// thread until then, at most [`READ_FLUSH_WAIT`]; past that it does not read at all
    /// ([`StoreError::Behind`]): a stale read could hand off a thread without its last turn.
    /// Keep it to reads that need their own writes and cannot wait asynchronously.
    pub fn read<T>(&self, f: impl FnOnce(&Store) -> T) -> Result<T> {
        if let Self::Queued { writer, .. } = self {
            let started = Instant::now();
            let caught_up = writer.flush(READ_FLUSH_WAIT);
            let took = started.elapsed();
            if took > SLOW_WRITE {
                tracing::warn!(
                    kind = "read barrier",
                    ms = took.as_secs_f64() * 1000.0,
                    "slow wait for the store writer"
                );
            }
            if !caught_up {
                tracing::error!("the store writer did not catch up; not reading stale rows");
                return Err(StoreError::Behind);
            }
        }
        Ok(f(self.reader()))
    }

    /// Runs `job` now (inline) or queues it (writer). Queued, `Ok` means accepted: a failure
    /// later is logged and counted by the writer.
    pub fn write(
        &self,
        what: &'static str,
        thread: Option<&str>,
        job: impl FnOnce(&Store) -> Result<()> + Send + 'static,
    ) -> Result<()> {
        match self {
            // As the writer runs it: all of it or none of it.
            Self::Inline(store) => store.atomically(job),
            Self::Queued { writer, .. } => writer.write(what, thread, job),
        }
    }

    /// A fresh id for a row a queued write will create.
    pub fn new_id(&self) -> Result<String> {
        self.reader().new_id()
    }

    /// Removes app metadata on the same writer as the thread's events.
    pub fn delete_meta(&self, key: &str) -> Result<()> {
        let key = key.to_owned();
        self.write("remove app metadata", None, move |s| s.delete_meta(&key))
    }

    pub fn append_event(
        &self,
        thread: &str,
        provider_thread: Option<&str>,
        env: &Envelope,
    ) -> Result<()> {
        let writer = match self {
            Self::Inline(store) => {
                return store.append_event(thread, provider_thread, env).map(drop)
            }
            Self::Queued { writer, .. } => writer,
        };
        writer.append_event(thread, provider_thread, env)
    }

    /// Stores a user prompt; returns its item id (made here, so it is known before the write).
    pub fn append_user_message(
        &self,
        thread: &str,
        provider_thread: Option<&str>,
        text: &str,
    ) -> Result<String> {
        if let Self::Inline(store) = self {
            return store.append_user_message(thread, provider_thread, text);
        }
        let item = self.new_id()?;
        let (t, pt, i, text) = (
            thread.to_owned(),
            provider_thread.map(str::to_owned),
            item.clone(),
            text.to_owned(),
        );
        self.write("store a message", Some(thread), move |s| {
            s.append_user_message_with_id(&t, pt.as_deref(), &i, &text)
        })?;
        Ok(item)
    }

    /// Stores a queued prompt under the item id already shown in the view.
    pub fn append_user_message_with_id(
        &self,
        thread: &str,
        provider_thread: Option<&str>,
        item: &str,
        text: &str,
    ) -> Result<()> {
        let (t, pt, i, text) = (
            thread.to_owned(),
            provider_thread.map(str::to_owned),
            item.to_owned(),
            text.to_owned(),
        );
        self.write("store a message", Some(thread), move |s| {
            s.append_user_message_with_id(&t, pt.as_deref(), &i, &text)
        })
    }

    pub fn mark_read(&self, thread: &str) -> Result<()> {
        let t = thread.to_owned();
        self.write("mark the thread read", Some(thread), move |s| {
            s.mark_read(&t)
        })
    }

    pub fn set_native_id(&self, provider_thread: &str, native_id: &str) -> Result<()> {
        let (pt, n) = (provider_thread.to_owned(), native_id.to_owned());
        self.write("record the native session id", None, move |s| {
            s.set_native_id(&pt, &n)
        })
    }

    pub fn dismiss_native(&self, driver: &str, native_id: &str) -> Result<()> {
        let (d, n) = (driver.to_owned(), native_id.to_owned());
        self.write("record a replaced native session", None, move |s| {
            s.dismiss_native(&d, &n)
        })
    }

    pub fn set_provider_model(&self, provider_thread: &str, model: &str) -> Result<()> {
        let (pt, m) = (provider_thread.to_owned(), model.to_owned());
        self.write("record the thread's model", None, move |s| {
            s.set_provider_model(&pt, &m)
        })
    }

    pub fn set_active_provider_thread(&self, thread: &str, provider_thread: &str) -> Result<()> {
        let (t, pt) = (thread.to_owned(), provider_thread.to_owned());
        self.write("switch the thread's agent", Some(thread), move |s| {
            s.set_active_provider_thread(&t, &pt)
        })
    }

    /// A new provider thread for `thread` on `driver`/`model`, made the active one. Queued as
    /// one write: both rows or neither.
    pub fn create_provider_thread(
        &self,
        thread: &str,
        driver: &str,
        model: &str,
    ) -> Result<ProviderThreadId> {
        let id = self.new_id()?;
        let (t, i, d, m) = (
            thread.to_owned(),
            id.clone(),
            driver.to_owned(),
            model.to_owned(),
        );
        self.write("start the agent's thread", Some(thread), move |s| {
            s.add_provider_thread_with_id(&i, &t, &d, &m)?;
            s.set_active_provider_thread(&t, &i)
        })?;
        Ok(id)
    }

    /// The thread's page closed: its heal tails go (in order after its last writes), and so
    /// does what the writer remembers of its items' admission.
    pub fn forget_thread(&self, thread: &str) {
        let t = thread.to_owned();
        let _ = self.write("forget a closed thread", Some(thread), move |s| {
            s.forget_tails(&t);
            Ok(())
        });
    }
}

/// Test access to the writer's batch logic without its thread: runs `jobs` as the writer would
/// one batch of them, and returns how many failed.
#[cfg(test)]
pub(crate) fn run_jobs_as_batch(store: &Store, jobs: Vec<WriteJob>) -> u64 {
    let shared = Shared {
        closed: Mutex::new(false),
        pending: AtomicUsize::new(jobs.len()),
        queued_bytes: AtomicUsize::new(0),
        failures: AtomicU64::new(0),
        overloaded: AtomicU64::new(0),
        last_failure: Mutex::new(None),
        config: WriterConfig::default(),
        over_bound: std::sync::atomic::AtomicBool::new(false),
    };
    let writes = jobs
        .into_iter()
        .map(|job| Write {
            what: "test job",
            thread: None,
            job,
            deliver: None,
            cost: 0,
            force: false,
            caller_reports: false,
        })
        .collect();
    run_batch(store, &shared, writes);
    shared.failures.load(Ordering::SeqCst)
}

#[cfg(test)]
pub(crate) type TestJob = WriteJob;

#[cfg(test)]
mod tests {
    use super::*;
    use agent_core::event::{Event, ItemKind, StreamKind};

    fn notice(n: usize) -> Envelope {
        Envelope::new(Event::Notice {
            text: format!("n{n}"),
        })
    }

    fn texts(store: &Store, thread: &str) -> Vec<String> {
        store
            .events(thread, None, 100_000)
            .expect("events")
            .into_iter()
            .filter_map(|(_, e)| match e.event {
                Event::Notice { text } => Some(text),
                _ => None,
            })
            .collect()
    }

    fn setup_with(config: WriterConfig) -> (tempfile::TempDir, std::path::PathBuf, StoreHandle) {
        crate::store::quiet_injected_panics();
        let dir = tempfile::tempdir().expect("tmp");
        let path = dir.path().join("threads.db");
        let reader = Rc::new(Store::open(&path).expect("reader"));
        let writer = StoreWriter::spawn_with(&path, config).expect("writer");
        (dir, path, StoreHandle::Queued { reader, writer })
    }

    fn setup() -> (tempfile::TempDir, std::path::PathBuf, StoreHandle) {
        setup_with(WriterConfig::default())
    }

    #[test]
    fn pending_prompt_and_metadata_deletion_follow_the_message_writer() {
        let (_dir, _path, handle) = setup();
        let t = create(&handle);
        let saved = t.clone();
        handle
            .write("save an unsent prompt", Some(&t), move |s| {
                s.set_pending_prompt(&saved, "later")
            })
            .unwrap();
        handle
            .append_user_message_with_id(&t, None, "queued-item", "later")
            .unwrap();
        assert_eq!(
            handle
                .read(|s| s.meta(&crate::store::pending_prompt_key(&t)).unwrap())
                .unwrap(),
            None
        );
        handle
            .write("save test metadata", None, |s| s.set_meta("test", "value"))
            .unwrap();
        handle.delete_meta("test").unwrap();
        assert_eq!(handle.read(|s| s.meta("test").unwrap()).unwrap(), None);
    }

    fn create(handle: &StoreHandle) -> String {
        let t = handle.new_id().expect("id");
        let t2 = t.clone();
        handle
            .write("create", None, move |s| {
                s.create_thread_with_id(&t2, "/", None)
            })
            .expect("queued");
        t
    }

    #[test]
    fn the_writer_connection_checkpoints_on_its_own() {
        let (_dir, _path, handle) = setup();
        let writer = handle.writer().expect("writer").clone();
        let pages = writer.call("read a pragma", |s| s.autocheckpoint().expect("pragma"));
        assert_eq!(
            pages.recv().expect("answer"),
            0,
            "SQLite's autocheckpoint is off"
        );
        // The reader (any other connection) keeps SQLite's default.
        assert_eq!(handle.reader().autocheckpoint().expect("pragma"), 1000);
    }

    #[test]
    fn events_land_in_the_order_they_were_queued_across_threads() {
        let (_dir, path, handle) = setup();
        let writer = handle.writer().expect("writer").clone();
        let a = create(&handle);
        let b = create(&handle);
        // Interleaved from this thread: global order follows the queue.
        for n in 0..200 {
            let t = if n % 2 == 0 { &a } else { &b };
            handle.append_event(t, None, &notice(n)).expect("queued");
        }
        // And from other OS threads, each in its own order.
        let producers: Vec<_> = (0..4)
            .map(|p| {
                let (w, t) = (
                    writer.clone(),
                    if p % 2 == 0 { a.clone() } else { b.clone() },
                );
                std::thread::spawn(move || {
                    for n in 0..100 {
                        let (t2, env) = (t.clone(), notice(1000 * (p + 1) + n));
                        w.write("store an event", Some(&t), move |s| {
                            s.append_event(&t2, None, &env).map(drop)
                        })
                        .expect("queued");
                    }
                })
            })
            .collect();
        for p in producers {
            p.join().expect("producer");
        }
        assert!(writer.flush(Duration::from_secs(10)));
        let check = Store::open(&path).expect("open");
        for (t, parity) in [(&a, 0), (&b, 1)] {
            let got = texts(&check, t);
            let main: Vec<String> = got.iter().filter(|s| s.len() <= 4).cloned().collect();
            let want: Vec<String> = (0..200)
                .filter(|n| n % 2 == parity)
                .map(|n| format!("n{n}"))
                .collect();
            assert_eq!(main, want);
            for p in (0..4).filter(|p| p % 2 == parity) {
                let lo = 1000 * (p + 1);
                let seen: Vec<String> = got
                    .iter()
                    .filter(|s| {
                        s[1..]
                            .parse::<usize>()
                            .is_ok_and(|n| (lo..lo + 100).contains(&n))
                    })
                    .cloned()
                    .collect();
                let want: Vec<String> = (lo..lo + 100).map(|n| format!("n{n}")).collect();
                assert_eq!(seen, want, "producer {p} kept its order");
            }
        }
        assert_eq!(writer.failures().0, 0);
        assert_eq!(writer.pending(), 0);
    }

    #[test]
    fn a_read_after_a_barrier_sees_every_queued_write() {
        let (_dir, _path, handle) = setup();
        let t = create(&handle);
        let pt = handle
            .create_provider_thread(&t, "claude", "opus")
            .expect("queued");
        for n in 0..500 {
            handle
                .append_event(&t, Some(&pt), &notice(n))
                .expect("queued");
        }
        let item = handle
            .append_user_message(&t, Some(&pt), "hello")
            .expect("queued");
        handle.mark_read(&t).expect("queued");
        let (events, summary, active, msgs) = handle
            .read(|s| {
                (
                    s.events(&t, None, 10_000).expect("events").len(),
                    s.thread_summary(&t).expect("summary").expect("exists"),
                    s.active_provider_thread(&t).expect("active"),
                    s.transcript_messages(&t).expect("msgs"),
                )
            })
            .expect("caught up");
        assert_eq!(events, 502);
        assert!(!summary.unread, "the read mark came after every event");
        assert_eq!(active, Some(pt));
        assert_eq!(msgs.last().map(|m| m.item_id.clone()), Some(item));
    }

    #[test]
    fn a_read_the_writer_cannot_catch_up_with_is_refused_not_stale() {
        let (_dir, _path, handle) = setup();
        let writer = handle.writer().expect("writer").clone();
        let t = create(&handle);
        writer
            .write("wait", None, |_| {
                std::thread::sleep(READ_FLUSH_WAIT + Duration::from_millis(500));
                Ok(())
            })
            .expect("queued");
        handle.append_event(&t, None, &notice(1)).expect("queued");
        assert!(matches!(
            handle.read(|s| s.events(&t, None, 10).map(|e| e.len())),
            Err(StoreError::Behind)
        ));
        assert!(writer.flush(Duration::from_secs(10)));
    }

    #[test]
    fn a_call_answers_after_its_batch_commits_and_counts_as_pending() {
        let (_dir, path, handle) = setup();
        let writer = handle.writer().expect("writer").clone();
        let t = create(&handle);
        assert!(writer.flush(Duration::from_secs(10)));
        let t2 = t.clone();
        writer
            .write("wait", None, |_| {
                std::thread::sleep(Duration::from_millis(200));
                Ok(())
            })
            .expect("queued");
        let answer = writer.call("rename", move |s| s.rename_thread(&t2, "renamed").is_ok());
        assert_eq!(
            writer.pending(),
            2,
            "a job counts as pending like any write"
        );
        assert!(answer.recv().expect("answered"));
        // Answered only after the commit: another connection sees it at once.
        let check = Store::open(&path).expect("open");
        assert_eq!(
            check.thread_summary(&t).expect("q").expect("exists").title,
            "renamed"
        );
        // A job that panics answers nothing, and is counted.
        let never = writer.call("panic", |_| -> bool { panic!("a job that panics") });
        assert!(never.recv().is_err());
        assert_eq!(writer.failures().0, 1);
    }

    #[test]
    fn a_failing_job_leaves_nothing_and_hands_its_error_back() {
        let (_dir, path, handle) = setup();
        let writer = handle.writer().expect("writer").clone();
        let t = create(&handle);
        let t2 = t.clone();
        let answer = writer.call_result("half a job", move |s| {
            s.append_event(&t2, None, &notice(1))?;
            s.rename_thread(&t2, "renamed")?;
            s.rename_thread("missing-thread", "x")
        });
        let got = answer.recv().expect("answered");
        assert!(matches!(got, Err(StoreError::NotFound(_))), "{got:?}");
        assert!(writer.flush(Duration::from_secs(10)));
        let check = Store::open(&path).expect("open");
        assert!(
            texts(&check, &t).is_empty(),
            "the job's rows were taken back"
        );
        assert_eq!(
            check.thread_summary(&t).expect("q").expect("exists").title,
            ""
        );
        assert_eq!(
            writer.failures().0,
            0,
            "its caller reports it, not the writer"
        );
        let t3 = t.clone();
        let ok = writer.call_result("a good job", move |s| s.rename_thread(&t3, "ok"));
        assert!(ok.recv().expect("answered").is_ok());
    }

    #[test]
    fn sized_receipt_refuses_oversized_capture_without_executing_it() {
        let (_dir, path, handle) = setup_with(WriterConfig {
            max_queued_bytes: 1024,
            ..WriterConfig::default()
        });
        let writer = handle.writer().unwrap();
        let ran = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let job_ran = ran.clone();
        let answer = writer.call_result_sized("oversized captured frame", usize::MAX, move |_| {
            job_ran.store(true, Ordering::SeqCst);
            Ok(())
        });
        assert!(answer.recv_timeout(Duration::from_secs(2)).is_err());
        assert!(!ran.load(Ordering::SeqCst));
        let job_ran = ran.clone();
        let durable =
            writer.durable_call_sized("oversized durable result", usize::MAX, move |_| {
                job_ran.store(true, Ordering::SeqCst);
                Ok(())
            });
        assert!(matches!(
            durable.recv_timeout(Duration::from_secs(2)).unwrap(),
            Err(StoreError::Overloaded)
        ));
        assert!(!ran.load(Ordering::SeqCst));
        assert!(writer.flush(Duration::from_secs(2)));
        assert_eq!(
            Store::open(&path)
                .unwrap()
                .list_threads(true)
                .unwrap()
                .len(),
            0
        );
    }

    /// Under a tiny bound, a stalled writer: every event of an item is stored (none refused),
    /// a turn's end too; events of no item may be refused, and are counted. Every split secret
    /// is healed, including one a fresh writer finds stored by an earlier run and whose item
    /// starts again.
    #[test]
    fn no_event_of_an_item_is_ever_refused_and_every_split_secret_heals() {
        let dir = tempfile::tempdir().expect("tmp");
        let path = dir.path().join("threads.db");
        let secret = format!("ghp_{}", "abcdefghijklmnopqrstuvwxyz0123");
        let frags = [&secret[..7], &secret[7..15], &secret[15..24], &secret[24..]];
        let delta = |item: &str, text: &str| {
            Envelope::new(Event::ContentDelta {
                stream: StreamKind::Assistant,
                text: text.into(),
            })
            .item(item)
        };
        let start = |item: &str| {
            Envelope::new(Event::ItemStarted {
                kind: ItemKind::AssistantMessage,
                title: String::new(),
                input: None,
                parent: None,
            })
            .item(item)
        };
        let done = |item: &str| {
            Envelope::new(Event::ItemCompleted {
                status: agent_core::event::ItemStatus::Completed,
                output: None,
                error: None,
            })
            .item(item)
        };
        // An earlier run stored half of "old"'s secret and never completed it.
        let t = {
            let earlier = Store::open(&path).expect("earlier run");
            let t = earlier.create_thread("/", None).expect("t");
            for env in [start("old"), delta("old", frags[0]), delta("old", frags[1])] {
                earlier.append_event(&t, None, &env).expect("append");
            }
            t
        };
        crate::store::quiet_injected_panics();
        let reader = Rc::new(Store::open(&path).expect("reader"));
        let config = WriterConfig {
            max_queued: 1,
            ..WriterConfig::default()
        };
        let writer = StoreWriter::spawn_with(&path, config).expect("writer");
        let handle = StoreHandle::Queued {
            reader,
            writer: writer.clone(),
        };
        writer
            .write("wait", None, |_| {
                std::thread::sleep(Duration::from_millis(300));
                Ok(())
            })
            .expect("queued");
        let mut sent = 0;
        let mut send = |env: Envelope| {
            sent += 1;
            handle
                .append_event(&t, None, &env)
                .expect("an item's event is never refused");
        };
        // The old item starts again in this run, gets the rest, completes.
        send(start("old"));
        send(delta("old", frags[2]));
        send(delta("old", frags[3]));
        send(done("old"));
        // New items, interleaved.
        let items: Vec<String> = (0..4).map(|n| format!("it{n}")).collect();
        for item in &items {
            send(start(item));
        }
        for frag in frags {
            for item in &items {
                send(delta(item, frag));
            }
        }
        for item in &items {
            send(done(item));
        }
        // Events of no item may be refused, and are counted.
        let refused = (0..5)
            .filter(|n| handle.append_event(&t, None, &notice(*n)).is_err())
            .count();
        assert!(refused > 0);
        assert_eq!(writer.overloaded(), refused as u64);
        let turn = Envelope::new(Event::TurnCompleted {
            state: agent_core::event::TurnState::Completed,
            usage: None,
            cost_usd: None,
            error: None,
        });
        handle
            .append_event(&t, None, &turn)
            .expect("a turn's end is never refused");
        assert!(writer.flush(Duration::from_secs(10)));
        let check = Store::open(&path).expect("open");
        let stored = check.events(&t, None, 1000).expect("events");
        let item_rows = stored.iter().filter(|(_, e)| e.item.is_some()).count();
        assert_eq!(item_rows, 3 + sent, "every item event is stored");
        let all = serde_json::to_string(&stored).expect("json");
        // The failure names the fragment by position, never its text (secret-shaped).
        for (n, frag) in frags.iter().enumerate() {
            assert!(!all.contains(frag), "fragment {n} survived");
        }
        assert_eq!(
            all.matches("****0123").count(),
            1 + items.len(),
            "every secret healed"
        );
    }

    #[test]
    fn shutdown_drains_the_queue_first() {
        let (_dir, path, handle) = setup();
        let writer = handle.writer().expect("writer").clone();
        let t = create(&handle);
        // A slow first write keeps the rest queued when shutdown is asked for.
        writer
            .write("wait", None, |_| {
                std::thread::sleep(Duration::from_millis(200));
                Ok(())
            })
            .expect("queued");
        for n in 0..1000 {
            handle.append_event(&t, None, &notice(n)).expect("queued");
        }
        assert_eq!(writer.shutdown(Duration::from_secs(20)), Shutdown::Drained);
        assert_eq!(writer.pending(), 0);
        let check = Store::open(&path).expect("open");
        assert_eq!(texts(&check, &t).len(), 1000);
        // Writes after the writer stopped are refused and counted, never lost silently.
        assert!(handle.append_event(&t, None, &notice(1)).is_err());
        assert_eq!(writer.failures().0, 1);
        // A second shutdown is not reported as a clean drain.
        assert_eq!(
            writer.shutdown(Duration::from_secs(1)),
            Shutdown::WriterGone
        );
    }

    #[test]
    fn a_shutdown_that_runs_out_of_time_says_so() {
        let (_dir, _path, handle) = setup();
        let writer = handle.writer().expect("writer").clone();
        writer
            .write("wait", None, |_| {
                std::thread::sleep(Duration::from_millis(600));
                Ok(())
            })
            .expect("queued");
        assert_eq!(
            writer.shutdown(Duration::from_millis(50)),
            Shutdown::TimedOut
        );
    }

    #[test]
    fn a_full_queue_refuses_writes_at_once_and_counts_them() {
        let config = WriterConfig {
            max_queued: 10,
            ..WriterConfig::default()
        };
        let (_dir, path, handle) = setup_with(config);
        let writer = handle.writer().expect("writer").clone();
        let t = create(&handle);
        assert!(writer.flush(Duration::from_secs(10)));
        writer
            .write("wait", None, |_| {
                std::thread::sleep(Duration::from_millis(300));
                Ok(())
            })
            .expect("queued");
        let started = Instant::now();
        let taken: Vec<usize> = (0..30)
            .filter(|n| handle.append_event(&t, None, &notice(*n)).is_ok())
            .collect();
        assert!(
            started.elapsed() < Duration::from_millis(100),
            "never blocks"
        );
        assert_eq!(taken.len(), 9, "the wait job holds one slot");
        assert_eq!(writer.overloaded(), 21);
        assert_eq!(writer.failures().0, 21);
        assert!(writer
            .failures()
            .1
            .is_some_and(|l| l.contains("too far behind")));
        assert!(writer.flush(Duration::from_secs(10)));
        // What was taken kept its order.
        let check = Store::open(&path).expect("open");
        let want: Vec<String> = taken.iter().map(|n| format!("n{n}")).collect();
        assert_eq!(texts(&check, &t), want);
    }

    #[test]
    fn a_failed_write_is_counted_and_said_and_its_neighbours_still_commit() {
        let (_dir, path, handle) = setup();
        let writer = handle.writer().expect("writer").clone();
        let t = create(&handle);
        handle.append_event(&t, None, &notice(1)).expect("queued");
        // A thread that does not exist: the append fails on the writer.
        handle
            .append_event("missing-thread", None, &notice(2))
            .expect("accepted");
        writer
            .write("panic", None, |_| panic!("a job that panics"))
            .expect("queued");
        // A job that fails part-way leaves nothing behind.
        let t3 = t.clone();
        writer
            .write("half a job", None, move |s| {
                s.append_event(&t3, None, &notice(99))?;
                s.append_event("missing-thread", None, &notice(98))
                    .map(drop)
            })
            .expect("queued");
        handle.append_event(&t, None, &notice(3)).expect("queued");
        assert!(writer.flush(Duration::from_secs(10)));
        let (count, last) = writer.failures();
        assert_eq!(count, 3, "{last:?}");
        let check = Store::open(&path).expect("open");
        assert_eq!(texts(&check, &t), vec!["n1", "n3"]);
        let failing = handle.append_event("missing-thread", None, &notice(4));
        assert!(failing.is_ok());
        assert!(writer.flush(Duration::from_secs(10)));
        let (count, last) = writer.failures();
        assert_eq!(count, 4);
        assert!(
            last.as_deref()
                .is_some_and(|l| l.starts_with("Could not store an event: not found")),
            "{last:?}"
        );
    }

    #[test]
    fn a_split_secret_is_healed_on_the_writer() {
        let (_dir, path, handle) = setup();
        let t = create(&handle);
        let secret = format!("ghp_{}", "abcdefghijklmnopqrstuvwxyz0123");
        let (a, b) = secret.split_at(10);
        let delta = |text: &str| {
            Envelope::new(Event::ContentDelta {
                stream: StreamKind::Assistant,
                text: text.into(),
            })
            .item("a1")
        };
        for env in [
            Envelope::new(Event::ItemStarted {
                kind: ItemKind::AssistantMessage,
                title: String::new(),
                input: None,
                parent: None,
            })
            .item("a1"),
            delta(&format!("token {a}")),
            delta(&format!("{b} done")),
            Envelope::new(Event::ItemCompleted {
                status: agent_core::event::ItemStatus::Completed,
                output: None,
                error: None,
            })
            .item("a1"),
        ] {
            handle.append_event(&t, None, &env).expect("queued");
        }
        assert!(handle
            .writer()
            .expect("writer")
            .flush(Duration::from_secs(10)));
        let check = Store::open(&path).expect("open");
        let all = serde_json::to_string(&check.events(&t, None, 100).expect("ev")).expect("json");
        assert!(!all.contains(a) && !all.contains(b), "{all}");
    }

    #[test]
    fn an_inline_handle_writes_in_place() {
        let store = Rc::new(Store::open_in_memory().expect("store"));
        let handle = StoreHandle::from(store.clone());
        let t = store.create_thread("/", None).expect("thread");
        let pt = handle
            .create_provider_thread(&t, "agy", "gemini")
            .expect("pt");
        handle
            .append_event(&t, Some(&pt), &notice(1))
            .expect("append");
        assert_eq!(store.events(&t, None, 10).expect("events").len(), 1);
        assert_eq!(store.active_provider_thread(&t).expect("a"), Some(pt));
        assert!(handle.append_event("nope", None, &notice(2)).is_err());
        assert!(handle.writer().is_none());
        assert_eq!(
            handle
                .read(|s| s.events(&t, None, 10).map(|e| e.len()))
                .expect("read")
                .expect("events"),
            1
        );
    }

    /// `meta(key)` as another connection sees it: committed rows only.
    fn committed_meta(path: &Path, key: &str) -> Option<String> {
        let conn =
            rusqlite::Connection::open_with_flags(path, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)
                .expect("read-only");
        conn.query_row("SELECT value FROM meta WHERE key = ?1", [key], |r| r.get(0))
            .ok()
    }

    fn sync_level(writer: &StoreWriter) -> i64 {
        writer
            .call("read synchronous", |s| s.synchronous().expect("pragma"))
            .recv()
            .expect("answer")
    }

    #[test]
    fn a_durable_call_commits_under_full_after_the_open_batch_and_restores_normal() {
        let (_dir, path, handle) = setup();
        let writer = handle.writer().expect("writer").clone();
        assert_eq!(sync_level(&writer), 1, "the writer runs NORMAL");
        // Queued together: a normal write, the durable one, another normal write.
        writer
            .write("before", None, |s| s.set_meta("before", "1"))
            .expect("queued");
        let probe = path.clone();
        let durable = writer.durable_call("reserve a dispatch", move |s| {
            // The batch before it is already committed (another connection sees it), and this
            // runs in a transaction of its own, under FULL.
            let seen_before = committed_meta(&probe, "before");
            s.set_meta("durable", "1")?;
            Ok((seen_before, s.synchronous()?))
        });
        writer
            .write("after", None, |s| s.set_meta("after", "1"))
            .expect("queued");
        let (seen_before, level) = durable.recv().expect("answer").expect("durable");
        assert_eq!(
            seen_before.as_deref(),
            Some("1"),
            "the open batch committed first"
        );
        assert_eq!(level, 2, "FULL inside the control transaction");
        assert_eq!(committed_meta(&path, "durable").as_deref(), Some("1"));
        assert!(writer.flush(Duration::from_secs(10)));
        assert_eq!(sync_level(&writer), 1, "NORMAL restored");
        assert_eq!(committed_meta(&path, "after").as_deref(), Some("1"));
        assert_eq!(writer.failures().0, 0);
    }

    #[test]
    fn a_durable_write_never_shares_a_batch_with_normal_writes() {
        let (_dir, path, handle) = setup();
        let writer = handle.writer().expect("writer").clone();
        // Hold the writer so all three land in one receive.
        writer
            .write("wait", None, |_| {
                std::thread::sleep(Duration::from_millis(200));
                Ok(())
            })
            .expect("queued");
        writer
            .write("before", None, |s| s.set_meta("before", "1"))
            .expect("queued");
        let durable = writer.durable_call("control", |s| s.set_meta("durable", "1"));
        // The batch after it fails to commit: the durable write, already committed alone, stays.
        writer
            .write("after", None, |s| {
                s.fail_next_batch_commit();
                s.set_meta("after", "1")
            })
            .expect("queued");
        durable.recv().expect("answer").expect("durable");
        assert!(writer.flush(Duration::from_secs(10)));
        assert_eq!(committed_meta(&path, "before").as_deref(), Some("1"));
        assert_eq!(committed_meta(&path, "durable").as_deref(), Some("1"));
        assert_eq!(committed_meta(&path, "after"), None, "its own batch failed");
        assert_eq!(writer.failures().0, 1, "only the failed batch is counted");
    }

    #[test]
    fn a_failed_synchronous_switch_refuses_the_durable_write_and_is_counted() {
        let (_dir, path, handle) = setup();
        let writer = handle.writer().expect("writer").clone();
        // The switch to FULL fails: the job never runs.
        writer
            .call("inject", |s| s.fail_sync_switch_after(0))
            .recv()
            .expect("armed");
        let refused = writer
            .durable_call("control", |s| s.set_meta("first", "1"))
            .recv()
            .expect("answer");
        assert!(
            matches!(refused, Err(StoreError::Durability(_))),
            "{refused:?}"
        );
        assert_eq!(committed_meta(&path, "first"), None);
        assert_eq!(sync_level(&writer), 1, "still NORMAL");
        assert_eq!(writer.failures().0, 1);

        // The restore fails after the commit: still refused (and said to have committed).
        writer
            .call("inject", |s| s.fail_sync_switch_after(1))
            .recv()
            .expect("armed");
        let refused = writer
            .durable_call("control", |s| s.set_meta("second", "1"))
            .recv()
            .expect("answer");
        assert!(
            matches!(&refused, Err(StoreError::Durability(why)) if why.contains("committed")),
            "{refused:?}"
        );
        assert_eq!(committed_meta(&path, "second").as_deref(), Some("1"));
        assert_eq!(writer.failures().0, 2);
        assert!(writer.failures().1.is_some_and(|l| l.contains("durable")));
        // Never weaker than asked: the level stays FULL until a restore succeeds.
        assert_eq!(sync_level(&writer), 2);
    }

    #[test]
    fn a_failed_durable_commit_rolls_back_restores_the_level_and_is_counted() {
        let (_dir, path, handle) = setup();
        let writer = handle.writer().expect("writer").clone();
        writer
            .call("inject", |s| s.fail_next_txn_commit())
            .recv()
            .expect("armed");
        let got = writer
            .durable_call("control", |s| s.set_meta("lost", "1"))
            .recv()
            .expect("answered");
        assert!(matches!(got, Err(StoreError::Io(_))), "{got:?}");
        assert_eq!(committed_meta(&path, "lost"), None, "rolled back");
        assert_eq!(sync_level(&writer), 1, "NORMAL restored");
        assert_eq!(writer.failures().0, 1);
        // The next durable write commits normally.
        writer
            .durable_call("control", |s| s.set_meta("kept", "1"))
            .recv()
            .expect("answered")
            .expect("committed");
        assert_eq!(committed_meta(&path, "kept").as_deref(), Some("1"));
    }

    #[test]
    fn a_durable_jobs_own_error_goes_back_uncounted_and_leaves_nothing() {
        let (_dir, path, handle) = setup();
        let writer = handle.writer().expect("writer").clone();
        let got = writer
            .durable_call("control", |s| {
                s.set_meta("partial", "1")?;
                s.rename_thread("missing", "x")
            })
            .recv()
            .expect("answer");
        assert!(matches!(got, Err(StoreError::NotFound(_))), "{got:?}");
        assert_eq!(committed_meta(&path, "partial"), None);
        assert_eq!(writer.failures().0, 0, "its caller reports it");
        assert_eq!(sync_level(&writer), 1, "NORMAL restored");
        let panicked = writer
            .durable_call("control", |_| -> Result<()> { panic!("a job that panics") })
            .recv()
            .expect("a panic still answers");
        assert!(panicked.is_err());
        assert_eq!(writer.failures().0, 1);
        assert_eq!(sync_level(&writer), 1, "restored after a panic too");
    }

    #[test]
    fn a_durable_call_on_a_full_queue_is_refused_at_once_and_counted() {
        let config = WriterConfig {
            max_queued: 2,
            ..WriterConfig::default()
        };
        let (_dir, path, handle) = setup_with(config);
        let writer = handle.writer().expect("writer").clone();
        writer
            .write("wait", None, |_| {
                std::thread::sleep(Duration::from_millis(300));
                Ok(())
            })
            .expect("queued");
        writer
            .write("fill", None, |s| s.set_meta("fill", "1"))
            .expect("queued");
        let started = Instant::now();
        let refused = writer
            .durable_call("control", |s| s.set_meta("refused", "1"))
            .recv()
            .expect("answered at once");
        assert!(
            started.elapsed() < Duration::from_millis(200),
            "never blocks"
        );
        assert!(
            matches!(refused, Err(StoreError::Overloaded)),
            "{refused:?}"
        );
        assert_eq!(writer.overloaded(), 1);
        assert_eq!(writer.failures().0, 1);
        assert!(writer.flush(Duration::from_secs(10)));
        assert_eq!(committed_meta(&path, "refused"), None);
        writer.shutdown(Duration::from_secs(10));
        let gone = writer
            .durable_call("control", |s| s.set_meta("late", "1"))
            .recv()
            .expect("answered");
        assert!(matches!(gone, Err(StoreError::WriterGone)), "{gone:?}");
        assert_eq!(writer.failures().0, 2);
    }
}
