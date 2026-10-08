//! Test helpers for code that runs on the glib main loop. No display, no real HOME or XDG.

use std::time::{Duration, Instant};

use gtk4::glib;

/// Runs `body` with a private main context as the thread default, so every test drives its own
/// loop and tests running in parallel threads never share a context.
pub fn in_loop<R>(body: impl FnOnce(&glib::MainContext) -> R) -> R {
    let ctx = glib::MainContext::new();
    ctx.with_thread_default(|| body(&ctx))
        .expect("acquire the test main context")
}

/// Iterates `ctx` until `cond` holds. Returns false on timeout (`secs`).
pub fn pump_until(ctx: &glib::MainContext, secs: u64, mut cond: impl FnMut() -> bool) -> bool {
    let deadline = Instant::now() + Duration::from_secs(secs);
    while !cond() {
        if Instant::now() > deadline {
            return false;
        }
        if !ctx.iteration(false) {
            std::thread::sleep(Duration::from_millis(2));
        }
    }
    true
}

/// A widget's minimum size along `orientation` (unconstrained in the other).
pub fn min_size(widget: &gtk4::Widget, orientation: gtk4::Orientation) -> i32 {
    use gtk4::prelude::*;
    widget.measure(orientation, -1).0
}

/// Every mapped `AdwBreakpointBin` under `root` whose child needs more room than the bin was
/// given. A bin with breakpoints reports only its size request as its minimum, so its overflow
/// never shows in `root`'s: each must be checked at its own allocation. Also check clamp widths,
/// since scroll containers can conceal an unbounded row's horizontal minimum.
pub fn overflowing_bins(root: &gtk4::Widget) -> Vec<String> {
    use gtk4::prelude::*;
    let mut out = Vec::new();
    let mut stack = vec![root.clone()];
    while let Some(widget) = stack.pop() {
        if !widget.is_mapped() {
            continue;
        }
        if widget.is::<gtk4::Button>() && widget.has_css_class("pill") {
            let mut parent = widget.parent();
            let mut interruption = false;
            while let Some(ancestor) = parent {
                interruption |= ancestor.has_css_class("interruption-shelf");
                parent = ancestor.parent();
            }
            if interruption {
                if let Some(bounds) = widget.compute_bounds(&widget) {
                    if bounds.width() < 44.0 || bounds.height() < 44.0 {
                        out.push(format!(
                            "permission target is {}×{}; needs 44×44",
                            bounds.width(),
                            bounds.height()
                        ));
                    }
                }
            }
        }
        if let Some(bin) = widget.downcast_ref::<adw::BreakpointBin>() {
            if let Some(child) = adw::prelude::BreakpointBinExt::child(bin) {
                let (w, h) = (bin.width(), bin.height());
                let (cw, ch) = (
                    min_size(&child, gtk4::Orientation::Horizontal),
                    min_size(&child, gtk4::Orientation::Vertical),
                );
                if cw > w || ch > h {
                    out.push(format!(
                        "{} {}×{} holds a child needing {cw}×{ch}",
                        child.css_classes().join("."),
                        w,
                        h
                    ));
                }
            }
        }
        if let Some(clamp) = widget.downcast_ref::<adw::Clamp>() {
            if let Some(child) = clamp.child() {
                let needed = min_size(&child, gtk4::Orientation::Horizontal);
                if needed > clamp.width() {
                    out.push(format!(
                        "clamp {} px holds a child needing {needed} px\n{}",
                        clamp.width(),
                        min_size_report(&child, gtk4::Orientation::Horizontal, 100)
                    ));
                }
            }
        }
        let mut child = widget.first_child();
        while let Some(c) = child {
            child = c.next_sibling();
            stack.push(c);
        }
    }
    out
}

/// Why `root` needs at least `limit` px along `orientation`: every laid-out descendant that
/// alone needs `threshold` px or more, indented by depth, with its type, CSS classes and any
/// label text. A widget whose minimum is too big shows up with the children that make it so.
pub fn min_size_report(
    root: &gtk4::Widget,
    orientation: gtk4::Orientation,
    threshold: i32,
) -> String {
    use gtk4::prelude::*;
    fn describe(widget: &gtk4::Widget) -> String {
        let classes = widget.css_classes().join(".");
        let text = widget
            .downcast_ref::<gtk4::Label>()
            .map(|l| {
                let t: String = l.text().chars().take(40).collect();
                format!(" {t:?}")
            })
            .unwrap_or_default();
        format!(
            "{}{}{text}",
            widget.type_().name(),
            if classes.is_empty() {
                String::new()
            } else {
                format!(".{classes}")
            }
        )
    }
    fn walk(
        widget: &gtk4::Widget,
        orientation: gtk4::Orientation,
        threshold: i32,
        depth: usize,
        out: &mut String,
    ) {
        let min = min_size(widget, orientation);
        out.push_str(&format!(
            "{:indent$}{min:>5} {}\n",
            "",
            describe(widget),
            indent = depth * 2
        ));
        let mut child = widget.first_child();
        while let Some(c) = child {
            if c.should_layout() && min_size(&c, orientation) >= threshold {
                walk(&c, orientation, threshold, depth + 1, out);
            }
            child = c.next_sibling();
        }
    }
    let mut out = String::new();
    walk(root, orientation, threshold, 0, &mut out);
    out
}
