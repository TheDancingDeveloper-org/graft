//! Mirror requests (D41 §3.5) as the service runs them: claimed under the
//! repository's lease epoch, executed, then completed or failed with a
//! typed code. A batch's branch is `graft/batch-<id>` from the request's
//! server id, so a cut that crashed between opening its PR and recording it
//! adopts that PR on the next claim instead of opening a second.

use std::future::Future;
use std::path::Path;

use graft_proto::pb::{self, MirrorRequestKind};
use graft_proto::{ApiError, error::ApiResult};

use super::control::Control;
use crate::batch::BRANCH_PREFIX;
use crate::github::{Forge, ForgeError, NewPull, Pull};

/// The most requests one task runs between two polls.
pub const REQUESTS_PER_TURN: usize = 4;

/// The batch branch of request `id`.
pub fn batch_branch(id: u64) -> String {
    format!("{BRANCH_PREFIX}{id}")
}

/// Why a request failed: a typed code the server records.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RequestFailure(pub &'static str);

/// Code: this service does not run this kind of request yet.
pub const UNSUPPORTED: RequestFailure = RequestFailure("unsupported");
/// Code: the forge refused.
pub const FORGE_REFUSED: RequestFailure = RequestFailure("forge_refused");
/// Code: the repository's mirror is disabled or removed.
pub const MIRROR_DISABLED: RequestFailure = RequestFailure("mirror_disabled");
/// Code: the request is malformed.
pub const BAD_REQUEST: RequestFailure = RequestFailure("bad_request");
/// Code: the service failed.
pub const INTERNAL: RequestFailure = RequestFailure("internal");

/// The PR calls a batch cut needs.
pub trait PullDesk: Send + Sync {
    /// The open PR whose head is `branch`, if any.
    fn find_open(
        &self,
        branch: &str,
    ) -> impl Future<Output = Result<Option<Pull>, ForgeError>> + Send;
    /// Opens a PR.
    fn create(&self, new: &NewPull) -> impl Future<Output = Result<Pull, ForgeError>> + Send;
}

impl<F: Forge> PullDesk for F {
    async fn find_open(&self, branch: &str) -> Result<Option<Pull>, ForgeError> {
        Ok(self.find_pull(branch).await?.filter(|p| p.open))
    }

    async fn create(&self, new: &NewPull) -> Result<Pull, ForgeError> {
        self.create_pull(new).await
    }
}

/// The repository work behind each request kind.
pub trait RequestRunner: Send + Sync {
    /// Pushes the batch `req` asks for to `branch` and returns the PR to
    /// open. Called only when no PR is open on `branch`; pushing the same
    /// branch again must be safe.
    fn prepare_batch(
        &self,
        req: &pb::MirrorRequest,
        branch: &str,
    ) -> impl Future<Output = Result<NewPull, RequestFailure>> + Send;
    /// Runs a settle, exclude or rewind request.
    fn run(
        &self,
        req: &pb::MirrorRequest,
    ) -> impl Future<Output = Result<pb::MirrorRequestOutcome, RequestFailure>> + Send;
}

/// A runner for the kinds not wired to the batcher yet: refuses each with
/// [`UNSUPPORTED`].
pub struct Unwired;

impl RequestRunner for Unwired {
    async fn prepare_batch(
        &self,
        _req: &pb::MirrorRequest,
        _branch: &str,
    ) -> Result<NewPull, RequestFailure> {
        Err(UNSUPPORTED)
    }

    async fn run(
        &self,
        _req: &pb::MirrorRequest,
    ) -> Result<pb::MirrorRequestOutcome, RequestFailure> {
        Err(UNSUPPORTED)
    }
}

/// A desk with no forge: finds nothing and opens nothing.
pub struct NoDesk;

impl PullDesk for NoDesk {
    async fn find_open(&self, _branch: &str) -> Result<Option<Pull>, ForgeError> {
        Ok(None)
    }

    async fn create(&self, _new: &NewPull) -> Result<Pull, ForgeError> {
        Err(ForgeError::Refused("no forge is configured".to_owned()))
    }
}

fn pr_outcome(p: &Pull, branch: &str) -> pb::MirrorRequestOutcome {
    pb::MirrorRequestOutcome {
        pr_number: Some(p.number),
        pr_url: Some(p.url.clone()),
        branch: Some(branch.to_owned()),
        lands: Vec::new(),
        error_code: None,
    }
}

/// Runs a claimed cut: adopts the PR already open on its branch, else
/// prepares the branch and opens one. `Err` leaves the request claimed for
/// a later try (the forge or the server failed transiently).
async fn cut<D: PullDesk, R: RequestRunner>(
    desk: &D,
    runner: &R,
    req: &pb::MirrorRequest,
) -> Result<Result<pb::MirrorRequestOutcome, RequestFailure>, ForgeError> {
    let branch = batch_branch(req.id);
    if let Some(p) = desk.find_open(&branch).await? {
        tracing::info!(
            repo = %req.repo, request = req.id, pr = p.number,
            "mirror request: adopted the PR an earlier attempt opened"
        );
        return Ok(Ok(pr_outcome(&p, &branch)));
    }
    let new = match runner.prepare_batch(req, &branch).await {
        Ok(new) => new,
        Err(f) => return Ok(Err(f)),
    };
    if new.head != branch {
        return Ok(Err(INTERNAL));
    }
    match desk.create(&new).await {
        Ok(p) => Ok(Ok(pr_outcome(&p, &branch))),
        Err(ForgeError::Refused(_)) => Ok(Err(FORGE_REFUSED)),
        Err(e) => Err(e),
    }
}

/// What one request turn did.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Turn {
    /// Requests finished (done or failed).
    pub finished: usize,
    /// A request was left claimed to retry.
    pub retry: bool,
}

/// Claims and runs up to [`REQUESTS_PER_TURN`] requests on `repo` under
/// `epoch`. A purge is refused here: it runs only for a repository with no
/// task ([`purge`]). `LeaseLost` is returned so the task stops.
pub async fn run_turn<C: Control, D: PullDesk, R: RequestRunner>(
    control: &C,
    desk: &D,
    runner: &R,
    repo: &str,
    epoch: u64,
) -> ApiResult<Turn> {
    let mut turn = Turn::default();
    for _ in 0..REQUESTS_PER_TURN {
        let Some(req) = control.claim_request(repo, epoch).await? else {
            break;
        };
        let kind = MirrorRequestKind::try_from(req.kind).unwrap_or(MirrorRequestKind::Unspecified);
        let result = match kind {
            MirrorRequestKind::CutBatch => match cut(desk, runner, &req).await {
                Ok(r) => r,
                Err(e) => {
                    tracing::warn!(
                        repo, request = req.id, error = %e,
                        "mirror request: the forge failed; retried later"
                    );
                    turn.retry = true;
                    break;
                }
            },
            MirrorRequestKind::SettleBatch
            | MirrorRequestKind::Exclude
            | MirrorRequestKind::Rewind => runner.run(&req).await,
            // An enabled mirror is not purged (the server refuses it).
            MirrorRequestKind::Purge => Err(BAD_REQUEST),
            MirrorRequestKind::Unspecified => Err(BAD_REQUEST),
        };
        finish(control, repo, epoch, &req, result).await?;
        turn.finished += 1;
    }
    Ok(turn)
}

async fn finish<C: Control>(
    control: &C,
    repo: &str,
    epoch: u64,
    req: &pb::MirrorRequest,
    result: Result<pb::MirrorRequestOutcome, RequestFailure>,
) -> ApiResult<()> {
    match result {
        Ok(outcome) => {
            control
                .complete_request(repo, req.id, epoch, outcome, true)
                .await
        }
        Err(RequestFailure(code)) => {
            tracing::warn!(repo, request = req.id, code, "mirror request failed");
            control.fail_request(repo, req.id, epoch, code).await
        }
    }
}

/// Runs the requests of a repository the service has no task for (its row
/// is disabled or removed), under a lease the caller holds: a purge deletes
/// `dir`, any other kind fails with [`MIRROR_DISABLED`]. Returns whether
/// `dir` was purged.
pub async fn purge<C: Control>(control: &C, repo: &str, epoch: u64, dir: &Path) -> ApiResult<bool> {
    let mut purged = false;
    for _ in 0..REQUESTS_PER_TURN {
        let Some(req) = control.claim_request(repo, epoch).await? else {
            break;
        };
        let kind = MirrorRequestKind::try_from(req.kind).unwrap_or(MirrorRequestKind::Unspecified);
        let result = if kind == MirrorRequestKind::Purge {
            match std::fs::remove_dir_all(dir) {
                Ok(()) => {
                    purged = true;
                    Ok(pb::MirrorRequestOutcome::default())
                }
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                    purged = true;
                    Ok(pb::MirrorRequestOutcome::default())
                }
                Err(e) => {
                    tracing::error!(repo, error = %e, "mirror purge could not delete its data");
                    Err(INTERNAL)
                }
            }
        } else {
            Err(MIRROR_DISABLED)
        };
        finish(control, repo, epoch, &req, result).await?;
    }
    Ok(purged)
}

/// Whether `e` means the lease moved on.
pub fn is_lease_lost(e: &ApiError) -> bool {
    matches!(e, ApiError::LeaseLost { .. })
}
