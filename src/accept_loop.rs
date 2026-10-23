//! The accept loop shared by the app's two local socket servers.

use std::rc::{Rc, Weak};
use std::time::Duration;

use gtk4::gio::{self, prelude::*};
use gtk4::glib;
use tracing::{debug, warn};

/// Accepts connections on `listener` until it is closed or cancelled, handing each to `on_conn`
/// while the server `weak` points to is alive; once it is gone the connection goes to `on_gone`
/// and the loop ends. A transient accept failure (a client that vanished mid-accept, fd pressure)
/// is logged and retried after 100 ms, so the listener never goes deaf. `name` labels the log
/// lines. The server is held weakly: the loop never keeps it alive.
pub fn spawn_accept_loop<S: 'static>(
    name: &'static str,
    weak: Weak<S>,
    listener: gio::SocketListener,
    on_conn: impl Fn(&Rc<S>, gio::SocketConnection) + 'static,
    on_gone: impl Fn(gio::SocketConnection) + 'static,
) {
    glib::spawn_future_local(async move {
        loop {
            match listener.accept_future().await {
                Ok((conn, _)) => {
                    let Some(server) = weak.upgrade() else {
                        on_gone(conn);
                        break;
                    };
                    on_conn(&server, conn);
                }
                Err(e)
                    if e.matches(gio::IOErrorEnum::Closed)
                        || e.matches(gio::IOErrorEnum::Cancelled) =>
                {
                    debug!("{name} accept loop ended");
                    break;
                }
                Err(e) => {
                    warn!(error = %e.message(), "{name} accept failed; continuing");
                    glib::timeout_future(Duration::from_millis(100)).await;
                    if weak.strong_count() == 0 {
                        break;
                    }
                }
            }
        }
    });
}
