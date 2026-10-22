//! The local socket: creation with safe permissions, and accepting with a
//! bound on connections.
//!
//! The socket is created in a private directory and given its configured
//! mode before it is moved into place, so it is never briefly open to other
//! users; the mode never has bits for other users (see [`crate::config`]).
//! A stale socket from a previous run
//! is replaced, but a live one (another node answering) or a path that is
//! not a socket is refused. Every call still needs a valid token; the mode
//! keeps other local users from reaching the daemon at all.
//!
//! Each connection's peer credentials (`SO_PEERCRED`) are attached to its
//! requests and logged with them, for audit.

use std::io;
use std::os::unix::fs::{DirBuilderExt as _, FileTypeExt as _, PermissionsExt as _};
use std::path::Path;
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll, ready};
use std::time::Duration;

use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};
use tokio::net::{UnixListener, UnixStream};
use tokio::sync::{OwnedSemaphorePermit, Semaphore};
use tokio::time::{Sleep, sleep};
use tokio_stream::Stream;
use tonic::transport::server::Connected;

use crate::metrics::NodeMetrics;

/// The calling process, from the socket's peer credentials.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Peer {
    /// User id.
    pub uid: u32,
    /// Group id.
    pub gid: u32,
    /// Process id, where the platform reports it.
    pub pid: Option<i32>,
}

/// Creates the socket at `path` with `mode`. Must be called inside a
/// runtime.
pub fn bind(path: &Path, mode: u32) -> io::Result<UnixListener> {
    if let Some(dir) = path.parent().filter(|d| !d.as_os_str().is_empty()) {
        std::fs::create_dir_all(dir)?;
    }
    match std::fs::symlink_metadata(path) {
        Ok(meta) if meta.file_type().is_socket() => {
            if std::os::unix::net::UnixStream::connect(path).is_ok() {
                return Err(io::Error::new(
                    io::ErrorKind::AddrInUse,
                    format!("{} is served by a running node", path.display()),
                ));
            }
            std::fs::remove_file(path)?;
        }
        Ok(_) => {
            return Err(io::Error::new(
                io::ErrorKind::AlreadyExists,
                format!("{} exists and is not a socket", path.display()),
            ));
        }
        Err(e) if e.kind() == io::ErrorKind::NotFound => {}
        Err(e) => return Err(e),
    }
    // Bind inside a fresh private directory, set the mode, then move the
    // socket into place: it is never reachable with looser permissions,
    // and nothing process-wide (the umask) changes.
    let parent = path
        .parent()
        .filter(|d| !d.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    let private = parent.join(format!(
        ".graft-node-bind.{}.{}",
        std::process::id(),
        BIND_SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
    ));
    std::fs::DirBuilder::new().mode(0o700).create(&private)?;
    let staged = private.join("sock");
    let result = (|| {
        let listener = UnixListener::bind(&staged)?;
        std::fs::set_permissions(&staged, std::fs::Permissions::from_mode(mode))?;
        std::fs::rename(&staged, path)?;
        Ok(listener)
    })();
    let _ = std::fs::remove_file(&staged);
    let _ = std::fs::remove_dir(&private);
    result
}

static BIND_SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// One accepted connection, holding its share of the connection bound.
pub(crate) struct Conn {
    io: UnixStream,
    peer: Peer,
    _permit: OwnedSemaphorePermit,
    metrics: NodeMetrics,
}

impl Drop for Conn {
    fn drop(&mut self) {
        self.metrics.connections.dec();
    }
}

impl Connected for Conn {
    type ConnectInfo = Peer;
    fn connect_info(&self) -> Peer {
        self.peer.clone()
    }
}

impl AsyncRead for Conn {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        Pin::new(&mut self.io).poll_read(cx, buf)
    }
}

impl AsyncWrite for Conn {
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        Pin::new(&mut self.io).poll_write(cx, buf)
    }
    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.io).poll_flush(cx)
    }
    fn poll_shutdown(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.io).poll_shutdown(cx)
    }
    fn poll_write_vectored(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        bufs: &[io::IoSlice<'_>],
    ) -> Poll<io::Result<usize>> {
        Pin::new(&mut self.io).poll_write_vectored(cx, bufs)
    }
    fn is_write_vectored(&self) -> bool {
        self.io.is_write_vectored()
    }
}

/// The listener, shared with its [`Closer`]. `None` once closed.
type Slot = Arc<Mutex<Option<UnixListener>>>;

/// The accepting end of the socket. It accepts on the listener itself, not
/// from a spawned task, so the listener is dropped the moment it is closed.
pub(crate) struct Incoming {
    listener: Slot,
    slots: Arc<Semaphore>,
    metrics: NodeMetrics,
    backoff: Option<Pin<Box<Sleep>>>,
}

/// Closes an [`Incoming`]'s listener. The socket path refuses connections
/// from then on, even while the server still drains the ones it has.
#[derive(Clone)]
pub(crate) struct Closer(Slot);

impl Closer {
    pub(crate) fn close(&self) {
        // Taken out of the lock first, then dropped, so the close is
        // complete before this returns.
        let listener = self.0.lock().unwrap().take();
        drop(listener);
    }
}

/// Accepts connections on `listener`, at most `max` open at once: past it a
/// new connection is closed at once (the client falls back to the server),
/// rather than queued.
pub(crate) fn incoming(
    listener: UnixListener,
    max: usize,
    metrics: NodeMetrics,
) -> (Incoming, Closer) {
    let slot: Slot = Arc::new(Mutex::new(Some(listener)));
    let incoming = Incoming {
        listener: slot.clone(),
        slots: Arc::new(Semaphore::new(max)),
        metrics,
        backoff: None,
    };
    (incoming, Closer(slot))
}

impl Stream for Incoming {
    type Item = io::Result<Conn>;

    fn poll_next(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        let this = self.get_mut();
        loop {
            if let Some(backoff) = this.backoff.as_mut() {
                ready!(backoff.as_mut().poll(cx));
                this.backoff = None;
            }
            let slot = this.listener.lock().unwrap();
            let Some(listener) = slot.as_ref() else {
                return Poll::Ready(None);
            };
            let io = match ready!(listener.poll_accept(cx)) {
                Ok((io, _)) => io,
                Err(e) => {
                    tracing::warn!(error = %e, "accept failed");
                    // Out of descriptors and the like: don't spin.
                    this.backoff = Some(Box::pin(sleep(Duration::from_millis(50))));
                    continue;
                }
            };
            let Ok(permit) = this.slots.clone().try_acquire_owned() else {
                this.metrics.connections_refused.inc();
                drop(io);
                continue;
            };
            let peer = match io.peer_cred() {
                Ok(c) => Peer {
                    uid: c.uid(),
                    gid: c.gid(),
                    pid: c.pid(),
                },
                Err(_) => continue,
            };
            this.metrics.connections.inc();
            let conn = Conn {
                io,
                peer,
                _permit: permit,
                metrics: this.metrics.clone(),
            };
            return Poll::Ready(Some(Ok(conn)));
        }
    }
}
