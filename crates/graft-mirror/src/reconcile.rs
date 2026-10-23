//! `graft-mirror reconcile` (`docs/design.md` §11.3, mode B): an operator
//! resolves a line that export stopped because GitHub was written out of
//! band. Nothing here runs on its own; export never forces.
//!
//! - **Land** ([`Resolution::Land`]): import the stray commits, hold them as
//!   drafts in a `reconcile/<sha>` workspace on the commit export last
//!   pushed, and land them through graft (`Land`, so the line's policy and
//!   rebase apply). The lease becomes the stray commit, so the next export
//!   pushes the line on top of it, or, when `Land` had to rebase, replaces it
//!   with the rebased copies the operator asked for.
//! - **Discard** ([`Resolution::Discard`]): push the commit export last
//!   pushed back over the stray one, on a lease naming the stray one, so a
//!   push that raced the operator is refused rather than lost.
//!
//! Either way the stray commits are first fetched into the local mirror
//! under `refs/graft-mirror/stray/`, so nothing is deleted. Then the line
//! is marked not diverged, and export resumes from its cursor: no move is
//! skipped. A repeated run is safe: one that finds upstream back on the
//! lease only clears the stop.

use graft_git::GitSha;
use graft_git::import::{ImportConfig, import};
use graft_git::map::MirrorMap;
use graft_model::ChangesetId;
use graft_proto::api::{AdminApi, ObjectApi, RepoApi, WorkspaceApi};
use graft_proto::types::{Pair, WorkspaceRef};
use graft_proto::{ApiError, CallContext};

use crate::credentials::CredentialProvider;
use crate::db::MirrorDb;
use crate::follow::FollowLine;
use crate::git::{Mirror, PushOutcome};
use crate::land::{Lander, hold, stack_base};
use crate::{MirrorError, current_ref};

/// The lease of a reconcile workspace: long enough for a slow `Land`.
pub const RECONCILE_LEASE_MS: u64 = 60 * 60 * 1000;

/// What the operator chose.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Resolution {
    /// Import the stray commits as drafts and land them through graft.
    Land,
    /// Replace them with graft's history, on a lease.
    Discard,
}

/// What `reconcile` did.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Reconciled {
    /// What upstream held.
    pub stray: Option<GitSha>,
    /// Where the stray commits were kept in the local mirror.
    pub kept_as: Option<String>,
    /// The changeset the line ends with after landing them (`Land`).
    pub landed: Option<ChangesetId>,
    /// Upstream was already back where export left it; only the stop was
    /// cleared.
    pub already_resolved: bool,
}

fn config_err(what: impl Into<String>) -> MirrorError {
    MirrorError::Config(what.into())
}

/// The local ref the stray commits of `branch` at `sha` are kept under.
pub fn stray_ref(branch: &str, sha: GitSha) -> String {
    format!("refs/graft-mirror/stray/{branch}/{}", sha.to_hex())
}

/// Resolves the divergence of `line` as the operator chose.
#[allow(clippy::too_many_arguments)]
pub async fn reconcile<A, L, C>(
    api: &A,
    cx: &CallContext,
    db: &MirrorDb,
    mirror: &Mirror,
    lander: &L,
    credentials: &C,
    repo: &str,
    line: &FollowLine,
    resolution: Resolution,
) -> Result<Reconciled, MirrorError>
where
    A: AdminApi + ObjectApi + RepoApi + WorkspaceApi,
    L: Lander,
    C: CredentialProvider,
{
    let mut state = db
        .export_state(&line.line)?
        .ok_or_else(|| config_err(format!("export has not started line {}", line.line)))?;
    if !state.diverged {
        return Err(config_err(format!("line {} is not diverged", line.line)));
    }
    mirror.ensure().await?;
    let upstream = mirror
        .ls_remote(&[line.branch.as_str()])
        .await?
        .pop()
        .and_then(|(_, s)| s);
    let mut out = Reconciled {
        stray: upstream,
        kept_as: None,
        landed: None,
        already_resolved: false,
    };
    if upstream == state.sha {
        out.already_resolved = true;
    } else {
        if let Some(stray) = upstream {
            let kept = stray_ref(&line.branch, stray);
            mirror
                .fetch_refs(&[(format!("refs/heads/{}", line.branch), kept.clone())])
                .await?;
            // The branch may have moved again since ls-remote: keep and
            // judge what was fetched.
            let fetched = mirror
                .local_ref(&kept)
                .await?
                .ok_or_else(|| config_err("the stray commits could not be fetched"))?;
            if fetched != stray {
                return Err(config_err(
                    "upstream moved while reconciling; run reconcile again",
                ));
            }
            out.kept_as = Some(kept);
        }
        let token = if mirror.is_https() {
            Some(credentials.token().await?)
        } else {
            None
        };
        let bearer = token.as_ref().map(|t| t.expose());
        match resolution {
            Resolution::Discard => {
                let sha = state.sha.ok_or_else(|| {
                    config_err("export never pushed this line: there is nothing to restore")
                })?;
                let pushed = mirror
                    .push_lease(&line.branch, sha, upstream, bearer)
                    .await?;
                if pushed == PushOutcome::Rejected {
                    return Err(config_err(
                        "upstream moved while reconciling; nothing was changed, run reconcile again",
                    ));
                }
                tracing::warn!(
                    repo, line = %line.line, stray = ?upstream, restored = %sha,
                    kept_as = ?out.kept_as,
                    "mirror reconcile: stray commits replaced by graft's history"
                );
            }
            Resolution::Land => {
                let stray = upstream.ok_or_else(|| {
                    config_err("upstream has no branch: there is nothing to land; use --discard")
                })?;
                let kept = out.kept_as.clone().expect("fetched above");
                let landed = land_stray(
                    api, cx, db, mirror, lander, repo, line, &state, stray, &kept,
                )
                .await?;
                out.landed = Some(landed.1);
                // The stray commit is upstream's tip, and it is the export of
                // the imported head: the next push leases on it.
                state.changeset = Some(landed.0);
                state.sha = Some(stray);
                tracing::warn!(
                    repo, line = %line.line, stray = %stray, landed = %landed.1,
                    "mirror reconcile: stray commits landed through graft"
                );
            }
        }
    }
    state.diverged = false;
    state.stray = None;
    db.set_export_state(&line.line, &state)?;
    Ok(out)
}

/// Imports the stray commits on `kept` and lands them. Returns the imported
/// head and the changeset the line ends with.
#[allow(clippy::too_many_arguments)]
async fn land_stray<A, L>(
    api: &A,
    cx: &CallContext,
    db: &MirrorDb,
    mirror: &Mirror,
    lander: &L,
    repo: &str,
    line: &FollowLine,
    state: &crate::db::ExportState,
    stray: GitSha,
    kept: &str,
) -> Result<(ChangesetId, ChangesetId), MirrorError>
where
    A: AdminApi + ObjectApi + RepoApi + WorkspaceApi,
    L: Lander,
{
    if let Some(lease) = state.sha
        && !mirror.is_ancestor(lease, stray).await?
    {
        return Err(config_err(
            "upstream was rewritten, not added to: its commits do not descend from the last \
             export; use --discard",
        ));
    }
    let config = ImportConfig {
        git_dir: mirror.dir().to_owned(),
        repo: repo.to_owned(),
        lines: vec![(kept.to_owned(), line.line.clone())],
        tags: false,
        allow_lossy: Vec::new(),
        batch: 256,
    };
    let report = import(api, cx, db, &config).await?;
    let head = report
        .lines
        .first()
        .map(|(_, id)| *id)
        .or(db.changeset_of(stray)?)
        .ok_or_else(|| config_err("the stray commits did not import"))?;
    let tip = current_ref(api, cx, repo, &line.line)
        .await?
        .ok_or_else(|| config_err(format!("line {} has no tip", line.line)))?;
    // Landed already by an earlier run that stopped before recording it.
    let on_line = head == tip
        || api
            .is_ancestor(
                cx,
                Pair {
                    repo: repo.to_owned(),
                    a: head,
                    b: tip,
                },
            )
            .await?;
    if on_line {
        return Ok((head, tip));
    }
    let base = stack_base(api, cx, repo, head).await?;
    let workspace = format!("reconcile/{}", &stray.to_hex()[..12]);
    hold(
        api,
        cx,
        repo,
        &line.line,
        &workspace,
        base,
        head,
        RECONCILE_LEASE_MS,
    )
    .await?;
    let landed = lander.land(cx, repo, &line.line, head).await?;
    let wref = WorkspaceRef {
        repo: repo.to_owned(),
        name: workspace,
    };
    match api.close_workspace(cx, wref, false).await {
        Ok(()) | Err(ApiError::NotFound { .. }) => {}
        Err(e) => return Err(e.into()),
    }
    Ok((head, landed))
}
