//! Scope checks (`docs/design.md` §12.1, §12.2).
//!
//! Scopes arrive in the caller's [`Principal`] as text:
//! `repo:<name>:read`, `repo:<name>:write[:<path-prefix>]`,
//! `repo:<name>:land:<line>`, `repo:<name>:attest`, `repo:<name>:admin` and
//! `objects:read`. `admin` grants everything on its repository; `write` and
//! `land` grant `read`; `land:<line>` grants landing on that line only,
//! and creating it (`CreateLine`) when it does not exist yet;
//! `attest` grants `Attest` and nothing else, and only it and `admin` grant
//! `Attest`. This is the engine's minimal reading of the text form until
//! `graft_auth::authorize` (G1.9) replaces it.

use graft_model::{Change, RepoPath};
use graft_proto::{ApiError, CallContext, Principal};

/// What an operation needs.
#[derive(Clone, Copy, Debug)]
pub(crate) enum Need<'a> {
    /// Read objects (`objects:read` or any repository read).
    Objects,
    /// Read the repository.
    Read,
    /// Write, at a path when the write is path-scoped.
    Write(Option<&'a RepoPath>),
    /// Hold workspaces: any write scope on the repository, whatever its
    /// path prefix (the paths are checked per change).
    Workspace,
    /// Land on a line (`land:<line>`).
    Land(&'a str),
    /// Post an attestation (`attest`).
    Attest,
    /// Administer the repository.
    Admin,
}

/// Fails with `PermissionDenied` unless the caller holds `need` on `repo`.
pub(crate) fn require(cx: &CallContext, repo: &str, need: Need<'_>) -> Result<(), ApiError> {
    if granted(&cx.principal, repo, need) {
        return Ok(());
    }
    let (perm, path) = match need {
        Need::Objects | Need::Read => ("read", None),
        Need::Write(path) => ("write", path.map(|p| p.to_string())),
        Need::Workspace => ("write", None),
        Need::Land(line) => {
            return Err(ApiError::PermissionDenied {
                scope: format!("repo:{repo}:land:{line}"),
                path: None,
            });
        }
        Need::Attest => ("attest", None),
        Need::Admin => ("admin", None),
    };
    Err(ApiError::PermissionDenied {
        scope: format!("repo:{repo}:{perm}"),
        path,
    })
}

/// Fails with `PermissionDenied` unless the caller may write every path
/// `changes` touches (an empty change list needs an unrestricted write).
pub(crate) fn require_changes(
    cx: &CallContext,
    repo: &str,
    changes: &[Change],
) -> Result<(), ApiError> {
    if changes.is_empty() {
        return require(cx, repo, Need::Write(None));
    }
    changes
        .iter()
        .try_for_each(|change| require(cx, repo, Need::Write(Some(change.path()))))
}

/// Whether the caller administers `repo`.
pub(crate) fn is_admin(cx: &CallContext, repo: &str) -> bool {
    granted(&cx.principal, repo, Need::Admin)
}

fn granted(principal: &Principal, repo: &str, need: Need<'_>) -> bool {
    principal.scopes.iter().any(|s| grants(s, repo, need))
}

fn grants(scope: &str, repo: &str, need: Need<'_>) -> bool {
    if scope == "objects:read" {
        return matches!(need, Need::Objects);
    }
    let mut parts = scope.splitn(4, ':');
    if parts.next() != Some("repo") || parts.next() != Some(repo) {
        return false;
    }
    let perm = parts.next();
    let arg = parts.next();
    match (perm, need) {
        (Some("admin"), _) => true,
        (Some("read" | "land" | "write"), Need::Objects | Need::Read) => true,
        (Some("write"), Need::Workspace) => true,
        (Some("land"), Need::Land(line)) => arg == Some(line),
        (Some("attest"), Need::Attest) => arg.is_none(),
        (Some("write"), Need::Write(path)) => match (arg, path) {
            (None, _) => true,
            // A prefix-restricted write scope covers only paths beneath it.
            (Some(prefix), Some(path)) => {
                RepoPath::new(prefix).is_ok_and(|prefix| path.starts_with(&prefix))
            }
            (Some(_), None) => false,
        },
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn path(s: &str) -> RepoPath {
        RepoPath::new(s).expect("valid path")
    }

    #[test]
    fn admin_grants_everything_on_its_repo_only() {
        assert!(grants("repo:r:admin", "r", Need::Admin));
        assert!(grants("repo:r:admin", "r", Need::Write(Some(&path("a")))));
        assert!(!grants("repo:r:admin", "s", Need::Read));
    }

    #[test]
    fn write_grants_read_and_respects_its_prefix() {
        assert!(grants("repo:r:write", "r", Need::Read));
        assert!(grants(
            "repo:r:write:src",
            "r",
            Need::Write(Some(&path("src/a")))
        ));
        assert!(!grants(
            "repo:r:write:src",
            "r",
            Need::Write(Some(&path("srcx")))
        ));
        assert!(!grants("repo:r:write:src", "r", Need::Write(None)));
        assert!(!grants("repo:r:write", "r", Need::Admin));
    }

    #[test]
    fn any_write_scope_holds_workspaces() {
        assert!(grants("repo:r:write", "r", Need::Workspace));
        assert!(grants("repo:r:write:src", "r", Need::Workspace));
        assert!(grants("repo:r:admin", "r", Need::Workspace));
        assert!(!grants("repo:r:read", "r", Need::Workspace));
        assert!(!grants("repo:r:land:main", "r", Need::Workspace));
        assert!(!grants("repo:s:write", "r", Need::Workspace));
    }

    #[test]
    fn read_and_objects_scopes_are_read_only() {
        assert!(grants("repo:r:read", "r", Need::Objects));
        assert!(!grants("repo:r:read", "r", Need::Write(None)));
        assert!(grants("objects:read", "anything", Need::Objects));
        assert!(!grants("objects:read", "r", Need::Read));
        assert!(grants("repo:r:land:main", "r", Need::Read));
        assert!(!grants("repo:r:land:main", "r", Need::Admin));
    }

    #[test]
    fn attest_is_its_own_scope() {
        assert!(grants("repo:r:attest", "r", Need::Attest));
        assert!(grants("repo:r:admin", "r", Need::Attest));
        // Write no longer implies attest (WI-2272), nor does land or read.
        assert!(!grants("repo:r:write", "r", Need::Attest));
        assert!(!grants("repo:r:write:.ci", "r", Need::Attest));
        assert!(!grants("repo:r:land:main", "r", Need::Attest));
        assert!(!grants("repo:r:read", "r", Need::Attest));
        assert!(!grants("repo:s:attest", "r", Need::Attest));
        assert!(!grants("repo:r:attest:x", "r", Need::Attest));
        // And attest grants nothing else.
        for need in [
            Need::Objects,
            Need::Read,
            Need::Write(None),
            Need::Write(Some(&path("a"))),
            Need::Workspace,
            Need::Land("main"),
            Need::Admin,
        ] {
            assert!(!grants("repo:r:attest", "r", need), "{need:?}");
        }
    }

    #[test]
    fn land_scopes_name_their_line() {
        assert!(grants("repo:r:land:main", "r", Need::Land("main")));
        assert!(grants(
            "repo:r:land:release:1",
            "r",
            Need::Land("release:1")
        ));
        assert!(!grants("repo:r:land:main", "r", Need::Land("dev")));
        assert!(!grants("repo:r:land", "r", Need::Land("main")));
        assert!(!grants("repo:r:write", "r", Need::Land("main")));
        assert!(!grants("repo:s:land:main", "r", Need::Land("main")));
        assert!(grants("repo:r:admin", "r", Need::Land("main")));
    }
}
