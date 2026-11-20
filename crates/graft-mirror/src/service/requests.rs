//! Mirror requests (D41 §3.5) as the service runs them: claimed under the
//! repository's lease epoch, executed, then completed or failed with a
//! typed code. A batch takes the next free batch number, and the runner
//! remembers which request cut it, so a cut that crashed between opening its
//! PR and recording it adopts that PR on the next claim instead of opening a
//! second.

use std::future::Future;
use std::path::Path;

use graft_proto::pb::{self, MirrorRequestKind};
use graft_proto::{ApiError, error::ApiResult};

use super::control::Control;
use crate::github::{Forge, ForgeError, NewPull, Pull};

/// The most requests one task runs between two polls.
pub const REQUESTS_PER_TURN: usize = 4;

/// How many of a repository's newest cuts one turn looks at for a batch to
/// settle. A line holds at most one open batch, and it is the line's newest
/// cut.
pub const SETTLE_SCAN: u32 = 16;

/// Why a request failed: a typed code the server records.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RequestFailure(pub &'static str);

/// Code: this service does not run this kind of request yet.
pub const UNSUPPORTED: RequestFailure = RequestFailure("unsupported");
/// Code: the forge refused.
pub const FORGE_REFUSED: RequestFailure = RequestFailure("forge_refused");
/// Code: the forge refused the repository's stored credential (status 401,
/// or git's authentication failure). The server marks the credential's last
/// authentication failed (gh#357).
pub const AUTH_FAILED: RequestFailure = RequestFailure("auth_failed");
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
    /// The branch of the batch an earlier attempt at `req` cut, if any.
    fn cut_branch(
        &self,
        req: &pb::MirrorRequest,
    ) -> impl Future<Output = Result<Option<String>, RequestFailure>> + Send;
    /// Pushes the batch `req` asks for and returns the PR to open, its head
    /// the batch's branch. Called only when no PR is open on that branch;
    /// running it again for the same request must be safe.
    fn prepare_batch(
        &self,
        req: &pb::MirrorRequest,
    ) -> impl Future<Output = Result<NewPull, RequestFailure>> + Send;
    /// Runs a settle, exclude or rewind request.
    fn run(
        &self,
        req: &pb::MirrorRequest,
    ) -> impl Future<Output = Result<pb::MirrorRequestOutcome, RequestFailure>> + Send;
    /// Reads the PR of the batch cut `req` opened and reports whether it is
    /// over: merged (and followed back) or closed unmerged.
    fn batch_settled(
        &self,
        req: &pb::MirrorRequest,
    ) -> impl Future<Output = Result<bool, RequestFailure>> + Send;
}

/// A runner for the kinds not wired to the batcher yet: refuses each with
/// [`UNSUPPORTED`].
pub struct Unwired;

impl RequestRunner for Unwired {
    async fn cut_branch(&self, _req: &pb::MirrorRequest) -> Result<Option<String>, RequestFailure> {
        Ok(None)
    }

    async fn prepare_batch(&self, _req: &pb::MirrorRequest) -> Result<NewPull, RequestFailure> {
        Err(UNSUPPORTED)
    }

    async fn run(
        &self,
        _req: &pb::MirrorRequest,
    ) -> Result<pb::MirrorRequestOutcome, RequestFailure> {
        Err(UNSUPPORTED)
    }

    async fn batch_settled(&self, _req: &pb::MirrorRequest) -> Result<bool, RequestFailure> {
        Ok(false)
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
    let earlier = match runner.cut_branch(req).await {
        Ok(b) => b,
        Err(f) => return Ok(Err(f)),
    };
    let open = match &earlier {
        Some(branch) => match desk.find_open(branch).await {
            Ok(p) => p,
            Err(e) if e.unauthorized() => return Ok(Err(AUTH_FAILED)),
            Err(e) => return Err(e),
        },
        None => None,
    };
    if let (Some(branch), Some(p)) = (earlier, open) {
        tracing::info!(
            repo = %req.repo, request = req.id, pr = p.number,
            "mirror request: adopted the PR an earlier attempt opened"
        );
        return Ok(Ok(pr_outcome(&p, &branch)));
    }
    let new = match runner.prepare_batch(req).await {
        Ok(new) => new,
        Err(f) => return Ok(Err(f)),
    };
    match desk.create(&new).await {
        Ok(p) => Ok(Ok(pr_outcome(&p, &new.head))),
        Err(e) if e.unauthorized() => Ok(Err(AUTH_FAILED)),
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
    /// Open batches settled because their PR was merged or closed.
    pub settled: usize,
}

/// Settles each open or sealed batch on `repo` whose PR is over, so the
/// server frees its line for the next cut (§3.5). A batch the runner cannot
/// read yet is left for a later turn. `LeaseLost` is returned.
async fn settle_open<C: Control, R: RequestRunner>(
    control: &C,
    runner: &R,
    repo: &str,
    epoch: u64,
) -> ApiResult<usize> {
    let mut settled = 0;
    for b in control.list_batches(repo, SETTLE_SCAN).await? {
        let open = matches!(
            pb::BatchState::try_from(b.batch_state),
            Ok(pb::BatchState::Open | pb::BatchState::Sealed)
        );
        if !open {
            continue;
        }
        match runner.batch_settled(&b).await {
            Ok(true) => {}
            Ok(false) => continue,
            Err(RequestFailure(code)) => {
                tracing::warn!(
                    repo,
                    request = b.id,
                    code,
                    "mirror batch: its PR could not be read"
                );
                continue;
            }
        }
        match control.settle_batch(repo, b.id, epoch).await {
            Ok(()) => {
                tracing::info!(repo, request = b.id, line = %b.line, "mirror batch settled; the line is free");
                settled += 1;
            }
            Err(e) if is_lease_lost(&e) => return Err(e),
            // Cancelled or settled since it was listed.
            Err(e) => {
                tracing::warn!(repo, request = b.id, error = %e, "mirror batch: settling failed")
            }
        }
    }
    Ok(settled)
}

/// Settles the batches whose PRs are over, then claims and runs up to
/// [`REQUESTS_PER_TURN`] requests on `repo` under `epoch`. A purge is refused here: it runs only for a repository with no
/// task ([`purge`]). `LeaseLost` is returned so the task stops.
pub async fn run_turn<C: Control, D: PullDesk, R: RequestRunner>(
    control: &C,
    desk: &D,
    runner: &R,
    repo: &str,
    epoch: u64,
) -> ApiResult<Turn> {
    let mut turn = Turn {
        settled: settle_open(control, runner, repo, epoch).await?,
        ..Turn::default()
    };
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
        Err(AUTH_FAILED) => {
            tracing::warn!(
                repo,
                request = req.id,
                code = AUTH_FAILED.0,
                "mirror request failed: the forge refused the repository's stored mirror credential; reseal it"
            );
            control
                .fail_request(repo, req.id, epoch, AUTH_FAILED.0)
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
