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
use std::sync::Arc;
use std::task::{Context, Poll};

use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};
use tokio::net::{UnixListener, UnixStream};
use tokio::sync::{OwnedSemaphorePermit, Semaphore, mpsc};
use tokio_stream::wrappers::ReceiverStream;
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

/// Accepts connections on `listener`, at most `max` open at once: past it a
/// new connection is closed at once (the client falls back to the server),
/// rather than queued.
pub(crate) fn incoming(
    listener: UnixListener,
    max: usize,
    metrics: NodeMetrics,
) -> ReceiverStream<io::Result<Conn>> {
    let (tx, rx) = mpsc::channel(16);
    let slots = Arc::new(Semaphore::new(max));
    tokio::spawn(async move {
        loop {
            let accepted = tokio::select! {
                a = listener.accept() => a,
                () = tx.closed() => return,
            };
            let io = match accepted {
                Ok((io, _)) => io,
                Err(e) => {
                    tracing::warn!(error = %e, "accept failed");
                    // Out of descriptors and the like: don't spin.
                    tokio::time::sleep(std::time::Duration::from_millis(50)).await;
                    continue;
                }
            };
            let Ok(permit) = slots.clone().try_acquire_owned() else {
                metrics.connections_refused.inc();
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
            metrics.connections.inc();
            let conn = Conn {
                io,
                peer,
                _permit: permit,
                metrics: metrics.clone(),
            };
            if tx.send(Ok(conn)).await.is_err() {
                return;
            }
        }
    });
    ReceiverStream::new(rx)
}
