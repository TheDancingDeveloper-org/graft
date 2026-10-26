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

pub mod backend;
pub mod fetch;
pub mod ignore;
pub mod local;
pub mod remote;
pub mod sparse;
pub mod workcopy;
pub mod workspace;

pub use backend::Backend;
pub use local::LocalRepo;
pub use remote::Remote;
pub use sparse::Sparse;

use graft_proto::api::RepoApi;
use graft_proto::error::ApiResult;
use graft_proto::types::{
    Attestation, AttestationPage, GetAttestations, GetLandChecks, LandCheck, LandCheckPage, Page,
    ReviewItem, ReviewQueue, ReviewQueuePage,
};

/// How many pages one helper walk will follow before it stops.
const MAX_PAGES_WALKED: u32 = 10_000;

/// Walks `GetAttestations` until the server has no next token or `limit`
/// attestations have been collected. Each request asks for at most the
/// remaining count. A server that never ends the walk is capped.
pub async fn attestations(
    api: &impl RepoApi,
    cx: &graft_proto::CallContext,
    mut req: GetAttestations,
    limit: u32,
) -> ApiResult<Vec<Attestation>> {
    let mut out = Vec::new();
    for _ in 0..MAX_PAGES_WALKED {
        if out.len() as u32 >= limit {
            break;
        }
        req.page.limit = limit - out.len() as u32;
        let page: AttestationPage = api.get_attestations(cx, req.clone()).await?;
        let n = page.attestations.len();
        out.extend(page.attestations);
        match page.next {
            Some(token) if n > 0 => req.page.token = Some(token),
            _ => break,
        }
    }
    out.truncate(limit as usize);
    Ok(out)
}

/// Walks `GetLandChecks` the same way as [`attestations`].
pub async fn land_checks(
    api: &impl RepoApi,
    cx: &graft_proto::CallContext,
    mut req: GetLandChecks,
    limit: u32,
) -> ApiResult<Vec<LandCheck>> {
    let mut out = Vec::new();
    for _ in 0..MAX_PAGES_WALKED {
        if out.len() as u32 >= limit {
            break;
        }
        req.page.limit = limit - out.len() as u32;
        let page: LandCheckPage = api.get_land_checks(cx, req.clone()).await?;
        let n = page.rows.len();
        out.extend(page.rows);
        match page.next {
            Some(token) if n > 0 => req.page.token = Some(token),
            _ => break,
        }
    }
    out.truncate(limit as usize);
    Ok(out)
}

/// Walks `ReviewQueue`, passing each page's `next_lsn` back as `since_lsn`.
/// Stops at `limit` items, when the server is caught up, or when a page
/// does not move the cursor forward.
pub async fn review_queue(
    api: &impl RepoApi,
    cx: &graft_proto::CallContext,
    mut req: ReviewQueue,
    limit: u32,
) -> ApiResult<Vec<ReviewItem>> {
    let mut out = Vec::new();
    for _ in 0..MAX_PAGES_WALKED {
        if out.len() as u32 >= limit {
            break;
        }
        req.page = Page {
            limit: limit - out.len() as u32,
            token: None,
        };
        let page: ReviewQueuePage = api.review_queue(cx, req.clone()).await?;
        let n = page.items.len();
        out.extend(page.items);
        match page.next_lsn {
            Some(next) if n > 0 && next > req.since_lsn => req.since_lsn = next,
            _ => break,
        }
    }
    out.truncate(limit as usize);
    Ok(out)
}

/// A request id for call `n` of this process: `prefix`, the process id, a
/// random per-process nonce, and `n`. The server names an unnamed
/// workspace from the principal and the request id, and answers a second
/// open under the same name with the first workspace (a retried open). Two
/// processes under one token (two containers, both pid 7) must therefore
/// never send the same id, or their working copies share a workspace.
pub fn request_id(prefix: &str, n: u64) -> String {
    use std::hash::{BuildHasher, RandomState};
    use std::sync::OnceLock;
    static NONCE: OnceLock<u64> = OnceLock::new();
    // `RandomState` keys come from the OS's random source, once per process.
    let nonce = NONCE.get_or_init(|| RandomState::new().hash_one(std::process::id()));
    format!("{prefix}-{}-{nonce:016x}-{n}", std::process::id())
}

/// Where the client sends requests.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Endpoint {
    /// The per-host node daemon's socket.
    NodeSocket(PathBuf),
    /// A server over TCP (`http://host:port` or `https://`).
    Server(String),
}

/// A `graft://` or `grafts://` repository URL. The parser lives in
/// `graft-proto` so the CLI and MCP accept the same URLs.
pub use graft_proto::RepoUrl;

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
    /// Another process holds this local repository's lock.
    Busy,
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
    /// someone else moved it. This is also the answer when two directories
    /// bind one workspace (a reused `--workspace` name or a copied
    /// `.graft/`): every write moves the head by compare-and-swap, so the
    /// first writer wins and the other gets this, with nothing overwritten.
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
    /// The stack conflicts with the line tip. `sync` moved the working copy
    /// to the tip with the stack's changes on top and wrote the conflicts
    /// into the files: resolve them, then `commit` and `land`.
    Conflicted(Box<workcopy::Conflicts>),
    /// These conflicts from a sync are not resolved yet: a marked file
    /// still has markers, or a sidecar is still there.
    Unresolved(Vec<RepoPath>),
    /// An earlier update stopped half way (`WorkingCopy::finish`, or
    /// `sync`, carries it on).
    Unfinished,
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
            Error::Busy => f.write_str("repository is in use by another process"),
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
                "workspace {workspace} moved on the server (head {}, this working copy has {}); \
                 another working copy may share it: give each directory its own clone",
                id_or_none(*server),
                id_or_none(*local)
            ),
            Error::StillQueued { pr_url } => write!(f, "still queued: {pr_url}"),
            Error::LandedDirty { landed, paths } => write!(
                f,
                "landed {landed}, but {} paths have local changes; commit or revert them and run land again",
                paths.len()
            ),
            Error::Conflicted(c) => match c.unresolved().count() {
                1 => write!(f, "1 file conflicts with {}", c.onto),
                n => write!(f, "{n} files conflict with {}", c.onto),
            },
            Error::Unresolved(paths) => match paths.len() {
                1 => f.write_str("1 conflict is not resolved yet"),
                n => write!(f, "{n} conflicts are not resolved yet"),
            },
            Error::Unfinished => f.write_str("an earlier update did not finish; run sync"),
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
    #[test]
    fn request_ids_differ_per_call_and_carry_a_nonce() {
        let a = super::request_id("cli", 1);
        let b = super::request_id("cli", 2);
        assert_ne!(a, b);
        assert_eq!(a.rsplit_once('-').unwrap().0, b.rsplit_once('-').unwrap().0);
        // prefix, pid, 16 hex digits of nonce, n.
        let parts: Vec<&str> = a.split('-').collect();
        assert_eq!(parts.len(), 4);
        assert_eq!(parts[2].len(), 16);
        assert!(a.len() <= 64);
    }

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
