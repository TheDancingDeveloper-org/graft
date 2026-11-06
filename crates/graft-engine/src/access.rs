//! Scope checks (`docs/design.md` §12.1, §12.2).
//!
//! Scopes arrive in the caller's [`Principal`] as text:
//! `repo:<name>:read`, `repo:<name>:write[:<path-prefix>]`,
//! `repo:<name>:land:<line>`, `repo:<name>:attest`, `repo:<name>:admin` and
//! `objects:read`. `admin` grants everything on its repository; `write` and
//! `land` grant `read`; `land:<line>` grants landing on that line only,
//! and creating it (`CreateLine`) when it does not exist yet;
//! `attest` grants `Attest` and nothing else, and only it and `admin` grant
//! `Attest`; `drafts` grants `Drafts` (the draft overlap query, D24) and
//! nothing else, and only it and `admin` grant `Drafts`. A `*` repository
//! (`repo:*:<perm>[:<arg>]`) holds on every repository. `repo:*:mirror`
//! grants `Mirror` to the `mirror` principal kind only, and `admin` does not
//! grant it. The same set reads (`Read`, `Objects`) only a repository bound
//! to a mirror, which needs a store read: [`Engine::require_read`] checks
//! that, and every read goes through it. `repos:admin` is checked by
//! [`holds_repos_admin`] (D41). This is the engine's minimal reading of the
//! text form until `graft_auth::authorize` (G1.9) replaces it.

use graft_model::{Change, RepoPath};
use graft_proto::{ApiError, CallContext, Principal, PrincipalKind};
use graft_store::{
    LineSlot, MetaStore, MirrorQuery, MirrorRequestKind, MirrorRequestQuery, MirrorRequestRecord,
    MirrorRequestState, ObjectStore,
};

use crate::{Engine, errors};

/// Claimed cuts read when a changeset write looks for one held under the
/// lease. Claims outlived by their lease are settled back to pending as
/// they are read, so few stay claimed at once.
const CLAIMED_CUTS_PAGE: u32 = 32;

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
    /// See other workspaces' draft paths (`drafts`).
    Drafts,
    /// Administer the repository.
    Admin,
    /// Act as the mirror service (`repo:*:mirror`, `mirror` kind only).
    Mirror,
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
        Need::Drafts => ("drafts", None),
        Need::Admin => ("admin", None),
        Need::Mirror => ("mirror", None),
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

/// Whether the caller is the mirror service, holding the mirror permission
/// set on `repo`.
pub(crate) fn is_mirror(cx: &CallContext, repo: &str) -> bool {
    granted(&cx.principal, repo, Need::Mirror)
}

/// Whether the caller holds `repos:admin`: create, delete and list
/// repositories (D41).
pub(crate) fn holds_repos_admin(cx: &CallContext) -> bool {
    cx.principal.scopes.iter().any(|s| s == "repos:admin")
}

fn granted(principal: &Principal, repo: &str, need: Need<'_>) -> bool {
    if matches!(need, Need::Mirror) && principal.kind != PrincipalKind::Mirror {
        return false;
    }
    principal.scopes.iter().any(|s| grants(s, repo, need))
}

impl<M: MetaStore, O: ObjectStore> Engine<M, O> {
    /// Fails with `PermissionDenied` unless the caller may read `repo`
    /// (`need` is `Read` or `Objects`). Beyond the scopes, the mirror
    /// service reads a repository only while it is bound to a mirror (has a
    /// mirror row), to poll and import it (D41): one indexed read, and only
    /// for the mirror kind. An unknown repository is refused the same way,
    /// so the caller learns nothing about it.
    pub(crate) async fn require_read(
        &self,
        cx: &CallContext,
        repo: &str,
        need: Need<'_>,
    ) -> Result<(), ApiError> {
        debug_assert!(matches!(need, Need::Read | Need::Objects));
        let denied = match require(cx, repo, need) {
            Ok(()) => return Ok(()),
            Err(denied) => denied,
        };
        if self.is_bound_mirror(cx, repo).await? {
            Ok(())
        } else {
            Err(denied)
        }
    }

    /// Fails with `PermissionDenied` unless the caller holds `need` on
    /// `repo`, or is the mirror service and `repo` is bound to a mirror.
    /// For the writes a poll makes before it moves a line: uploading objects
    /// and importing changesets add unlanded content only, and the line and
    /// its marker still move by their own checked, fenced CAS (D41).
    pub(crate) async fn require_or_bound_mirror(
        &self,
        cx: &CallContext,
        repo: &str,
        need: Need<'_>,
    ) -> Result<(), ApiError> {
        let denied = match require(cx, repo, need) {
            Ok(()) => return Ok(()),
            Err(denied) => denied,
        };
        if self.is_bound_mirror(cx, repo).await? {
            Ok(())
        } else {
            Err(denied)
        }
    }

    /// `checked`, a write check, unless it denied the mirror service running
    /// a batch cut on `repo`: `repo` is bound to a mirror and holds a cut
    /// claimed under the live lease's epoch (D41 §3.5, WI-3426). A cut
    /// creates and rebases unlanded changesets only, which name no line; the
    /// line it then moves is checked by [`Engine::cut_lease`]. Reads the
    /// store only after a denial.
    pub(crate) async fn or_cut(
        &self,
        cx: &CallContext,
        repo: &str,
        checked: Result<(), ApiError>,
    ) -> Result<(), ApiError> {
        let Err(denied) = checked else {
            return Ok(());
        };
        if self.cut_claimed(cx, repo, None).await?.is_some() {
            Ok(())
        } else {
            Err(denied)
        }
    }

    /// The lease epoch of the claimed batch cut of `line` on `repo`, when
    /// the caller is the mirror service and `repo` is bound to a mirror:
    /// `line`'s batch slot names a cut that is claimed. The service may move
    /// that line, and only under that epoch (D41 §3.5, WI-3426).
    pub(crate) async fn cut_lease(
        &self,
        cx: &CallContext,
        repo: &str,
        line: &str,
    ) -> Result<Option<u64>, ApiError> {
        self.cut_claimed(cx, repo, Some(line)).await
    }

    /// The claimer's epoch of a claimed cut on `repo` (of `line`, when
    /// given), for the mirror service on a bound repository, while the
    /// lease is held under that epoch: a claim outlived by its lease grants
    /// nothing, even before it is settled back to pending. Indexed reads.
    async fn cut_claimed(
        &self,
        cx: &CallContext,
        repo: &str,
        line: Option<&str>,
    ) -> Result<Option<u64>, ApiError> {
        if !self.is_bound_mirror(cx, repo).await? {
            return Ok(None);
        }
        let Some(id) = self.meta.repo(repo).await.map_err(errors::store)? else {
            return Ok(None);
        };
        let now_ms = self.clock.now().ms;
        let Some(lease) = self
            .meta
            .get_lease(id)
            .await
            .map_err(errors::store)?
            .filter(|l| l.held(l.epoch, now_ms))
        else {
            return Ok(None);
        };
        let claims = |r: &MirrorRequestRecord| {
            r.kind == MirrorRequestKind::CutBatch
                && r.state == MirrorRequestState::Claimed
                && r.claimed_by_epoch == Some(lease.epoch)
                && line.is_none_or(|l| r.line == l)
        };
        let claimed = match line {
            Some(line) => {
                let Some(rid) = self
                    .meta
                    .line_slot(id, line, LineSlot::Batch)
                    .await
                    .map_err(errors::store)?
                else {
                    return Ok(None);
                };
                self.meta
                    .get_mirror_request(id, rid)
                    .await
                    .map_err(errors::store)?
                    .is_some_and(|r| claims(&r))
            }
            // A stale claim may sort first, so read a page, not one row.
            None => self
                .meta
                .list_mirror_requests(
                    id,
                    MirrorRequestQuery {
                        kind: Some(MirrorRequestKind::CutBatch),
                        states: vec![MirrorRequestState::Claimed],
                        limit: CLAIMED_CUTS_PAGE,
                        ..MirrorRequestQuery::default()
                    },
                )
                .await
                .map_err(errors::store)?
                .iter()
                .any(claims),
        };
        Ok(claimed.then_some(lease.epoch))
    }

    /// Whether the caller is the mirror service and `repo` is bound to a
    /// mirror (has a mirror row): one indexed read, and only for the mirror
    /// kind. An unknown repository is not bound.
    async fn is_bound_mirror(&self, cx: &CallContext, repo: &str) -> Result<bool, ApiError> {
        if !is_mirror(cx, repo) {
            return Ok(false);
        }
        let Some(id) = self.meta.repo(repo).await.map_err(errors::store)? else {
            return Ok(false);
        };
        let bound = self
            .meta
            .list_mirrors(MirrorQuery {
                repo: Some(id),
                enabled: None,
                after: None,
                limit: 1,
            })
            .await
            .map_err(errors::store)?;
        Ok(!bound.is_empty())
    }
}

fn grants(scope: &str, repo: &str, need: Need<'_>) -> bool {
    if scope == "objects:read" {
        return matches!(need, Need::Objects);
    }
    let mut parts = scope.splitn(4, ':');
    if parts.next() != Some("repo") || !parts.next().is_some_and(|r| r == "*" || r == repo) {
        return false;
    }
    let perm = parts.next();
    let arg = parts.next();
    match (perm, need) {
        (Some("mirror"), Need::Mirror) => arg.is_none(),
        (_, Need::Mirror) => false,
        (Some("admin"), _) => true,
        (Some("read" | "land" | "write"), Need::Objects | Need::Read) => true,
        (Some("write"), Need::Workspace) => true,
        (Some("land"), Need::Land(line)) => arg == Some(line),
        (Some("attest"), Need::Attest) => arg.is_none(),
        (Some("drafts"), Need::Drafts) => arg.is_none(),
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
    fn drafts_is_its_own_scope() {
        assert!(grants("repo:r:drafts", "r", Need::Drafts));
        assert!(grants("repo:r:admin", "r", Need::Drafts));
        for scope in [
            "repo:r:read",
            "repo:r:write",
            "repo:r:land:main",
            "repo:r:attest",
        ] {
            assert!(!grants(scope, "r", Need::Drafts), "{scope}");
        }
        assert!(!grants("repo:s:drafts", "r", Need::Drafts));
        assert!(!grants("repo:r:drafts:src", "r", Need::Drafts));
        for need in [
            Need::Objects,
            Need::Read,
            Need::Write(None),
            Need::Workspace,
            Need::Admin,
        ] {
            assert!(!grants("repo:r:drafts", "r", need), "{need:?}");
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

    #[test]
    fn a_wildcard_repository_holds_everywhere() {
        assert!(grants("repo:*:read", "r", Need::Read));
        assert!(grants("repo:*:land:main", "s", Need::Land("main")));
        assert!(!grants("repo:*:read", "r", Need::Write(None)));
    }

    #[test]
    fn only_the_mirror_kind_holds_the_mirror_set() {
        let principal = |kind, scope: &str| Principal {
            subject: "m".into(),
            kind,
            session: None,
            work_item: None,
            scopes: vec![scope.into()],
        };
        let mirror = principal(PrincipalKind::Mirror, "repo:*:mirror");
        assert!(granted(&mirror, "r", Need::Mirror));
        // Its reads depend on the repository's binding: `require_read`.
        for need in [
            Need::Read,
            Need::Objects,
            Need::Write(None),
            Need::Write(Some(&path("a"))),
            Need::Workspace,
            Need::Admin,
            Need::Land("main"),
            Need::Attest,
            Need::Drafts,
        ] {
            assert!(!granted(&mirror, "r", need), "{need:?}");
        }
        // The scope on another kind, and admin on the mirror kind, are not it.
        assert!(!granted(
            &principal(PrincipalKind::Service, "repo:*:mirror"),
            "r",
            Need::Mirror
        ));
        assert!(!granted(
            &principal(PrincipalKind::Mirror, "repo:r:admin"),
            "r",
            Need::Mirror
        ));
        assert!(!grants("repo:*:admin", "r", Need::Mirror));
    }
}
