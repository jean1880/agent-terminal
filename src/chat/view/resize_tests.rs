//! What a window resize costs a chat thread on the CPU: layout, and layout plus building the
//! render nodes, at widths new to GTK's caches, by how many rows are materialised and how many
//! other threads are open. `make bench T=resize_cost`.
//!
//! Measured 2026-10-17 (Xvfb): before a thread's rows have ever been laid out, a layout of 150 rows
//! costs ~50 ms, all of it one-off style and shaping work. Once shown, a resize of 150 rows costs
//! ~1–7 ms of layout and ~1 ms more for its render nodes. Hidden threads (each shown once first, as
//! in the app) add nothing: `AdwTabView` lays out only the visible page.

use super::tests::{native_init, pump_until, status_of, SwitchableBackend};
use super::*;
use agent_core::event::{ItemKind, ItemStatus, StreamKind};
use std::time::{Duration, Instant};

/// Iterates the main loop for `ms`: idle fills and the window's own frames run.
fn pump_for(ctx: &glib::MainContext, ms: u64) {
    let until = Instant::now() + Duration::from_millis(ms);
    pump_until(ctx, || Instant::now() > until);
}

const PARA: &str = "The transcript keeps every materialised row as a real widget, so a width \
change re-wraps every paragraph in it. This sentence is long enough to wrap a few times at the \
usual column width, like a real assistant reply does, with `inline code` and **bold** words.";

/// A thread of `n` items with wrapping replies, as a real one has.
fn history(n: usize) -> Vec<Envelope> {
    let mut out = Vec::new();
    for i in 0..n {
        let id = format!("r{i}");
        let (kind, text) = if i % 2 == 0 {
            (ItemKind::UserMessage, format!("Question {i}: {PARA}"))
        } else {
            (
                ItemKind::AssistantMessage,
                format!("{PARA}\n\n{PARA}\n\n- {PARA}\n- one more point\n\n{PARA}"),
            )
        };
        out.push(
            Envelope::new(Event::ItemStarted {
                kind,
                title: String::new(),
                input: None,
                parent: None,
            })
            .item(&id),
        );
        out.push(
            Envelope::new(Event::ContentSnapshot {
                stream: StreamKind::Assistant,
                text,
            })
            .item(&id),
        );
        out.push(
            Envelope::new(Event::ItemCompleted {
                status: ItemStatus::Completed,
                output: None,
                error: None,
            })
            .item(&id),
        );
    }
    out
}

fn chat(n: usize) -> ChatView {
    let view = ChatView::new(Rc::new(SwitchableBackend {
        status: RefCell::new(status_of(Driver::Claude)),
    }));
    view.replay(&history(n));
    view
}

/// Mean time (ms) to lay `content` out at each of `widths`: what a resize frame does to it
/// (Xvfb has no window manager, so the window itself cannot be resized). Each width is new to
/// GTK's measure cache, as during a drag.
fn layout_cost(content: &gtk4::Widget, widths: &[i32]) -> f64 {
    let started = Instant::now();
    for &w in widths {
        let _ = content.measure(gtk4::Orientation::Horizontal, -1);
        let _ = content.measure(gtk4::Orientation::Vertical, w);
        content.allocate(w, 800, -1, None);
    }
    started.elapsed().as_secs_f64() * 1000.0 / widths.len() as f64
}

/// Sweeps from one base before widths repeat. GTK caches only a few sizes per widget, so a width
/// last used a whole cycle (160 layouts) ago is a miss again: any number of sweeps is fine.
const SWEEPS: i32 = 16;

thread_local! {
    static NARROW: Cell<i32> = const { Cell::new(0) };
    static WIDE: Cell<i32> = const { Cell::new(0) };
}

/// 10 widths from `base`, interleaved with the other sweeps', so none is in any widget's
/// measure cache (as during a drag).
fn fresh(base: i32, sweep: &'static std::thread::LocalKey<Cell<i32>>) -> Vec<i32> {
    let k = sweep.with(|s| s.replace(s.get() + 1)) % SWEEPS;
    (0..10).map(|i| base + i * SWEEPS + k).collect()
}

/// Widths inside the transcript's clamp (700–860 px): every step re-wraps the column.
fn narrow() -> Vec<i32> {
    fresh(700, &NARROW)
}

/// Widths above the clamp's 908 px: the column keeps its width.
fn wide() -> Vec<i32> {
    fresh(1300, &WIDE)
}

/// Mean time (ms) to lay `content` out at each of `widths` and build its render nodes: the CPU
/// half of a resize frame (the GPU only rasterises the nodes afterwards). After a resize every
/// widget's cached node is stale, as here.
fn frame_cost(content: &gtk4::Widget, widths: &[i32]) -> f64 {
    let paintable = gtk4::WidgetPaintable::new(Some(content));
    let started = Instant::now();
    for &w in widths {
        let _ = content.measure(gtk4::Orientation::Horizontal, -1);
        let _ = content.measure(gtk4::Orientation::Vertical, w);
        content.allocate(w, 800, -1, None);
        let snapshot = gtk4::Snapshot::new();
        paintable.snapshot(&snapshot, f64::from(w), 800.0);
        let _ = snapshot.to_node();
    }
    started.elapsed().as_secs_f64() * 1000.0 / widths.len() as f64
}

fn report(label: &str, content: &impl IsA<gtk4::Widget>) {
    let content = content.upcast_ref();
    let n_ms = layout_cost(content, &narrow());
    let w_ms = layout_cost(content, &wide());
    let f_ms = frame_cost(content, &narrow());
    eprintln!(
        "{label:<36} layout narrow {n_ms:6.2}  wide {w_ms:6.2}  | layout+snapshot narrow {f_ms:6.2} ms"
    );
}

#[test]
#[ignore = "a measurement: prints costs, asserts nothing (make bench T=resize_cost)"]
fn resize_cost_of_long_threads() {
    use adw::prelude::AdwWindowExt;
    let ctx = native_init();
    // The window of rows built and laid out at least once, as a thread the user has looked at.
    let shown = |view: &ChatView, n: usize| {
        let inner = view.inner().expect("inner");
        let want = n.min(transcript::WINDOW);
        assert!(
            pump_until(&ctx, || view.is_mapped()
                && inner.transcript.materialised() == want),
            "{n} items: the window never filled"
        );
        pump_for(&ctx, 500);
        want
    };
    for n in [0usize, 30, 150, 400] {
        let tabs = adw::TabView::new();
        let window = adw::Window::new();
        window.set_default_size(900, 800);
        window.set_content(Some(&tabs));
        window.present();
        let view = chat(n);
        tabs.append(&view);
        let rows = shown(&view, n);
        report(&format!("{n} items, {rows} rows"), &tabs);
        if n == 150 {
            // Laid out at these widths' neighbours by now: GTK's style and Pango's shaping are
            // cached, as during a drag. The baseline for the hidden threads below.
            report("150 rows, laid out before", &tabs);
            for hidden in 1..=3 {
                let other = chat(150);
                let page = tabs.append(&other);
                tabs.set_selected_page(&page);
                shown(&other, 150);
                tabs.set_selected_page(&tabs.nth_page(0));
                pump_for(&ctx, 300);
                report(&format!("150 rows + {hidden} hidden thread(s)"), &tabs);
            }
        }
        window.close();
    }
}

/// Nanoseconds the calling thread has run on a CPU (`/proc/thread-self/schedstat`'s first
/// field): the test thread, which is the one GTK runs on here. 0 when unreadable.
fn gtk_thread_cpu_ns() -> u64 {
    std::fs::read_to_string("/proc/thread-self/schedstat")
        .ok()
        .and_then(|s| s.split_whitespace().next()?.parse().ok())
        .unwrap_or(0)
}

/// How long (ms) the first frame after `action` took, from before-paint to after-paint: style
/// validation, layout and snapshot of whatever `action` invalidated.
fn frame_after(ctx: &glib::MainContext, window: &gtk4::Window, action: impl FnOnce()) -> f64 {
    let clock = window.frame_clock().expect("mapped");
    let started = Rc::new(Cell::new(None::<Instant>));
    let took = Rc::new(Cell::new(None::<Duration>));
    let s = started.clone();
    let before = clock.connect_before_paint(move |_| s.set(Some(Instant::now())));
    let (s, t) = (started.clone(), took.clone());
    let after = clock.connect_after_paint(move |_| {
        if let (Some(at), None) = (s.get(), t.get()) {
            t.set(Some(at.elapsed()));
        }
    });
    action();
    window.queue_draw();
    pump_until(ctx, || took.get().is_some());
    clock.disconnect(before);
    clock.disconnect(after);
    took.get().map_or(f64::NAN, |d| d.as_secs_f64() * 1000.0)
}

/// Mean ms per frame spent in each frame-clock phase over `ms` of idling: (update — ticks and
/// style —, layout, paint). Each handler is connected after GTK's own, so it runs at the end of
/// its phase.
fn idle_phases(ctx: &glib::MainContext, window: &gtk4::Window, ms: u64) -> (f64, f64, f64, u64) {
    let clock = window.frame_clock().expect("mapped");
    let marks = Rc::new(RefCell::new((None::<Instant>, [Duration::ZERO; 3], 0u64)));
    let start = {
        let m = marks.clone();
        clock.connect_before_paint(move |_| m.borrow_mut().0 = Some(Instant::now()))
    };
    let phase = |i: usize| {
        let m = marks.clone();
        move |_: &gtk4::gdk::FrameClock| {
            let mut m = m.borrow_mut();
            if let Some(at) = m.0 {
                let now = Instant::now();
                m.1[i] += now - at;
                m.0 = Some(now);
                if i == 2 {
                    m.2 += 1;
                    m.0 = None;
                }
            }
        }
    };
    let ids = [
        clock.connect_update(phase(0)),
        clock.connect_layout(phase(1)),
        clock.connect_paint(phase(2)),
    ];
    pump_for(ctx, ms);
    clock.disconnect(start);
    for id in ids {
        clock.disconnect(id);
    }
    let (_, spent, frames) = *marks.borrow();
    let per = |d: Duration| d.as_secs_f64() * 1000.0 / frames.max(1) as f64;
    (per(spent[0]), per(spent[1]), per(spent[2]), frames)
}

/// What open threads cost while nothing happens in them, and when the window's style changes
/// (focus in and out restyles every widget that tracks `:backdrop`).
#[test]
#[ignore = "a measurement: prints costs, asserts nothing (make bench T=idle_cost)"]
fn idle_cost_of_open_threads() {
    use adw::prelude::AdwWindowExt;
    let ctx = native_init();
    // 1 again last: a cost that stays is the run's (earlier windows), not the threads'.
    for threads in [1usize, 4, 8, 1] {
        let tabs = adw::TabView::new();
        let window = adw::Window::new();
        window.set_default_size(900, 800);
        window.set_content(Some(&tabs));
        window.present();
        let mut views = Vec::new();
        for _ in 0..threads {
            let view = chat(150);
            let page = tabs.append(&view);
            tabs.set_selected_page(&page);
            let inner = view.inner().expect("inner");
            assert!(pump_until(&ctx, || view.is_mapped()
                && inner.transcript.materialised() == transcript::WINDOW));
            views.push((view, inner));
        }
        tabs.set_selected_page(&tabs.nth_page(0));
        pump_for(&ctx, 500);

        let hidden_ticks = || -> u64 {
            views[1..]
                .iter()
                .map(|(_, inner)| inner.transcript.tick_count())
                .sum()
        };
        // (settles, moves) of the shown thread's transcript and of all the hidden ones.
        let settles = || {
            let of = |v: &[(ChatView, Rc<Inner>)]| {
                v.iter().fold((0, 0), |(s, m), (_, inner)| {
                    let (s2, m2) = inner.transcript.settle_counts();
                    (s + s2, m + m2)
                })
            };
            (of(&views[..1]), of(&views[1..]))
        };
        let clock = window.frame_clock().expect("mapped");
        let (ticks0, frames0, cpu0) = (hidden_ticks(), clock.frame_counter(), gtk_thread_cpu_ns());
        let settles0 = settles();
        let idle = Instant::now();
        pump_for(&ctx, 2000);
        let secs = idle.elapsed().as_secs_f64();
        let frames = (clock.frame_counter() - frames0) as f64 / secs;
        let ticks = (hidden_ticks() - ticks0) as f64 / secs;
        let cpu = gtk_thread_cpu_ns().saturating_sub(cpu0) as f64 / 1e9 / secs * 100.0;
        let (shown1, hidden1) = settles();
        let rate = |a: u64, b: u64| (a - b) as f64 / secs;
        eprintln!(
            "{threads} thread(s): shown settles {:.0}/s moves {:.0}/s | hidden settles {:.0}/s moves {:.0}/s",
            rate(shown1.0, settles0.0 .0),
            rate(shown1.1, settles0.0 .1),
            rate(hidden1.0, settles0.1 .0),
            rate(hidden1.1, settles0.1 .1),
        );
        let (update, layout, paint, phase_frames) = idle_phases(&ctx, window.upcast_ref(), 1000);
        eprintln!(
            "{threads} thread(s): per idle frame ({phase_frames} frames) update {update:.2} ms, \
             layout {layout:.2} ms, paint {paint:.2} ms"
        );

        let w = window.upcast_ref::<gtk4::Window>();
        let backdrop = frame_after(&ctx, w, || {
            window.set_state_flags(gtk4::StateFlags::BACKDROP, false)
        });
        let focus = frame_after(&ctx, w, || {
            window.unset_state_flags(gtk4::StateFlags::BACKDROP)
        });
        eprintln!(
            "{threads} thread(s) open, 1 shown: idle {frames:5.1} frames/s, hidden ticks {ticks:6.1}/s, \
             main thread {cpu:5.1}% CPU | backdrop frame {backdrop:6.2} ms, refocus frame {focus:6.2} ms"
        );
        // The same style change with the hidden threads' views invisible, not just unmapped.
        for (view, _) in &views[1..] {
            view.set_visible(false);
        }
        pump_for(&ctx, 300);
        let backdrop = frame_after(&ctx, w, || {
            window.set_state_flags(gtk4::StateFlags::BACKDROP, false)
        });
        let focus = frame_after(&ctx, w, || {
            window.unset_state_flags(gtk4::StateFlags::BACKDROP)
        });
        eprintln!(
            "{:>40} hidden views invisible: backdrop frame {backdrop:6.2} ms, refocus frame {focus:6.2} ms",
            ""
        );
        window.close();
    }
}
