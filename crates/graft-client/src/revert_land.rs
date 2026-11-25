//! `Revert` with `land` (D42): the server builds the inverse of a changeset
//! on a line and lands it in the same call, for a caller that holds
//! `land:<line>` on the named repository and not `write`.
//!
//! There is no version gate. A server that predates D42 ignores `land` and
//! answers with the inverse as an unlanded changeset, so the answer is
//! checked against the line before it is believed.

use graft_model::ChangesetId;
use graft_proto::ApiError;
use graft_proto::CallContext;
use graft_proto::api::RepoApi;
use graft_proto::error::ApiResult;
use graft_proto::types::{Pair, ResolveRef, Revert};

/// The rule an unlanded answer to a revert-land is reported under. It is the
/// rule a current server uses for the same fault.
pub const REVERT_NOT_LANDED: &str = "revert-not-landed";

/// Reverts `changeset` and lands the inverse on `line` in one call, then
/// checks that the answer is on the line.
///
/// Needs `read` and a `land:<line>` scope naming `repo` (a wildcard
/// repository scope does not do), not `write`. Returns the landed revert.
/// `revert-empty` (the change is already undone) and every other server
/// refusal come back as the server's own error; a changeset the server
/// returned that the line does not hold is `InvalidChange` with rule
/// [`REVERT_NOT_LANDED`].
pub async fn revert_and_land<A: RepoApi>(
    api: &A,
    cx: &CallContext,
    repo: &str,
    changeset: ChangesetId,
    line: &str,
    check_url: Option<String>,
) -> ApiResult<ChangesetId> {
    let tip = resolve_line(api, cx, repo, line).await?;
    let id = api
        .revert(
            cx,
            Revert {
                repo: repo.to_owned(),
                changeset,
                onto: tip,
                workspace: None,
                check_url,
                land: Some(line.to_owned()),
            },
        )
        .await?;
    verify_landed(api, cx, repo, line, id).await?;
    Ok(id)
}

/// Fails with [`REVERT_NOT_LANDED`] unless `id` is on `line` (the tip or
/// reachable from it). One ref read and one ancestry query, never a walk.
pub async fn verify_landed<A: RepoApi>(
    api: &A,
    cx: &CallContext,
    repo: &str,
    line: &str,
    id: ChangesetId,
) -> ApiResult<()> {
    let tip = resolve_line(api, cx, repo, line).await?;
    let on_line = id == tip
        || api
            .is_ancestor(
                cx,
                Pair {
                    repo: repo.to_owned(),
                    a: id,
                    b: tip,
                },
            )
            .await?;
    if on_line {
        Ok(())
    } else {
        Err(ApiError::InvalidChange {
            path: String::new(),
            rule: REVERT_NOT_LANDED.to_owned(),
        })
    }
}

async fn resolve_line<A: RepoApi>(
    api: &A,
    cx: &CallContext,
    repo: &str,
    line: &str,
) -> ApiResult<ChangesetId> {
    Ok(api
        .resolve_ref(
            cx,
            ResolveRef {
                repo: repo.to_owned(),
                name: line.to_owned(),
            },
        )
        .await?
        .target)
}
