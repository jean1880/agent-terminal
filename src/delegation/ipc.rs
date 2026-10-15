//! Lane C: the delegation IPC server (contract §4, P4 §4.1–§4.2): a GIO Unix socket on the main
//! context. Nothing here blocks: the accept loop, every connection's reader and writer are
//! futures on the main context, and every call goes to the supervisor through the
//! [`CallDispatcher`]. Local only: a Unix socket in the private runtime directory, no port.

mod conn;
pub(crate) mod peer;
#[cfg(test)]
mod tests;

use std::cell::{Cell, RefCell};
use std::collections::BTreeMap;
use std::os::unix::fs::{FileTypeExt, MetadataExt};
use std::path::{Path, PathBuf};
use std::rc::{Rc, Weak};
use std::time::Duration;

use agent_core::delegation::limits::{admit_connection, Usage};
use agent_core::delegation::{Limits, ToolError};
use gtk4::gio;
use gtk4::gio::prelude::*;
use gtk4::glib;
use tracing::{debug, info, warn};

use self::conn::Conn;
use super::seam::{CallDispatcher, ConnId, RuntimeDir};

/// `sun_path` holds 108 bytes, NUL included.
const MAX_SOCKET_PATH: usize = 107;

/// The listener on `<rundir>/delegation-<app_pid>.sock` and its connections.
pub(crate) struct IpcServer {
    path: PathBuf,
    /// The socket file this server made, so only that file is ever unlinked.
    file_id: (u64, u64),
    listener: gio::SocketListener,
    limits: Limits,
    dispatcher: Rc<dyn CallDispatcher>,
    conns: RefCell<BTreeMap<ConnId, Rc<Conn>>>,
    next_conn: Cell<u64>,
    /// `close_all` ran: no connection is accepted any more.
    shut: Cell<bool>,
    unlinked: Cell<bool>,
}

/// What [`leftover_verdict`] says about a file already at the socket path.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Leftover {
    /// Nothing there.
    Absent,
    /// A socket of ours from an earlier run with this pid: remove it.
    Stale,
}

/// A file found at the socket path, as `lstat` saw it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Found {
    symlink: bool,
    socket: bool,
    uid: u32,
}

/// Only a socket (not a symlink) owned by `our_uid` is removed; anything else refuses the bind,
/// since it is not ours to delete and cannot be a leftover of this app.
fn leftover_verdict(found: Option<Found>, our_uid: u32) -> Result<Leftover, String> {
    let Some(f) = found else {
        return Ok(Leftover::Absent);
    };
    if f.symlink {
        return Err("the delegation socket path is a symlink".to_owned());
    }
    if !f.socket {
        return Err("the delegation socket path holds a file that is not a socket".to_owned());
    }
    if f.uid != our_uid {
        return Err("the delegation socket path holds another user's socket".to_owned());
    }
    Ok(Leftover::Stale)
}

fn lstat(path: &Path) -> Result<Option<std::fs::Metadata>, String> {
    match std::fs::symlink_metadata(path) {
        Ok(m) => Ok(Some(m)),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(format!("cannot inspect the delegation socket path: {e}")),
    }
}

impl IpcServer {
    /// Binds `dir/delegation-<app_pid>.sock` (`0600` inside the `0700` runtime directory) and
    /// starts accepting on the main context. A stale socket of ours is removed first; a symlink,
    /// a non-socket or another user's socket at that path refuses the bind.
    pub(crate) fn bind(
        dir: &RuntimeDir,
        app_pid: u32,
        limits: Limits,
        dispatcher: Rc<dyn CallDispatcher>,
    ) -> Result<Rc<Self>, String> {
        use std::os::unix::fs::PermissionsExt;
        let path = dir.path().join(format!("delegation-{app_pid}.sock"));
        if path.as_os_str().len() > MAX_SOCKET_PATH {
            return Err("the delegation socket path is too long".to_owned());
        }
        // The verified runtime directory is owned by this uid (`rundir::verified`).
        // SAFETY: `geteuid` has no preconditions and cannot fail.
        let our_uid = unsafe { libc::geteuid() };
        let found = lstat(&path)?.map(|m| Found {
            symlink: m.file_type().is_symlink(),
            socket: m.file_type().is_socket(),
            uid: m.uid(),
        });
        if leftover_verdict(found, our_uid)? == Leftover::Stale {
            // The name holds our pid: no live server of this process uses it (one per app).
            debug!("removing a stale delegation socket");
            std::fs::remove_file(&path)
                .map_err(|e| format!("cannot remove the stale delegation socket: {e}"))?;
        }
        let listener = gio::SocketListener::new();
        listener
            .add_address(
                &gio::UnixSocketAddress::new(&path),
                gio::SocketType::Stream,
                gio::SocketProtocol::Default,
                None::<&glib::Object>,
            )
            .map_err(|e| format!("cannot listen on the delegation socket: {}", e.message()))?;
        let made = match lstat(&path) {
            Ok(Some(m)) if m.file_type().is_socket() => m,
            _ => {
                listener.close();
                return Err("the delegation socket vanished after bind".to_owned());
            }
        };
        let server = Rc::new(Self {
            path,
            file_id: (made.dev(), made.ino()),
            listener: listener.clone(),
            limits,
            dispatcher,
            conns: RefCell::new(BTreeMap::new()),
            next_conn: Cell::new(1),
            shut: Cell::new(false),
            unlinked: Cell::new(false),
        });
        // From here `Drop` unlinks the file, including on the early return below.
        std::fs::set_permissions(&server.path, std::fs::Permissions::from_mode(0o600))
            .map_err(|e| format!("cannot secure the delegation socket: {e}"))?;
        info!("delegation socket listening");
        spawn_accept_loop(Rc::downgrade(&server), listener);
        Ok(server)
    }

    pub(crate) fn socket_path(&self) -> &Path {
        &self.path
    }

    /// Sends `revoked` with `error`, flushes (at most the write deadline), then closes.
    pub(crate) fn revoke(&self, conn: ConnId, error: ToolError) {
        let found = self.conns.borrow().get(&conn).cloned();
        if let Some(c) = found {
            conn::revoke(self, &c, error);
        }
    }

    /// Stops accepting, removes the socket, and revokes every connection with `error` (each
    /// flushes its `revoked` frame within the write deadline, then closes).
    pub(crate) fn close_all(&self, error: ToolError) {
        self.shut.set(true);
        self.listener.close();
        self.unlink();
        let all: Vec<Rc<Conn>> = self.conns.borrow().values().cloned().collect();
        for c in all {
            conn::revoke(self, &c, error.clone());
        }
    }

    /// (all connections, unauthenticated connections).
    pub(crate) fn counts(&self) -> (u32, u32) {
        let usage = self.usage();
        (usage.connections, usage.preauth_connections)
    }

    fn usage(&self) -> Usage {
        let conns = self.conns.borrow();
        let count = |n: usize| u32::try_from(n).unwrap_or(u32::MAX);
        Usage {
            connections: count(conns.len()),
            preauth_connections: count(conns.values().filter(|c| c.unauthenticated()).count()),
            ..Usage::default()
        }
    }

    fn next_id(&self) -> ConnId {
        let id = self.next_conn.get();
        self.next_conn.set(id + 1);
        ConnId(id)
    }

    /// Removes the socket file, only if it is still the one this server made.
    fn unlink(&self) {
        if self.unlinked.replace(true) {
            return;
        }
        match lstat(&self.path) {
            Ok(Some(m)) if (m.dev(), m.ino()) == self.file_id && m.file_type().is_socket() => {
                if let Err(e) = std::fs::remove_file(&self.path) {
                    debug!(error = %e, "delegation socket already gone");
                }
            }
            Ok(Some(_)) => warn!("the delegation socket path was replaced; left in place"),
            _ => debug!("delegation socket already gone"),
        }
    }
}

impl Drop for IpcServer {
    fn drop(&mut self) {
        self.listener.close();
        self.unlink();
        // Nobody is left to dispatch to: close what is open without telling the dispatcher,
        // which is being torn down with its server.
        let all: Vec<Rc<Conn>> = std::mem::take(self.conns.get_mut()).into_values().collect();
        for c in all {
            c.teardown();
        }
    }
}

fn spawn_accept_loop(weak: Weak<IpcServer>, listener: gio::SocketListener) {
    glib::spawn_future_local(async move {
        loop {
            match listener.accept_future().await {
                Ok((io, _)) => {
                    let Some(server) = weak.upgrade() else {
                        conn::close_io(&io);
                        break;
                    };
                    accept(&server, io);
                }
                Err(e)
                    if e.matches(gio::IOErrorEnum::Closed)
                        || e.matches(gio::IOErrorEnum::Cancelled) =>
                {
                    debug!("delegation accept loop ended");
                    break;
                }
                Err(e) => {
                    // A client that vanished mid-accept, or fd pressure: keep listening.
                    warn!(error = %e.message(), "delegation accept failed; continuing");
                    glib::timeout_future(Duration::from_millis(100)).await;
                    if weak.strong_count() == 0 {
                        break;
                    }
                }
            }
        }
    });
}

/// Admits one accepted connection, or closes it at once past either connection cap.
fn accept(server: &Rc<IpcServer>, io: gio::SocketConnection) {
    if server.shut.get() || !admit_connection(&server.limits, &server.usage()) {
        debug!("delegation connection refused: connection cap reached");
        conn::close_io(&io);
        return;
    }
    let id = server.next_id();
    conn::start(server, id, io);
}
