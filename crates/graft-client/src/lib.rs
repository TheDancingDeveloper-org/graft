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
pub mod ignore;
pub mod remote;
pub mod sparse;
pub mod workcopy;
pub mod workspace;

pub use remote::Remote;
pub use sparse::Sparse;

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

/// A `graft://` or `grafts://` repository URL: endpoint plus repository.
///
/// `graft://host[:port]/repo` is plain HTTP; `grafts://` is TLS. An optional
/// `?line=` selects the line. Userinfo is rejected so a token cannot hide in
/// the URL. `unix:` sockets are not this form.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RepoUrl {
    /// `http://host[:port]` or `https://host[:port]`.
    pub endpoint: String,
    /// Repository name, one path segment.
    pub repo: String,
    /// Line from `?line=`, when the URL named one.
    pub line: Option<String>,
}

impl RepoUrl {
    /// Parses a clone URL. `Err` is one line of usage text.
    pub fn parse(input: &str) -> std::result::Result<Self, String> {
        let s = input.trim();
        let (tls, rest) = if let Some(rest) = s.strip_prefix("grafts://") {
            (true, rest)
        } else if let Some(rest) = s.strip_prefix("graft://") {
            (false, rest)
        } else {
            return Err("expected graft://host[:port]/repo or grafts://host[:port]/repo".into());
        };
        if rest.contains('@') {
            return Err(
                "a graft URL must not carry userinfo; pass the token with --token-file or graft login"
                    .into(),
            );
        }
        let (authority, after) = match rest.split_once('/') {
            Some((authority, after)) if !authority.is_empty() && !after.is_empty() => {
                (authority, after)
            }
            _ => {
                return Err("expected graft://host[:port]/repo".into());
            }
        };
        let (path, query) = after
            .split_once('?')
            .map(|(path, query)| (path, Some(query)))
            .unwrap_or((after, None));
        if path.is_empty()
            || path.contains('/')
            || path.contains('#')
            || path.chars().any(char::is_control)
            || path.chars().any(char::is_whitespace)
        {
            return Err("repository name in a graft URL must be a single path segment".into());
        }
        validate_authority(authority)?;
        let mut line = None;
        if let Some(query) = query {
            if query.is_empty() {
                return Err("a graft URL query must be line=<name>".into());
            }
            for pair in query.split('&') {
                let Some((key, value)) = pair.split_once('=') else {
                    return Err("a graft URL query must be line=<name>".into());
                };
                if key != "line" || value.is_empty() || line.is_some() {
                    return Err("a graft URL query must be line=<name>".into());
                }
                if value.chars().any(char::is_control) || value.contains('/') {
                    return Err("line name in a graft URL is not valid".into());
                }
                line = Some(value.to_owned());
            }
        }
        let scheme = if tls { "https" } else { "http" };
        Ok(RepoUrl {
            endpoint: format!("{scheme}://{authority}"),
            repo: path.to_owned(),
            line,
        })
    }
}

/// Host, optional port, including `[IPv6]`.
fn validate_authority(authority: &str) -> std::result::Result<(), String> {
    let (host, port) = if let Some(rest) = authority.strip_prefix('[') {
        let Some((host, after)) = rest.split_once(']') else {
            return Err("IPv6 host in a graft URL must be [addr][:port]".into());
        };
        if host.is_empty() || host.contains('[') {
            return Err("IPv6 host in a graft URL must be [addr][:port]".into());
        }
        let port = if let Some(port) = after.strip_prefix(':') {
            if port.is_empty() {
                return Err("graft URL is missing a port after ':'".into());
            }
            Some(port)
        } else if after.is_empty() {
            None
        } else {
            return Err("expected graft://[ipv6][:port]/repo".into());
        };
        (host, port)
    } else if let Some((host, port)) = authority.rsplit_once(':') {
        if host.is_empty() {
            return Err("graft URL is missing a host".into());
        }
        if port.is_empty() {
            return Err("graft URL is missing a port after ':'".into());
        }
        (host, Some(port))
    } else {
        (authority, None)
    };
    if host
        .chars()
        .any(|c| c.is_control() || c.is_whitespace() || c == '/')
    {
        return Err("graft URL host is not valid".into());
    }
    if let Some(port) = port {
        let value: u16 = port
            .parse()
            .map_err(|_| "graft URL port must be 1 to 65535".to_owned())?;
        if value == 0 {
            return Err("graft URL port must be 1 to 65535".into());
        }
    }
    Ok(())
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
    fn repo_url_carries_endpoint_repo_and_line() {
        let url = RepoUrl::parse("graft://graft-dev:17888/rusttorrent?line=trial").unwrap();
        assert_eq!(url.endpoint, "http://graft-dev:17888");
        assert_eq!(url.repo, "rusttorrent");
        assert_eq!(url.line.as_deref(), Some("trial"));

        let tls = RepoUrl::parse("grafts://example.test/app").unwrap();
        assert_eq!(tls.endpoint, "https://example.test");
        assert_eq!(tls.repo, "app");
        assert_eq!(tls.line, None);

        let v6 = RepoUrl::parse("graft://[2001:db8::1]/repo").unwrap();
        assert_eq!(v6.endpoint, "http://[2001:db8::1]");
        let v6_port = RepoUrl::parse("graft://[2001:db8::1]:17888/repo").unwrap();
        assert_eq!(v6_port.endpoint, "http://[2001:db8::1]:17888");
    }

    #[test]
    fn repo_url_rejects_userinfo_and_a_bad_port() {
        let userinfo = RepoUrl::parse("graft://token@host/repo").unwrap_err();
        assert!(userinfo.contains("userinfo"), "{userinfo}");
        let port = RepoUrl::parse("graft://host:/repo").unwrap_err();
        assert!(port.contains("port"), "{port}");
        assert!(RepoUrl::parse("http://host/repo").is_err());
        assert!(RepoUrl::parse("graft://host").is_err());
        assert!(RepoUrl::parse("unix:/run/graft.sock").is_err());
        assert!(RepoUrl::parse("graft://[::1").is_err());
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
