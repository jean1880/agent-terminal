//! The store writer: one thread that owns the store's write connection.
//!
//! Every write the app makes on its main thread (an event as it streams, a prompt, a title, the
//! read mark, …) is queued here instead of run in place, so the main thread never waits on
//! SQLite: not on the disk, not on a checkpoint, not on another connection's lock. The writer runs
//! what is queued in order, many writes to one transaction ([`Store::batch`], each write under a
//! savepoint of its own so one failure does not take its neighbours with it).
//!
//! - **Order.** One queue, one connection: writes land in the order they were queued, whichever
//!   app thread queued them.
//! - **Read-after-write.** [`StoreWriter::barrier`] answers once everything queued before it is
//!   committed; a reader waits on it, then reads on a connection of its own (WAL shows it every
//!   committed row). [`StoreWriter::call`] runs a job on the writer's own connection, in order.
//! - **Errors.** A write that fails is logged with what it was and its thread, and counted
//!   ([`StoreWriter::failures`]) for the app to show. Nothing is dropped silently.
//! - **Checkpoints.** SQLite's automatic checkpoint is off on this connection: the writer
//!   checkpoints (PASSIVE) every [`Timing::checkpoint_every`] while there were writes, and
//!   whenever the queue has been idle for [`Timing::idle`]. With `synchronous=NORMAL` in WAL mode
//!   a commit reaches the disk at the next checkpoint (or WAL sync), so a power cut can lose the
//!   commits since the last checkpoint: at most about `checkpoint_every` of writes. It cannot
//!   corrupt the database. If the WAL still grows past [`WAL_WARN_BYTES`] (a reader pinning old
//!   pages), the writer says so and hands checkpoints back to SQLite.
//! - **Shutdown.** [`StoreWriter::shutdown`] drains the queue and closes the connection; SQLite
//!   checkpoints on the last close. No TRUNCATE: it would wait for readers.
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

use agent_core::event::Envelope;

use crate::store::{ProviderThreadId, Result, Store, StoreError, SLOW_WRITE};

/// Past this many bytes of WAL the writer warns and turns SQLite's own checkpoints back on.
pub const WAL_WARN_BYTES: i64 = 64 << 20;
/// SQLite's default automatic checkpoint threshold, restored by the WAL guard.
const AUTOCHECKPOINT_PAGES: i64 = 1000;
/// Most writes committed in one transaction.
const MAX_BATCH: usize = 512;
/// How long a read on the calling thread waits for the writer to catch up
/// ([`StoreHandle::read`]) before it reads what is committed and says so.
pub const READ_FLUSH_WAIT: Duration = Duration::from_secs(5);

/// When the writer checkpoints.
#[derive(Debug, Clone, Copy)]
pub struct Timing {
    /// At most this long between checkpoints while there are writes.
    pub checkpoint_every: Duration,
    /// A queue idle this long, with writes since the last checkpoint, checkpoints.
    pub idle: Duration,
}

impl Default for Timing {
    fn default() -> Self {
        Self {
            checkpoint_every: Duration::from_secs(30),
            idle: Duration::from_secs(5),
        }
    }
}

type WriteJob = Box<dyn FnOnce(&Store) -> Result<()> + Send>;
type CallJob = Box<dyn FnOnce(&Store) + Send>;

struct Write {
    what: &'static str,
    thread: Option<String>,
    job: WriteJob,
}

enum Op {
    Write(Write),
    Call(CallJob),
    Barrier(Sender<()>),
    Shutdown(Sender<()>),
}

#[derive(Default)]
struct Shared {
    /// Set (under the lock) once the writer takes its last look at the queue: a write sent
    /// after that would be lost, so it is refused instead. Writes send while holding it.
    closed: Mutex<bool>,
    /// Writes queued and not yet committed (or failed).
    pending: AtomicUsize,
    failures: AtomicU64,
    last_failure: Mutex<Option<String>>,
}

impl Shared {
    fn fail(&self, what: &str, thread: Option<&str>, error: &str) {
        self.failures.fetch_add(1, Ordering::SeqCst);
        tracing::error!(what, thread, error, "a store write failed");
        if let Ok(mut last) = self.last_failure.lock() {
            *last = Some(format!("Could not {what}: {error}"));
        }
    }
}

/// A handle on the store writer thread; cheap to clone, and `Send`.
#[derive(Clone)]
pub struct StoreWriter {
    tx: Sender<Op>,
    shared: Arc<Shared>,
}

impl StoreWriter {
    /// Opens the store at `path` on a new writer thread (blocking until it is open).
    pub fn spawn(path: &Path) -> Result<Self> {
        Self::spawn_with(path, Timing::default())
    }

    /// [`StoreWriter::spawn`] with its own checkpoint timing.
    pub fn spawn_with(path: &Path, timing: Timing) -> Result<Self> {
        let (tx, rx) = mpsc::channel();
        let (ready_tx, ready_rx) = mpsc::channel();
        let shared = Arc::new(Shared::default());
        let path = path.to_owned();
        let thread_shared = shared.clone();
        std::thread::Builder::new()
            .name("store-writer".into())
            .spawn(move || {
                let opened = Store::open(&path).and_then(|s| s.set_autocheckpoint(0).map(|()| s));
                match opened {
                    Ok(store) => {
                        let _ = ready_tx.send(Ok(()));
                        run(store, rx, &thread_shared, timing);
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
    /// `Err` only when the writer is gone; a job that fails later is counted in
    /// [`StoreWriter::failures`].
    pub fn write(
        &self,
        what: &'static str,
        thread: Option<&str>,
        job: impl FnOnce(&Store) -> Result<()> + Send + 'static,
    ) -> Result<()> {
        let op = Op::Write(Write {
            what,
            thread: thread.map(str::to_owned),
            job: Box::new(job),
        });
        let sent = match self.shared.closed.lock() {
            Ok(closed) if !*closed => {
                self.shared.pending.fetch_add(1, Ordering::SeqCst);
                self.tx.send(op).is_ok()
            }
            _ => {
                // Counted below as one that never went out.
                self.shared.pending.fetch_add(1, Ordering::SeqCst);
                false
            }
        };
        if !sent {
            self.shared.pending.fetch_sub(1, Ordering::SeqCst);
            self.shared
                .fail(what, thread, "the store writer has stopped");
            return Err(StoreError::WriterGone);
        }
        Ok(())
    }

    /// Runs `job` on the writer's connection after everything queued before it, outside any
    /// batch (so it may open transactions of its own), and returns its result. The receiver
    /// errors when the writer is gone or the job panicked.
    pub fn call<T: Send + 'static>(
        &self,
        job: impl FnOnce(&Store) -> T + Send + 'static,
    ) -> Receiver<T> {
        let (tx, rx) = mpsc::channel();
        let op = Op::Call(Box::new(move |store: &Store| {
            let _ = tx.send(job(store));
        }));
        // A writer that is gone drops the job, and with it the sender: the receiver errors.
        let _ = self.tx.send(op);
        rx
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

    /// Writes queued and not yet committed.
    pub fn pending(&self) -> usize {
        self.shared.pending.load(Ordering::SeqCst)
    }

    /// How many writes failed so far, and what the last one said.
    pub fn failures(&self) -> (u64, Option<String>) {
        let last = self.shared.last_failure.lock().ok().and_then(|l| l.clone());
        (self.shared.failures.load(Ordering::SeqCst), last)
    }

    /// Drains the queue and stops the writer (its connection closes, which checkpoints), waiting
    /// at most `wait`; whether it finished. Writes queued afterwards fail as "stopped".
    pub fn shutdown(&self, wait: Duration) -> bool {
        let (tx, rx) = mpsc::channel();
        if self.tx.send(Op::Shutdown(tx)).is_err() {
            return true; // already gone
        }
        rx.recv_timeout(wait).is_ok()
    }
}

fn run(owned: Store, rx: Receiver<Op>, shared: &Shared, timing: Timing) {
    let store = &owned;
    let mut dirty = false;
    let mut last_checkpoint = Instant::now();
    let mut guard_tripped = false;
    let mut checkpoint = |dirty: &mut bool, last: &mut Instant| {
        do_checkpoint(store, &mut guard_tripped);
        *dirty = false;
        *last = Instant::now();
    };
    loop {
        let first = match rx.recv_timeout(timing.idle) {
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
            drop(owned);
            tracing::debug!("store writer: queue drained, connection closed");
            for done in stop {
                let _ = done.send(());
            }
            return;
        }
        if dirty && last_checkpoint.elapsed() >= timing.checkpoint_every {
            checkpoint(&mut dirty, &mut last_checkpoint);
        }
    }
}

/// Runs `ops` in order: consecutive writes as one batch, the rest one by one. Whether anything
/// may have been written.
fn process(store: &Store, shared: &Shared, ops: Vec<Op>, stop: &mut Vec<Sender<()>>) -> bool {
    let mut wrote = false;
    let mut writes = Vec::new();
    for op in ops {
        match op {
            Op::Write(w) => writes.push(w),
            Op::Call(job) => {
                commit(store, shared, &mut writes);
                wrote = true;
                let started = Instant::now();
                if std::panic::catch_unwind(AssertUnwindSafe(|| job(store))).is_err() {
                    tracing::error!("a store job panicked on the writer");
                }
                let took = started.elapsed();
                if took > SLOW_WRITE {
                    tracing::warn!(
                        kind = "job",
                        ms = took.as_secs_f64() * 1000.0,
                        "slow store job"
                    );
                }
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

/// Commits `writes` as one batch; failures are reported. Whether there was anything to write.
fn commit(store: &Store, shared: &Shared, writes: &mut Vec<Write>) -> bool {
    if writes.is_empty() {
        return false;
    }
    let batch: Vec<Write> = std::mem::take(writes);
    let n = batch.len();
    let whats: Vec<(&'static str, Option<String>)> =
        batch.iter().map(|w| (w.what, w.thread.clone())).collect();
    let started = Instant::now();
    let mut failed = Vec::new();
    let outcome = store.batch(|s| {
        for w in batch {
            let one = Instant::now();
            let result = std::panic::catch_unwind(AssertUnwindSafe(|| (w.job)(s)));
            let took = one.elapsed();
            if took > SLOW_WRITE {
                tracing::warn!(
                    kind = w.what,
                    ms = took.as_secs_f64() * 1000.0,
                    "slow store write"
                );
            }
            match result {
                Ok(Ok(())) => {}
                Ok(Err(e)) => failed.push((w.what, w.thread, e.to_string())),
                Err(_) => failed.push((w.what, w.thread, "the write panicked".to_owned())),
            }
        }
    });
    match outcome {
        Ok(()) => {
            for (what, thread, error) in &failed {
                shared.fail(what, thread.as_deref(), error);
            }
        }
        Err(e) => {
            // Nothing of the batch is stored: every write in it failed.
            let error = format!("the batch could not be committed: {e}");
            for (what, thread) in &whats {
                shared.fail(what, thread.as_deref(), &error);
            }
        }
    }
    shared.pending.fetch_sub(n, Ordering::SeqCst);
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

fn do_checkpoint(store: &Store, guard_tripped: &mut bool) {
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
            if stats.wal_bytes > WAL_WARN_BYTES && !*guard_tripped {
                *guard_tripped = true;
                tracing::warn!(
                    wal_mb = stats.wal_bytes / (1 << 20),
                    "the store's WAL is past 64 MB; handing checkpoints back to SQLite"
                );
                if let Err(e) = store.set_autocheckpoint(AUTOCHECKPOINT_PAGES) {
                    tracing::warn!(error = %e, "could not turn automatic checkpoints back on");
                }
            }
        }
        Err(e) => tracing::warn!(error = %e, "store checkpoint failed"),
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
    /// thread until then (at most [`READ_FLUSH_WAIT`], after which it reads what is committed
    /// and warns): keep it to reads that need their own writes and cannot wait asynchronously.
    pub fn read<T>(&self, f: impl FnOnce(&Store) -> T) -> T {
        if let Self::Queued { writer, .. } = self {
            let started = Instant::now();
            if !writer.flush(READ_FLUSH_WAIT) {
                tracing::warn!("reading the store before its writer caught up");
            }
            let took = started.elapsed();
            if took > SLOW_WRITE {
                tracing::warn!(
                    kind = "read barrier",
                    ms = took.as_secs_f64() * 1000.0,
                    "slow wait for the store writer"
                );
            }
        }
        f(self.reader())
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
            Self::Inline(store) => job(store),
            Self::Queued { writer, .. } => writer.write(what, thread, job),
        }
    }

    /// A fresh id for a row a queued write will create.
    pub fn new_id(&self) -> Result<String> {
        self.reader().new_id()
    }

    pub fn append_event(
        &self,
        thread: &str,
        provider_thread: Option<&str>,
        env: &Envelope,
    ) -> Result<()> {
        if let Self::Inline(store) = self {
            return store.append_event(thread, provider_thread, env).map(drop);
        }
        let (t, pt, env) = (
            thread.to_owned(),
            provider_thread.map(str::to_owned),
            env.clone(),
        );
        self.write("store an event", Some(thread), move |s| {
            s.append_event(&t, pt.as_deref(), &env).map(drop)
        })
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

    /// A new provider thread for `thread` on `driver`/`model`, made the active one.
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

    /// The thread's page closed: its heal tails go (in order after its last writes).
    pub fn forget_thread(&self, thread: &str) {
        let t = thread.to_owned();
        let _ = self.write("forget a closed thread", Some(thread), move |s| {
            s.forget_tails(&t);
            Ok(())
        });
    }
}

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

    fn setup() -> (tempfile::TempDir, std::path::PathBuf, StoreHandle) {
        let dir = tempfile::tempdir().expect("tmp");
        let path = dir.path().join("threads.db");
        let reader = Rc::new(Store::open(&path).expect("reader"));
        let writer = StoreWriter::spawn(&path).expect("writer");
        (dir, path, StoreHandle::Queued { reader, writer })
    }

    #[test]
    fn the_writer_connection_checkpoints_on_its_own() {
        let (_dir, _path, handle) = setup();
        let writer = handle.writer().expect("writer").clone();
        let pages = writer.call(|s| s.autocheckpoint().expect("pragma"));
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
        let a = handle.new_id().expect("id");
        let b = handle.new_id().expect("id");
        for t in [&a, &b] {
            let t2 = t.clone();
            writer
                .write("create", None, move |s| {
                    s.create_thread_with_id(&t2, "/", None)
                })
                .expect("queued");
        }
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
        let t = handle.new_id().expect("id");
        let t2 = t.clone();
        handle
            .write("create", None, move |s| {
                s.create_thread_with_id(&t2, "/w", Some("x"))
            })
            .expect("queued");
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
        let (events, summary, active, msgs) = handle.read(|s| {
            (
                s.events(&t, None, 10_000).expect("events").len(),
                s.thread_summary(&t).expect("summary").expect("exists"),
                s.active_provider_thread(&t).expect("active"),
                s.transcript_messages(&t).expect("msgs"),
            )
        });
        assert_eq!(events, 502);
        assert!(!summary.unread, "the read mark came after every event");
        assert_eq!(active, Some(pt));
        assert_eq!(msgs.last().map(|m| m.item_id.clone()), Some(item));
    }

    #[test]
    fn shutdown_drains_the_queue_first() {
        let (_dir, path, handle) = setup();
        let writer = handle.writer().expect("writer").clone();
        let t = handle.new_id().expect("id");
        let t2 = t.clone();
        handle
            .write("create", None, move |s| {
                s.create_thread_with_id(&t2, "/", None)
            })
            .expect("queued");
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
        assert!(writer.shutdown(Duration::from_secs(20)), "drained in time");
        assert_eq!(writer.pending(), 0);
        let check = Store::open(&path).expect("open");
        assert_eq!(texts(&check, &t).len(), 1000);
        // Writes after the writer stopped are refused and counted, never lost silently.
        assert!(handle.append_event(&t, None, &notice(1)).is_err());
        assert_eq!(writer.failures().0, 1);
    }

    #[test]
    fn a_failed_write_is_counted_and_said_and_its_neighbours_still_commit() {
        let (_dir, path, handle) = setup();
        let writer = handle.writer().expect("writer").clone();
        let t = handle.new_id().expect("id");
        let t2 = t.clone();
        handle
            .write("create", None, move |s| {
                s.create_thread_with_id(&t2, "/", None)
            })
            .expect("queued");
        handle.append_event(&t, None, &notice(1)).expect("queued");
        // A thread that does not exist: the append fails on the writer.
        handle
            .append_event("missing-thread", None, &notice(2))
            .expect("accepted");
        writer
            .write("panic", None, |_| panic!("a job that panics"))
            .expect("queued");
        handle.append_event(&t, None, &notice(3)).expect("queued");
        assert!(writer.flush(Duration::from_secs(10)));
        let (count, last) = writer.failures();
        assert_eq!(count, 2, "{last:?}");
        assert!(
            last.as_deref().is_some_and(|l| l.contains("panic")),
            "{last:?}"
        );
        let check = Store::open(&path).expect("open");
        assert_eq!(texts(&check, &t), vec!["n1", "n3"]);
        // The first failure said what failed.
        let failing = handle.append_event("missing-thread", None, &notice(4));
        assert!(failing.is_ok());
        assert!(writer.flush(Duration::from_secs(10)));
        let (count, last) = writer.failures();
        assert_eq!(count, 3);
        assert!(
            last.as_deref()
                .is_some_and(|l| l.starts_with("Could not store an event: not found")),
            "{last:?}"
        );
    }

    #[test]
    fn a_split_secret_is_healed_on_the_writer() {
        let (_dir, path, handle) = setup();
        let t = handle.new_id().expect("id");
        let t2 = t.clone();
        handle
            .write("create", None, move |s| {
                s.create_thread_with_id(&t2, "/", None)
            })
            .expect("queued");
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
    }
}
