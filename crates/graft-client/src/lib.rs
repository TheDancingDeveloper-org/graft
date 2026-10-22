//! The graft client library (`docs/design.md` §5.1, §7.3, §9).
//!
//! [`Remote`] implements the API traits over gRPC (TCP or the node socket);
//! [`fetch`] is the named-object fetch protocol; [`sparse`] is the slice of
//! the snapshot a working copy holds; [`workcopy`] is the working copy.
//! Work items G1.11, G2.5, G3.5.

use std::fmt;
use std::io;
use std::path::{Path, PathBuf};

use graft_model::{ChangesetId, RepoPath};
use graft_proto::ApiError;
use graft_store::StoreError;

pub mod fetch;
pub mod remote;
pub mod sparse;
pub mod workcopy;
pub mod workspace;

pub use remote::Remote;
pub use sparse::Sparse;

/// Where the client sends requests.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Endpoint {
    /// The per-host node daemon's socket.
    NodeSocket(PathBuf),
    /// A server over TCP (`http://host:port` or `https://`).
    Server(String),
}

impl Endpoint {
    /// Parses the text form: `unix:<path>` or an absolute path is a socket,
    /// anything else is a server URL.
    pub fn parse(s: &str) -> Endpoint {
        let s = s.trim();
        if let Some(path) = s
            .strip_prefix("unix://")
            .or_else(|| s.strip_prefix("unix:"))
        {
            Endpoint::NodeSocket(PathBuf::from(path))
        } else if s.starts_with('/') {
            Endpoint::NodeSocket(PathBuf::from(s))
        } else {
            Endpoint::Server(s.to_owned())
        }
    }
}

impl fmt::Display for Endpoint {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Endpoint::NodeSocket(p) => write!(f, "unix:{}", p.display()),
            Endpoint::Server(url) => f.write_str(url),
        }
    }
}

/// The default node socket path.
pub const NODE_SOCKET: &str = "/run/graft/node.sock";

/// The environment variable that overrides endpoint discovery.
pub const ENDPOINT_ENV: &str = "GRAFT_ENDPOINT";

/// Picks an endpoint: `GRAFT_ENDPOINT`, else the node socket if present,
/// else the configured server.
pub fn discover(configured: Option<&str>) -> Option<Endpoint> {
    let env = std::env::var(ENDPOINT_ENV).ok();
    discover_from(env.as_deref(), Path::new(NODE_SOCKET), configured)
}

/// [`discover`] with its inputs explicit: the environment value, the node
/// socket path to probe, and the configured server. Empty values count as
/// absent.
pub fn discover_from(
    env: Option<&str>,
    node_socket: &Path,
    configured: Option<&str>,
) -> Option<Endpoint> {
    let present = |s: &&str| !s.trim().is_empty();
    if let Some(e) = env.filter(present) {
        return Some(Endpoint::parse(e));
    }
    if node_socket.exists() {
        return Some(Endpoint::NodeSocket(node_socket.to_owned()));
    }
    configured.filter(present).map(Endpoint::parse)
}

/// A client failure.
#[derive(Debug)]
#[non_exhaustive]
pub enum Error {
    /// The API answered with an error (or the transport failed, which
    /// arrives as `Unavailable`).
    Api(ApiError),
    /// A local file operation failed.
    Io(io::Error),
    /// The local object cache failed.
    Store(StoreError),
    /// An endpoint that can't be used, described.
    Endpoint(String),
    /// Local state (`.graft/`) is malformed, described.
    State(String),
    /// These paths have local changes an update would overwrite.
    Dirty(Vec<RepoPath>),
    /// Writing or removing this path would go through a symlink or a file:
    /// a parent component is not a real directory, on disk or in the base.
    UnsafePath(RepoPath),
    /// `commit` found no change in the slice.
    NothingToCommit,
    /// The operation needs a draft (`commit --amend`, `land`) and the
    /// workspace has none.
    NoDraft,
    /// This file kept changing while it was being read, so no consistent
    /// content could be committed.
    Unstable(RepoPath),
    /// The server's workspace head is not the one this working copy holds:
    /// someone else moved it.
    Diverged {
        /// The workspace.
        workspace: String,
        /// Its head on the server.
        server: Option<ChangesetId>,
        /// The head this working copy holds.
        local: Option<ChangesetId>,
    },
    /// `land --wait` ran out of time while the pull request was still open.
    StillQueued {
        /// The pull request.
        pr_url: String,
    },
    /// The stack landed, but these paths have local changes the update to
    /// the landed snapshot would overwrite. Running `land` again (it is
    /// idempotent) finishes the update once they are committed or reverted.
    LandedDirty {
        /// What landed.
        landed: ChangesetId,
        /// The paths in the way.
        paths: Vec<RepoPath>,
    },
}

/// Result alias for client calls.
pub type Result<T, E = Error> = std::result::Result<T, E>;

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::Api(e) => e.fmt(f),
            Error::Io(e) => e.fmt(f),
            Error::Store(e) => write!(f, "object cache: {e:?}"),
            Error::Endpoint(what) => write!(f, "endpoint: {what}"),
            Error::State(what) => write!(f, "working copy state: {what}"),
            Error::Dirty(paths) => write!(f, "{} paths have local changes", paths.len()),
            Error::UnsafePath(path) => {
                write!(f, "refusing {path:?}: a parent is a symlink or a file")
            }
            Error::NothingToCommit => f.write_str("nothing to commit"),
            Error::NoDraft => f.write_str("the workspace has no draft"),
            Error::Unstable(path) => write!(f, "{path} kept changing while it was read"),
            Error::Diverged {
                workspace,
                server,
                local,
            } => write!(
                f,
                "workspace {workspace} moved on the server (head {}, this working copy has {})",
                id_or_none(*server),
                id_or_none(*local)
            ),
            Error::StillQueued { pr_url } => write!(f, "still queued: {pr_url}"),
            Error::LandedDirty { landed, paths } => write!(
                f,
                "landed {landed}, but {} paths have local changes; commit or revert them and run land again",
                paths.len()
            ),
        }
    }
}

fn id_or_none(id: Option<ChangesetId>) -> String {
    id.map_or_else(|| "none".to_owned(), |id| id.to_string())
}

impl std::error::Error for Error {}

impl From<ApiError> for Error {
    fn from(e: ApiError) -> Self {
        Error::Api(e)
    }
}

impl From<io::Error> for Error {
    fn from(e: io::Error) -> Self {
        Error::Io(e)
    }
}

impl From<StoreError> for Error {
    fn from(e: StoreError) -> Self {
        Error::Store(e)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn endpoint_text_forms() {
        assert_eq!(
            Endpoint::parse("unix:/run/x.sock"),
            Endpoint::NodeSocket("/run/x.sock".into())
        );
        assert_eq!(
            Endpoint::parse("unix:///run/x.sock"),
            Endpoint::NodeSocket("/run/x.sock".into())
        );
        assert_eq!(
            Endpoint::parse("/run/x.sock"),
            Endpoint::NodeSocket("/run/x.sock".into())
        );
        assert_eq!(
            Endpoint::parse("http://graft:7070"),
            Endpoint::Server("http://graft:7070".into())
        );
    }

    #[test]
    fn discovery_order() {
        let dir = tempfile::tempdir().unwrap();
        let sock = dir.path().join("node.sock");
        let cfg = Some("http://configured:1");

        // No socket: the environment wins, then the configured server.
        assert_eq!(
            discover_from(Some("http://env:1"), &sock, cfg),
            Some(Endpoint::Server("http://env:1".into()))
        );
        assert_eq!(
            discover_from(None, &sock, cfg),
            Some(Endpoint::Server("http://configured:1".into()))
        );
        assert_eq!(discover_from(Some(" "), &sock, None), None);

        // A present socket beats the configured server, not the environment.
        std::fs::write(&sock, b"").unwrap();
        assert_eq!(
            discover_from(None, &sock, cfg),
            Some(Endpoint::NodeSocket(sock.clone()))
        );
        assert_eq!(
            discover_from(Some("unix:/elsewhere"), &sock, cfg),
            Some(Endpoint::NodeSocket("/elsewhere".into()))
        );
    }
}
