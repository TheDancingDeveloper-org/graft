//! Landing on a graft-primary line, for `reconcile` and imported GitHub PRs
//! (`docs/design.md` §11.3, mode B). The mirror never moves a graft-primary
//! line itself: it hands a draft stack to [`Lander`], which in production is
//! graft's own `Land` ([`NativeLand`]), with its rebase, policy gate and CAS
//! (§8.1). Export then pushes the moved line like any other land.

use std::future::Future;

use graft_model::ChangesetId;
use graft_proto::api::{RepoApi, WorkspaceApi};
use graft_proto::types::{
    DraftState, GetChangesets, Land, Landed, MergeMode, OpenWorkspace, SetHead, WorkspaceRef,
    WorkspaceView,
};
use graft_proto::{ApiError, CallContext};

/// Lands a draft stack on a line.
pub trait Lander: Send + Sync {
    /// Lands the stack headed by `head` on `line` of `repo`, and returns the
    /// changeset the line now ends with (the head, or its rebased copy).
    fn land(
        &self,
        cx: &CallContext,
        repo: &str,
        line: &str,
        head: ChangesetId,
    ) -> impl Future<Output = Result<ChangesetId, ApiError>> + Send;
}

/// [`Lander`] through graft's `Land`.
pub struct NativeLand<'a, A> {
    api: &'a A,
}

impl<'a, A> NativeLand<'a, A> {
    /// Lands through `api`.
    pub fn new(api: &'a A) -> Self {
        NativeLand { api }
    }
}

impl<A: RepoApi> Lander for NativeLand<'_, A> {
    async fn land(
        &self,
        cx: &CallContext,
        repo: &str,
        line: &str,
        head: ChangesetId,
    ) -> Result<ChangesetId, ApiError> {
        let req = Land {
            repo: repo.to_owned(),
            head,
            line: line.to_owned(),
            squash: false,
            merge: MergeMode::File,
            deadline_ms: None,
            merged_from: None,
        };
        match self.api.land(cx, req).await? {
            Landed::Landed { landed, .. } => Ok(landed),
            // A graft-primary line never hands back to GitHub; if it
            // did, the line's authority is not what the mirror believes.
            _ => Err(ApiError::Rejected {
                policy: "line-authority".to_owned(),
                detail: format!("line {line} is not graft-primary on the server"),
            }),
        }
    }
}

/// The most changesets one imported stack may carry.
pub const MAX_IMPORTED_STACK: usize = 256;

/// The changeset an imported stack sits on: the first changeset down the
/// first-parent chain from `head` that is on a line (landed). One read per
/// changeset of the stack, at most [`MAX_IMPORTED_STACK`].
pub async fn stack_base<A: RepoApi>(
    api: &A,
    cx: &CallContext,
    repo: &str,
    head: ChangesetId,
) -> Result<ChangesetId, ApiError> {
    let refuse = |detail: String| ApiError::Rejected {
        policy: "imported-stack".to_owned(),
        detail,
    };
    let mut next = head;
    for _ in 0..=MAX_IMPORTED_STACK {
        let got = api
            .get_changesets(
                cx,
                GetChangesets {
                    repo: repo.to_owned(),
                    ids: vec![next],
                },
            )
            .await?;
        let view = got
            .found
            .into_iter()
            .next()
            .ok_or_else(|| refuse(format!("changeset {next} is not in graft")))?;
        if view.state == DraftState::Landed {
            return Ok(next);
        }
        next = view.changeset.first_parent().ok_or_else(|| {
            refuse(format!(
                "the stack under {head} shares no history with the line"
            ))
        })?;
    }
    Err(refuse(format!(
        "the stack under {head} is longer than {MAX_IMPORTED_STACK} changesets"
    )))
}

/// Holds the imported stack `base..head` as the drafts of `workspace` on
/// `line`, opening the workspace at `base` (or reopening it there when it
/// sits on another base), and renews its lease. Safe to repeat.
#[allow(clippy::too_many_arguments)]
pub async fn hold<A: WorkspaceApi>(
    api: &A,
    cx: &CallContext,
    repo: &str,
    line: &str,
    workspace: &str,
    base: ChangesetId,
    head: ChangesetId,
    lease_ms: u64,
) -> Result<WorkspaceView, ApiError> {
    let wref = WorkspaceRef {
        repo: repo.to_owned(),
        name: workspace.to_owned(),
    };
    let open = OpenWorkspace {
        repo: repo.to_owned(),
        name: Some(workspace.to_owned()),
        line: line.to_owned(),
        at: Some(base),
        lease_ms,
    };
    match api.open_workspace(cx, open.clone()).await {
        Ok(_) => {}
        // Open on another base (the PR moved onto a newer line tip).
        Err(ApiError::RefConflict { .. }) => {
            api.close_workspace(cx, wref.clone(), false).await?;
            api.open_workspace(cx, open).await?;
        }
        Err(e) => return Err(e),
    }
    let ws = api.renew_lease(cx, wref.clone(), lease_ms).await?;
    if ws.head == Some(head) {
        return Ok(ws);
    }
    api.set_head(
        cx,
        SetHead {
            workspace: wref,
            expected: ws.head,
            new: Some(head),
        },
    )
    .await
}
