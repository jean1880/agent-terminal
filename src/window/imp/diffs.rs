//! A chat thread's diffs: the baseline taken before each turn, and the [`DiffSource`] the thread's
//! file-change cards ask.
//!
//! At each turn's start the working tree is recorded (a checkpoint, or a bare tree when
//! checkpoints are off) off the main thread. Every file-change item that starts in the turn is
//! tied to that baseline, so "View diff" compares the working file against the state BEFORE the
//! turn, not against the previous checkpoint or HEAD.
//!
//! Ceilings:
//! - The baseline is taken when the turn's start event arrives, a moment after the prompt is
//!   sent; an edit made inside that moment is part of the baseline. Upgrade path: take it before
//!   the prompt is written, holding the send.
//! - Baselines live in memory, per item. After a restart, earlier turns' cards show the agent's
//!   own edit (labelled so) rather than a checkpoint diff.

use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::time::Duration;

use agent_kit::filediff::{self, TurnBase};
use gtk4::glib;
use tracing::{info, warn};

use super::threads::{off_main, wait_until};
use crate::chat::{DiffAsk, DiffReply, DiffSource};
use crate::diff_tool::{self, DiffTools, NewSide, OpenRequest};

/// How long a card waits for a baseline that is still being taken.
const BASE_WAIT: Duration = Duration::from_secs(10);

/// The path an "Open in tool" request carries: a relative one is the thread's, so it is joined to
/// the thread's `dir` (the rule the card's own diff uses) rather than left for the repository
/// root to resolve.
fn path_to_open(dir: &str, path: &str) -> String {
    let p = Path::new(path);
    if p.is_absolute() {
        path.to_owned()
    } else {
        // A relative `dir` is the process's: made absolute first, or the repository root would
        // read the joined path as its own.
        let dir = Path::new(dir)
            .canonicalize()
            .or_else(|_| std::env::current_dir().map(|cwd| cwd.join(dir)))
            .unwrap_or_else(|_| PathBuf::from(dir));
        dir.join(p).to_string_lossy().into_owned()
    }
}

/// One turn's baseline, filled in off the main thread.
#[derive(Default)]
struct BaseSlot {
    state: RefCell<BaseState>,
}

#[derive(Default, Clone)]
enum BaseState {
    #[default]
    Pending,
    Ready(TurnBase),
    /// Not a repository, or git could not record the state.
    Unavailable,
}

impl BaseSlot {
    fn ready(&self) -> Option<TurnBase> {
        match &*self.state.borrow() {
            BaseState::Ready(b) => Some(b.clone()),
            _ => None,
        }
    }

    fn pending(&self) -> bool {
        matches!(&*self.state.borrow(), BaseState::Pending)
    }
}

type Toast = Rc<dyn Fn(&str)>;

/// The diff plumbing of one thread.
pub(super) struct ThreadDiffs {
    dir: String,
    /// The tab key checkpoints are filed under.
    key: u64,
    label: String,
    bases: RefCell<HashMap<String, Rc<BaseSlot>>>,
    current: RefCell<Option<Rc<BaseSlot>>>,
    in_turn: Cell<bool>,
    toast: Toast,
    /// Itself, so the async halves of the [`DiffSource`] methods can keep it alive.
    me: std::rc::Weak<ThreadDiffs>,
}

impl ThreadDiffs {
    pub(super) fn new(dir: &str, key: u64, label: &str, toast: Toast) -> Rc<Self> {
        Rc::new_cyclic(|me| Self {
            me: me.clone(),
            dir: dir.to_owned(),
            key,
            label: label.to_owned(),
            bases: RefCell::default(),
            current: RefCell::default(),
            in_turn: Cell::new(false),
            toast,
        })
    }

    /// A turn started: records the baseline. A second start event inside the same turn (an agent
    /// that re-announces itself) must not replace it with a state that already has the turn's
    /// edits in it.
    pub(super) fn turn_started(&self, keep_ref: bool) {
        if self.in_turn.replace(true) {
            return;
        }
        let slot = Rc::new(BaseSlot::default());
        *self.current.borrow_mut() = Some(slot.clone());
        let (dir, key, label) = (self.dir.clone(), self.key, self.label.clone());
        glib::MainContext::default().spawn_local(async move {
            let taken = gtk4::gio::spawn_blocking(move || {
                filediff::take_turn_base(Path::new(&dir), key, &label, keep_ref)
            })
            .await;
            let state = match taken {
                Ok(Ok(Some(base))) => BaseState::Ready(base),
                Ok(Ok(None)) => BaseState::Unavailable,
                Ok(Err(why)) => {
                    warn!("could not record the pre-turn state: {why}");
                    BaseState::Unavailable
                }
                Err(_) => BaseState::Unavailable,
            };
            *slot.state.borrow_mut() = state;
        });
    }

    /// The turn (or the session) ended.
    pub(super) fn turn_ended(&self) {
        self.in_turn.set(false);
    }

    /// A file-change item started: it belongs to the turn in progress.
    pub(super) fn item_started(&self, item: &str) {
        if let Some(slot) = self.current.borrow().clone().filter(|_| self.in_turn.get()) {
            self.bases
                .borrow_mut()
                .entry(item.to_owned())
                .or_insert(slot);
        }
    }

    /// The item's baseline, once it is known (waits for one still being taken).
    async fn base_of(&self, item: &str) -> Option<TurnBase> {
        let slot = self.bases.borrow().get(item).cloned()?;
        wait_until(BASE_WAIT, Duration::from_millis(100), || !slot.pending()).await;
        slot.ready()
    }

    #[cfg(test)]
    fn bound_items(&self) -> usize {
        self.bases.borrow().len()
    }

    #[cfg(test)]
    fn set_ready_for_test(&self, base: TurnBase) {
        let slot = Rc::new(BaseSlot::default());
        *slot.state.borrow_mut() = BaseState::Ready(base);
        *self.current.borrow_mut() = Some(slot);
        self.in_turn.set(true);
    }
}

impl DiffSource for ThreadDiffs {
    fn load(&self, ask: DiffAsk, done: Box<dyn FnOnce(DiffReply)>) {
        let Some(me) = self.me.upgrade() else { return };
        let dir = self.dir.clone();
        glib::MainContext::default().spawn_local(async move {
            let base = me.base_of(&ask.item).await;
            let reply = off_main("computing the diff", move || {
                filediff::shown_for_item_in(base.as_ref(), &ask.input, Some(Path::new(&dir)))
            })
            .await;
            done(reply);
        });
    }

    fn open_external(&self, ask: DiffAsk) {
        let Some(me) = self.me.upgrade() else { return };
        let dir = self.dir.clone();
        let Some(tool) = DiffTools::shared().get() else {
            (self.toast)(diff_tool::NO_TOOL_HINT);
            return;
        };
        // A multi-file edit (Codex) opens its first file; the diff panel lists the rest.
        let path = agent_kit::editdiff::paths_from_input(&ask.input)
            .into_iter()
            .next()
            .or_else(|| {
                agent_kit::editdiff::preview_from_input(&ask.input)
                    .into_iter()
                    .next()
                    .map(|e| e.path)
            });
        let Some(path) = path else {
            (self.toast)("This edit names no file to open");
            return;
        };
        glib::MainContext::default().spawn_local(async move {
            let Some(base) = me.base_of(&ask.item).await else {
                (me.toast)(
                    "No checkpoint was taken before this edit, so there is no earlier version \
                     to compare with",
                );
                return;
            };
            let name = tool.name.clone();
            // Resolving `dir` touches the filesystem, so it runs on a worker.
            let path =
                match off_main("resolving the path", move || Ok(path_to_open(&dir, &path))).await {
                    Ok(path) => path,
                    Err(why) => {
                        (me.toast)(&why);
                        return;
                    }
                };
            let request = OpenRequest {
                toplevel: base.toplevel.clone(),
                path,
                old_rev: base.rev.clone(),
                new_side: NewSide::Working,
            };
            if let Err(why) = diff_tool::open(tool, request).await {
                info!(tool = name, "the diff tool did not open: {why}");
                (me.toast)(&why);
            }
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn diffs() -> Rc<ThreadDiffs> {
        ThreadDiffs::new("/nonexistent", 1, "test", Rc::new(|_| {}))
    }

    #[test]
    fn a_relative_path_to_open_is_the_threads_not_the_repository_roots() {
        assert_eq!(path_to_open("/repo/sub", "a.rs"), "/repo/sub/a.rs");
        assert_eq!(path_to_open("/repo/sub", "x/a.rs"), "/repo/sub/x/a.rs");
        assert_eq!(path_to_open("/repo/sub", "/repo/b.rs"), "/repo/b.rs");
    }

    #[test]
    fn a_relative_thread_dir_gives_an_absolute_path_to_the_right_file() {
        let cwd = std::env::current_dir().expect("cwd");
        let got = path_to_open(".", "a.rs");
        assert!(Path::new(&got).is_absolute(), "{got}");
        assert_eq!(
            Path::new(&got),
            cwd.canonicalize().expect("canon").join("a.rs")
        );
        // A relative dir that does not exist still anchors at the working directory.
        let got = path_to_open("no/such/dir", "a.rs");
        assert_eq!(Path::new(&got), cwd.join("no/such/dir/a.rs"));
    }

    #[test]
    fn a_replayed_item_never_uses_a_later_turn_base() {
        let d = diffs();
        d.set_ready_for_test(TurnBase {
            toplevel: "/r".into(),
            rev: "later".into(),
        });
        assert!(glib::MainContext::new()
            .block_on(d.base_of("replayed"))
            .is_none());
        d.turn_ended();
        d.item_started("outside-a-turn");
        assert_eq!(d.bound_items(), 0);
    }

    #[test]
    fn an_item_of_the_current_turn_uses_its_base() {
        let d = diffs();
        let slot = Rc::new(BaseSlot::default());
        *d.current.borrow_mut() = Some(slot.clone());
        d.in_turn.set(true);
        d.item_started("edit"); // binds even while the baseline is pending
        *slot.state.borrow_mut() = BaseState::Ready(TurnBase {
            toplevel: "/r".into(),
            rev: "first".into(),
        });
        d.turn_ended();
        d.set_ready_for_test(TurnBase {
            toplevel: "/r".into(),
            rev: "second".into(),
        });
        d.item_started("edit"); // re-announced metadata keeps its original turn
        assert_eq!(
            glib::MainContext::new()
                .block_on(d.base_of("edit"))
                .unwrap()
                .rev,
            "first"
        );
    }

    #[test]
    fn a_file_change_is_tied_to_the_turn_it_started_in_and_a_restart_inside_it_changes_nothing() {
        let d = diffs();
        // Before any turn there is no baseline to bind to.
        d.item_started("early");
        assert_eq!(d.bound_items(), 0);

        d.set_ready_for_test(TurnBase {
            toplevel: "/r".into(),
            rev: "aaaa".into(),
        });
        d.item_started("first");
        assert_eq!(d.bound_items(), 1);
        let first = d.bases.borrow().get("first").cloned().expect("bound");
        assert_eq!(first.ready().map(|b| b.rev), Some("aaaa".to_owned()));

        // A second turn gets its own baseline; the first item keeps the old one.
        d.set_ready_for_test(TurnBase {
            toplevel: "/r".into(),
            rev: "bbbb".into(),
        });
        d.item_started("second");
        let second = d.bases.borrow().get("second").cloned().expect("bound");
        assert_eq!(second.ready().map(|b| b.rev), Some("bbbb".to_owned()));
        assert_eq!(
            d.bases
                .borrow()
                .get("first")
                .and_then(|s| s.ready())
                .map(|b| b.rev),
            Some("aaaa".to_owned())
        );
    }

    #[test]
    fn a_second_start_inside_one_turn_keeps_the_first_baseline() {
        let d = diffs();
        d.in_turn.set(true);
        d.set_ready_for_test(TurnBase {
            toplevel: "/r".into(),
            rev: "aaaa".into(),
        });
        let before = d.current.borrow().clone().expect("current");
        d.turn_started(true); // already in a turn: ignored
        let after = d.current.borrow().clone().expect("current");
        assert!(Rc::ptr_eq(&before, &after));
        d.turn_ended();
        assert!(!d.in_turn.get());
    }
}
