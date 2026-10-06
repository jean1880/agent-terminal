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
