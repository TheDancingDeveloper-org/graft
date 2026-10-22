//! Connecting to a server or node, finding the working copy, and the call
//! context. The bearer token comes from `--token-file` or `GRAFT_TOKEN`, and
//! is never printed or put in an argument.

use std::env;
use std::fs;
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};

use graft_client::workcopy::GRAFT_DIR;
use graft_client::workcopy::WorkingCopy;
use graft_client::{Endpoint, Remote, discover};
use graft_model::ChangesetId;
use graft_proto::api::RepoApi;
use graft_proto::context::{CallContext, Principal, PrincipalKind};
use graft_proto::types::ResolveRef;

use crate::cli::{DEFAULT_LINE, Global};
use crate::error::Failure;

/// The environment variable that carries the bearer token.
pub(crate) const TOKEN_ENV: &str = "GRAFT_TOKEN";

/// A connected session: the remote, the repository, and the working copy
/// when the current directory is inside one.
pub(crate) struct Conn {
    pub(crate) remote: Remote,
    pub(crate) repo: String,
}

/// A single-threaded runtime: the CLI makes one call at a time.
pub(crate) fn runtime() -> Result<tokio::runtime::Runtime, Failure> {
    Ok(tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?)
}

/// The working copy containing the current directory, if there is one.
/// A `.graft` directory that fails to load is an error, not "no working copy".
pub(crate) fn working_copy() -> Result<Option<WorkingCopy>, Failure> {
    let cwd = env::current_dir()?;
    if !cwd.ancestors().any(|d| d.join(GRAFT_DIR).is_dir()) {
        return Ok(None);
    }
    Ok(Some(WorkingCopy::open(&cwd)?))
}

/// The working copy the command needs, or a usage error saying so.
pub(crate) fn require_working_copy() -> Result<WorkingCopy, Failure> {
    working_copy()?.ok_or_else(|| Failure::usage("not inside a working copy (no .graft directory)"))
}

/// The call context for one request. Request ids are for logs and
/// idempotency, so they only need to be unique per process.
pub(crate) fn call_context() -> CallContext {
    static NEXT: AtomicU64 = AtomicU64::new(1);
    let n = NEXT.fetch_add(1, Ordering::Relaxed);
    CallContext {
        principal: Principal {
            subject: env::var("USER").unwrap_or_else(|_| "graft-cli".to_owned()),
            kind: PrincipalKind::Human,
            session: None,
            work_item: None,
            scopes: Vec::new(),
        },
        request_id: format!("cli-{}-{n}", std::process::id()),
        deadline_ms: None,
    }
}

/// Connects. The endpoint is `--endpoint`, else `GRAFT_ENDPOINT`, the node
/// socket, or the working copy's configured server, in that order. The
/// repository is `--repo`, else the working copy's.
pub(crate) async fn connect(g: &Global, wc: Option<&WorkingCopy>) -> Result<Conn, Failure> {
    let repo = match (&g.repo, wc) {
        (Some(repo), _) => repo.clone(),
        (None, Some(wc)) => wc.config().repo.clone(),
        (None, None) => {
            return Err(Failure::usage(
                "no repository: pass --repo, or run inside a working copy",
            ));
        }
    };
    let endpoint = match &g.endpoint {
        Some(text) => Endpoint::parse(text),
        None => {
            let configured = wc.and_then(|wc| wc.config().endpoint.clone());
            discover(configured.as_deref()).ok_or_else(|| {
                Failure::usage(
                    "no endpoint: pass --endpoint, set GRAFT_ENDPOINT, or run inside a working copy",
                )
            })?
        }
    };
    let remote = Remote::connect(endpoint).await?;
    let remote = match read_token(g)? {
        Some(token) => remote.with_token(token),
        None => remote,
    };
    Ok(Conn { remote, repo })
}

/// The bearer token from `--token-file`, else `GRAFT_TOKEN`. Surrounding
/// whitespace is dropped; the value is never echoed.
pub(crate) fn read_token(g: &Global) -> Result<Option<String>, Failure> {
    if let Some(path) = &g.token_file {
        return token_from_file(path).map(Some);
    }
    Ok(env::var(TOKEN_ENV)
        .ok()
        .map(|t| t.trim().to_owned())
        .filter(|t| !t.is_empty()))
}

fn token_from_file(path: &Path) -> Result<String, Failure> {
    let text = fs::read_to_string(path).map_err(|e| {
        Failure::new(
            crate::Exit::Failure,
            "token",
            format!(
                "cannot read the token file {}: {}",
                path.display(),
                e.kind()
            ),
        )
    })?;
    let token = text.trim().to_owned();
    if token.is_empty() {
        return Err(Failure::new(
            crate::Exit::Failure,
            "token",
            format!("the token file {} is empty", path.display()),
        ));
    }
    Ok(token)
}

impl Conn {
    /// Resolves a changeset id or a ref name to a changeset id.
    pub(crate) async fn resolve(
        &self,
        cx: &CallContext,
        rev: &str,
    ) -> Result<ChangesetId, Failure> {
        if let Ok(id) = ChangesetId::parse(rev) {
            return Ok(id);
        }
        let view = self
            .remote
            .resolve_ref(
                cx,
                ResolveRef {
                    repo: self.repo.clone(),
                    name: rev.to_owned(),
                },
            )
            .await?;
        Ok(view.target)
    }
}

/// The snapshot a read defaults to: the working copy's base, else the
/// `main` line. Resolved by [`Conn::resolve`].
pub(crate) fn default_rev(wc: Option<&WorkingCopy>) -> String {
    match wc {
        Some(wc) => wc.state().base.to_string(),
        None => DEFAULT_LINE.to_owned(),
    }
}

/// Parses a repository path, a usage error when it is not one.
pub(crate) fn repo_path(text: &str) -> Result<graft_model::RepoPath, Failure> {
    graft_model::RepoPath::new(text)
        .map_err(|e| Failure::usage(format!("invalid path {text:?}: {e}")))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn token_file_is_trimmed_and_never_empty() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("token");
        fs::write(&file, "  abc123\n").unwrap();
        assert_eq!(token_from_file(&file).unwrap(), "abc123");
        fs::write(&file, " \n").unwrap();
        assert!(token_from_file(&file).is_err());
    }

    #[test]
    fn default_rev_is_main_outside_a_working_copy() {
        assert_eq!(default_rev(None), "main");
    }
}
