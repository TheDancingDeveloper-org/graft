//! The batch-cut request as the service runs it (D41 §3.5): today's
//! [`Batcher`] over the task's `mirror.git` and `mirror.sqlite`, its batch
//! numbered from the next free batch number, as `pr-batch` numbers it. The
//! PR is opened by the request loop's desk, which adopts one an earlier
//! attempt opened; the batch keeps `pr-batch`'s semantics, one commit per land and
//! the D26 squash fallback.

use std::sync::Arc;

use graft_model::{ChangesetId, ObjectId};
use graft_proto::CallContext;
use graft_proto::api::{AdminApi, ObjectApi, RepoApi};
use graft_proto::pb;

use super::control::Control;
use super::requests::{BAD_REQUEST, INTERNAL, RequestFailure, RequestRunner, UNSUPPORTED};
use crate::batch::{BatchConfig, BatchOutcome, BatchRecord, BatchState, Batcher};
use crate::credentials::{CredentialPipe, CredentialProvider, SecretToken};
use crate::db::MirrorDb;
use crate::follow::{FollowConfig, FollowLine};
use crate::git::Mirror;
use crate::github::{Forge, ForgeError, NewPull};
use crate::{ExportLog, MirrorError};

/// Code: the line holds nothing its base lacks.
pub const NOTHING_TO_BATCH: RequestFailure = RequestFailure("nothing_to_batch");
/// Code: the line's last batch is still open.
pub const BATCH_OPEN: RequestFailure = RequestFailure("batch_open");
/// Code: the line does not rebase onto its moved base.
pub const CONFLICT: RequestFailure = RequestFailure("conflict");
/// Code: follow stopped the base line.
pub const BASE_STOPPED: RequestFailure = RequestFailure("base_stopped");
/// Code: the batch's lands are not settled or not due yet.
pub const NOT_DUE: RequestFailure = RequestFailure("not_due");
/// Code: the repository has no upstream credential to push with.
pub const NO_CREDENTIAL: RequestFailure = RequestFailure("no_credential");
/// Code: the batch branch holds commits this batch did not push.
pub const BRANCH_TAKEN: RequestFailure = RequestFailure("branch_taken");

/// The GitHub `owner/repo` of an `https://github.com/<owner>/<repo>(.git)`
/// upstream; `None` for any other upstream (no forge, no batches).
pub fn github_repo(upstream: &str) -> Option<(String, String)> {
    let rest = upstream.strip_prefix("https://github.com/")?;
    let rest = rest.trim_end_matches('/');
    let rest = rest.strip_suffix(".git").unwrap_or(rest);
    let (owner, repo) = rest.split_once('/')?;
    let ok = |s: &str| {
        !s.is_empty()
            && s != "."
            && s != ".."
            && s.bytes()
                .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.'))
    };
    (ok(owner) && ok(repo)).then(|| (owner.to_owned(), repo.to_owned()))
}

/// The repository's upstream credential, read from the server under the
/// task's lease each time the forge needs it.
pub struct LeaseCredential<C> {
    control: Arc<C>,
    repo: String,
    upstream: String,
    epoch: u64,
}

impl<C> LeaseCredential<C> {
    /// `repo`'s credential for `upstream` under `epoch`.
    pub fn new(control: Arc<C>, repo: &str, upstream: &str, epoch: u64) -> Self {
        LeaseCredential {
            control,
            repo: repo.to_owned(),
            upstream: upstream.to_owned(),
            epoch,
        }
    }
}

impl<C: Control> CredentialProvider for LeaseCredential<C> {
    async fn token(&self) -> Result<SecretToken, ForgeError> {
        match self
            .control
            .credential(&self.repo, &self.upstream, self.epoch)
            .await
        {
            Ok(Some(t)) => Ok(t),
            Ok(None) => Err(ForgeError::Refused(
                "the repository has no upstream credential".to_owned(),
            )),
            Err(_) => Err(ForgeError::Transient(
                "the upstream credential could not be read".to_owned(),
            )),
        }
    }
}

/// Runs a repository's batch cuts with `forge`.
pub struct BatchRunner<'a, A, F> {
    api: &'a A,
    cx: CallContext,
    db: &'a MirrorDb,
    mirror: Mirror,
    forge: &'a F,
    base: FollowLine,
    follow: FollowConfig,
    epoch: Option<u64>,
    exports: Option<&'a dyn ExportLog>,
}

impl<'a, A, F> BatchRunner<'a, A, F> {
    /// Cuts batches of `follow.repo` against `base`, its followed line.
    pub fn new(
        api: &'a A,
        cx: CallContext,
        db: &'a MirrorDb,
        mirror: Mirror,
        forge: &'a F,
        base: FollowLine,
        follow: FollowConfig,
    ) -> Self {
        BatchRunner {
            api,
            cx,
            db,
            mirror,
            forge,
            base,
            follow,
            epoch: None,
            exports: None,
        }
    }

    /// Fences the base follow by the task's lease `epoch`.
    pub fn with_lease_epoch(mut self, epoch: u64) -> Self {
        self.epoch = Some(epoch);
        self
    }

    /// Records each batch commit as its land's export in `log` (D40).
    pub fn with_export_log(mut self, log: &'a dyn ExportLog) -> Self {
        self.exports = Some(log);
        self
    }
}

fn change_id(b: &[u8]) -> Option<ChangesetId> {
    <[u8; 32]>::try_from(b)
        .ok()
        .map(|a| ChangesetId(ObjectId(a)))
}

fn new_pull(rec: &BatchRecord) -> NewPull {
    NewPull {
        title: rec.title.clone(),
        body: rec.body.clone(),
        head: rec.branch.clone(),
        base: rec.base_branch.clone(),
    }
}

/// A refused credential (none stored) is the request's fault; a transient
/// read failure is the service's.
fn credential_failure(e: &ForgeError) -> RequestFailure {
    match e {
        ForgeError::Refused(_) => NO_CREDENTIAL,
        _ => INTERNAL,
    }
}

fn failure(e: &MirrorError) -> RequestFailure {
    match e {
        MirrorError::Config(_) | MirrorError::Unmapped { .. } => BAD_REQUEST,
        _ => INTERNAL,
    }
}

impl<A, F> BatchRunner<'_, A, F>
where
    A: AdminApi + ObjectApi + RepoApi + Send + Sync,
    F: Forge,
{
    async fn cut(&self, req: &pb::MirrorRequest) -> Result<NewPull, RequestFailure> {
        // A cut at a chosen base changeset is not supported: the batch is
        // cut against the base line's tip, as `pr-batch` cuts it.
        if req.base.is_some() {
            return Err(UNSUPPORTED);
        }
        let exclude = req
            .excludes
            .iter()
            .map(|b| change_id(b))
            .collect::<Option<Vec<_>>>()
            .ok_or(BAD_REQUEST)?;
        let mut config = BatchConfig::new(req.line.clone(), self.base.clone(), self.follow.clone());
        config.exclude = exclude;
        let mut mirror = self.mirror.clone();
        let pipe = if mirror.is_https() {
            let token = self.forge.token().await.map_err(|e| {
                tracing::warn!(
                    repo = %req.repo, request = req.id, error = %e,
                    "mirror request: the upstream credential could not be read"
                );
                credential_failure(&e)
            })?;
            Some(Arc::new(
                CredentialPipe::open(token).await.map_err(|e| failure(&e))?,
            ))
        } else {
            None
        };
        mirror.set_credential_pipe(pipe);
        let mut batcher = Batcher::new(
            self.api,
            self.cx.clone(),
            self.db,
            mirror,
            self.forge,
            config,
        )
        .map_err(|e| failure(&e))?
        .for_request(req.id);
        if let Some(log) = self.exports {
            batcher = batcher.with_export_log(log);
        }
        if let Some(epoch) = self.epoch {
            batcher = batcher.with_lease_epoch(epoch);
        }
        let run = batcher.run().await.map_err(|e| {
            tracing::warn!(
                repo = %req.repo, request = req.id, error = %e,
                "mirror request: the batch cut failed"
            );
            failure(&e)
        })?;
        let mine = self.db.batch_of_request(req.id).map_err(|_| INTERNAL)?;
        match run.outcome {
            BatchOutcome::Opened(rec) | BatchOutcome::Waiting(rec)
                if rec.state == BatchState::Pushed
                    && mine.as_ref().is_some_and(|m| m.number == rec.number) =>
            {
                Ok(new_pull(&rec))
            }
            BatchOutcome::Opened(rec) if rec.state == BatchState::Rejected => Err(BRANCH_TAKEN),
            BatchOutcome::Opened(_) => Err(INTERNAL),
            BatchOutcome::Waiting(_) => Err(BATCH_OPEN),
            BatchOutcome::NothingToExport => Err(NOTHING_TO_BATCH),
            BatchOutcome::Conflict { .. } => Err(CONFLICT),
            BatchOutcome::BaseStopped(_) => Err(BASE_STOPPED),
            BatchOutcome::NotDue { .. }
            | BatchOutcome::Deferred { .. }
            | BatchOutcome::Unsettled { .. } => Err(NOT_DUE),
        }
    }
}

impl<A, F> RequestRunner for BatchRunner<'_, A, F>
where
    A: AdminApi + ObjectApi + RepoApi + Send + Sync,
    F: Forge,
{
    async fn cut_branch(&self, req: &pb::MirrorRequest) -> Result<Option<String>, RequestFailure> {
        let rec = self.db.batch_of_request(req.id).map_err(|_| INTERNAL)?;
        Ok(rec.map(|r| r.branch))
    }

    async fn prepare_batch(&self, req: &pb::MirrorRequest) -> Result<NewPull, RequestFailure> {
        self.cut(req).await
    }

    async fn run(
        &self,
        _req: &pb::MirrorRequest,
    ) -> Result<pb::MirrorRequestOutcome, RequestFailure> {
        Err(UNSUPPORTED)
    }
}
