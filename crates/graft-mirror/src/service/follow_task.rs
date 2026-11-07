//! The real repository task: today's [`Follower::poll_once`] over
//! `<data>/<repo>/mirror.git` and `mirror.sqlite`, its ref moves fenced by
//! the lease epoch, its credential handed to git through a pipe for one
//! poll.

use std::sync::Arc;

use graft_git::map::MirrorMap;
use graft_proto::api::{AdminApi, ObjectApi, RepoApi};
use graft_proto::pb::MirrorErrorCode;
use graft_proto::{CallContext, Principal, PrincipalKind};

use super::batch_task::{BatchRunner, LeaseCredential, forge_repo, github_repo};
use super::status::{PollFailure, classify};
use super::task::{PollReport, PollStep, TaskCtx};
use super::{Control, DB_FILE, GIT_DIR, Spawner};
use crate::ExportLog;
use crate::MirrorError;
use crate::credentials::{CredentialPipe, CredentialProvider, SecretToken};
use crate::db::MirrorDb;
use crate::follow::{Divergence, FollowConfig, FollowLine, Follower, LineOutcome, POLL_INTERVAL};
use crate::forgejo::ForgejoForge;
use crate::git::Mirror;
use crate::github::{
    CheckRun, Forge, ForgeError, GITHUB_API, HttpForge, NewPull, Pull, RetryPolicy,
};

/// Changesets per `ImportChangesets` call: the bound on one import step.
pub const IMPORT_BATCH: usize = 256;

/// Builds the forge a task's batch cuts use, from the row's upstream.
pub trait Forges: Send + Sync + 'static {
    /// The forge of a task under control `C`.
    type Forge<C: Control>: Forge + 'static;
    /// The forge for `upstream`, reading its token through `credential`;
    /// `None` when the upstream has no forge (no batches).
    fn forge<C: Control>(
        &self,
        upstream: &str,
        credential: LeaseCredential<C>,
    ) -> Option<Result<Self::Forge<C>, MirrorError>>;
}

/// GitHub's API, for an `https://github.com/<owner>/<repo>` upstream.
pub struct GitHubForges;

impl Forges for GitHubForges {
    type Forge<C: Control> = HttpForge<LeaseCredential<C>>;

    fn forge<C: Control>(
        &self,
        upstream: &str,
        credential: LeaseCredential<C>,
    ) -> Option<Result<Self::Forge<C>, MirrorError>> {
        let (owner, name) = github_repo(upstream)?;
        Some(HttpForge::new(
            GITHUB_API,
            &owner,
            &name,
            credential,
            RetryPolicy::default(),
        ))
    }
}

/// A Forgejo host `serve` opens batch PRs on (`[[forgejo]]`, WI-3492).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ForgejoHost {
    /// The host, with its port if any, as the upstream URLs name it.
    pub host: String,
    /// The API root, ending in `/api/v1`.
    pub api: String,
}

/// The forges of `serve`: GitHub's API for an `https://github.com/` upstream,
/// Forgejo's for an upstream on one of `forgejo`'s hosts, and none (no
/// batches) for any other. Either reads its token through the row's
/// credential, the one git fetches and pushes with.
#[derive(Clone, Debug, Default)]
pub struct ServeForges {
    forgejo: Vec<ForgejoHost>,
    retry: RetryPolicy,
}

impl ServeForges {
    /// GitHub, and Forgejo on `forgejo`'s hosts.
    pub fn new(forgejo: Vec<ForgejoHost>) -> Self {
        ServeForges {
            forgejo,
            retry: RetryPolicy::default(),
        }
    }

    /// Forge calls retry by `retry`.
    pub fn with_retry(mut self, retry: RetryPolicy) -> Self {
        self.retry = retry;
        self
    }

    /// The Forgejo host of `upstream` and its `owner/repo`, if it is on one.
    pub fn forgejo_for(&self, upstream: &str) -> Option<(&ForgejoHost, String, String)> {
        self.forgejo
            .iter()
            .find_map(|h| forge_repo(upstream, &h.host).map(|(owner, repo)| (h, owner, repo)))
    }
}

impl Forges for ServeForges {
    type Forge<C: Control> = AnyForge<LeaseCredential<C>>;

    fn forge<C: Control>(
        &self,
        upstream: &str,
        credential: LeaseCredential<C>,
    ) -> Option<Result<Self::Forge<C>, MirrorError>> {
        if let Some((owner, name)) = github_repo(upstream) {
            return Some(
                HttpForge::new(GITHUB_API, &owner, &name, credential, self.retry)
                    .map(AnyForge::GitHub),
            );
        }
        let (host, owner, name) = self.forgejo_for(upstream)?;
        Some(
            ForgejoForge::new(&host.api, &owner, &name, credential, self.retry)
                .map(AnyForge::Forgejo),
        )
    }
}

/// GitHub's forge or Forgejo's, chosen per repository by its upstream.
pub enum AnyForge<C> {
    /// GitHub's REST and GraphQL API.
    GitHub(HttpForge<C>),
    /// Forgejo's REST API.
    Forgejo(ForgejoForge<C>),
}

impl<C: CredentialProvider> Forge for AnyForge<C> {
    async fn find_pull(&self, branch: &str) -> Result<Option<Pull>, ForgeError> {
        match self {
            AnyForge::GitHub(f) => f.find_pull(branch).await,
            AnyForge::Forgejo(f) => f.find_pull(branch).await,
        }
    }

    async fn create_pull(&self, new: &NewPull) -> Result<Pull, ForgeError> {
        match self {
            AnyForge::GitHub(f) => f.create_pull(new).await,
            AnyForge::Forgejo(f) => f.create_pull(new).await,
        }
    }

    async fn get_pull(&self, number: u64) -> Result<Pull, ForgeError> {
        match self {
            AnyForge::GitHub(f) => f.get_pull(number).await,
            AnyForge::Forgejo(f) => f.get_pull(number).await,
        }
    }

    async fn update_pull(&self, number: u64, body: &str) -> Result<(), ForgeError> {
        match self {
            AnyForge::GitHub(f) => f.update_pull(number, body).await,
            AnyForge::Forgejo(f) => f.update_pull(number, body).await,
        }
    }

    async fn enqueue(&self, node_id: &str) -> Result<(), ForgeError> {
        match self {
            AnyForge::GitHub(f) => f.enqueue(node_id).await,
            AnyForge::Forgejo(f) => f.enqueue(node_id).await,
        }
    }

    async fn check_runs(&self, sha: &str) -> Result<Vec<CheckRun>, ForgeError> {
        match self {
            AnyForge::GitHub(f) => f.check_runs(sha).await,
            AnyForge::Forgejo(f) => f.check_runs(sha).await,
        }
    }

    async fn token(&self) -> Result<SecretToken, ForgeError> {
        match self {
            AnyForge::GitHub(f) => f.token().await,
            AnyForge::Forgejo(f) => f.token().await,
        }
    }

    async fn merging(&self, node_id: &str) -> Result<bool, ForgeError> {
        match self {
            AnyForge::GitHub(f) => f.merging(node_id).await,
            AnyForge::Forgejo(f) => f.merging(node_id).await,
        }
    }

    async fn close_pr(&self, number: u64, comment: &str) -> Result<(), ForgeError> {
        match self {
            AnyForge::GitHub(f) => f.close_pr(number, comment).await,
            AnyForge::Forgejo(f) => f.close_pr(number, comment).await,
        }
    }
}

/// Starts follower tasks calling `api`.
pub struct FollowSpawner<A, G = GitHubForges> {
    api: Arc<A>,
    cx: fn(&str) -> CallContext,
    exports: Option<Arc<dyn ExportLog>>,
    forges: Arc<G>,
}

impl<A> FollowSpawner<A> {
    /// Tasks call the graft API `api`.
    pub fn new(api: Arc<A>) -> Self {
        FollowSpawner {
            api,
            cx: task_cx,
            exports: None,
            forges: Arc::new(GitHubForges),
        }
    }
}

impl<A, G> FollowSpawner<A, G> {
    /// Batch cuts use the forges `forges` builds instead of GitHub's.
    pub fn with_forges<H>(self, forges: H) -> FollowSpawner<A, H> {
        FollowSpawner {
            api: self.api,
            cx: self.cx,
            exports: self.exports,
            forges: Arc::new(forges),
        }
    }

    /// Tasks call as `cx(repo)`. A server reads the principal from the
    /// token; this matters only for an in-process API.
    pub fn with_cx(mut self, cx: fn(&str) -> CallContext) -> Self {
        self.cx = cx;
        self
    }

    /// Batch cuts record each batch commit's export in `log` (D40).
    pub fn with_export_log(mut self, log: Arc<dyn ExportLog>) -> Self {
        self.exports = Some(log);
        self
    }
}

fn task_cx(repo: &str) -> CallContext {
    CallContext {
        principal: Principal {
            subject: "graft-mirror".to_owned(),
            kind: PrincipalKind::Mirror,
            session: None,
            work_item: None,
            scopes: vec![format!("repo:{repo}:mirror")],
        },
        request_id: "mirror".to_owned(),
        deadline_ms: None,
    }
}

/// The followed lines of a row (`follow` or `both`).
pub fn follow_lines(row: &graft_proto::pb::Mirror) -> Vec<FollowLine> {
    row.lines
        .iter()
        .filter(|l| l.direction == "follow" || l.direction == "both")
        .map(|l| FollowLine {
            branch: l.branch.clone(),
            line: l.line.clone(),
        })
        .collect()
}

impl<A, G, C> Spawner<C> for FollowSpawner<A, G>
where
    A: AdminApi + ObjectApi + RepoApi + Send + Sync + 'static,
    G: Forges,
    C: Control,
{
    fn spawn(&self, task: TaskCtx<C>) {
        let api = self.api.clone();
        let cx = self.cx;
        let exports = self.exports.clone();
        let forges = self.forges.clone();
        tokio::spawn(async move {
            let db = match MirrorDb::open(&task.dir.join(DB_FILE)) {
                Ok(db) => db,
                Err(_) => return task.broken("the mirror database could not be opened"),
            };
            let lines = follow_lines(&task.row);
            let first = lines.first().map(|l| l.line.clone());
            let base = lines.first().cloned();
            let config = FollowConfig {
                repo: task.row.repo.clone(),
                lines,
                allow_lossy: Vec::new(),
                batch: IMPORT_BATCH,
                interval: POLL_INTERVAL,
                tags: None,
            };
            let mirror = Mirror::new(task.dir.join(GIT_DIR), task.row.upstream.clone());
            // Batch cuts need a forge (its PRs) and a followed base.
            let forge = if task.config.claim_requests && base.is_some() {
                let credential = LeaseCredential::new(
                    task.control.clone(),
                    &task.row.repo,
                    &task.row.upstream,
                    task.epoch,
                );
                match forges.forge(&task.row.upstream, credential) {
                    Some(Ok(f)) => Some(f),
                    Some(Err(_)) => return task.broken("the forge client could not be built"),
                    None => None,
                }
            } else {
                None
            };
            let batch_follow = config.clone();
            let batch_mirror = mirror.clone();
            let mut follower = Follower::new(&*api, cx(&task.row.repo), &db, mirror, config)
                .with_lease_epoch(task.epoch);
            if let Some(m) = &task.metrics {
                follower = follower.with_metrics(m.base().clone());
            }
            let mut step = FollowStep {
                follower,
                db: &db,
                first,
            };
            match (&forge, base) {
                (Some(forge), Some(base)) => {
                    let runner = BatchRunner::new(
                        &*api,
                        cx(&task.row.repo),
                        &db,
                        batch_mirror,
                        forge,
                        base,
                        batch_follow,
                    )
                    .with_lease_epoch(task.epoch);
                    let runner = match &exports {
                        Some(log) => runner.with_export_log(&**log),
                        None => runner,
                    };
                    task.run_with(&mut step, forge, &runner).await;
                }
                _ => task.run(&mut step).await,
            }
        });
    }
}

/// One follower poll.
pub struct FollowStep<'a, A> {
    follower: Follower<'a, A>,
    db: &'a MirrorDb,
    first: Option<String>,
}

impl<'a, A> FollowStep<'a, A> {
    /// A step over `follower`, reporting `first`'s progress from `db`.
    pub fn new(follower: Follower<'a, A>, db: &'a MirrorDb, first: Option<String>) -> Self {
        FollowStep {
            follower,
            db,
            first,
        }
    }
}

fn stopped(reason: Divergence) -> PollFailure {
    match reason {
        Divergence::NonFastForward => PollFailure::new(
            MirrorErrorCode::ForcePushed,
            "upstream was rewritten; the line is stopped",
        ),
        r => PollFailure::new(
            MirrorErrorCode::Diverged,
            format!("the line is stopped: {}", r.as_str()),
        ),
    }
}

impl<A> PollStep for FollowStep<'_, A>
where
    A: AdminApi + ObjectApi + RepoApi + Send + Sync,
{
    fn pin(&mut self, host: &str, port: u16, addr: Option<std::net::IpAddr>) {
        self.follower.set_resolve_pin(host, port, addr);
    }

    async fn poll(&mut self, credential: Option<SecretToken>) -> Result<PollReport, PollFailure> {
        let pipe = match credential {
            Some(c) => Some(Arc::new(
                CredentialPipe::open(c).await.map_err(|e| classify(&e))?,
            )),
            None => None,
        };
        self.follower.set_credential_pipe(pipe.clone());
        let polled = self.follower.poll_once().await;
        self.follower.set_credential_pipe(None);
        drop(pipe);
        let outcomes = polled.map_err(|e| classify(&e))?;
        let mut report = PollReport {
            requests: 1,
            ..PollReport::default()
        };
        for (_, outcome) in &outcomes {
            match outcome {
                LineOutcome::Imported { .. } => report.requests += 1,
                LineOutcome::Diverged { reason, .. } => {
                    report.stopped.get_or_insert_with(|| stopped(*reason));
                }
                LineOutcome::Failed(_) => {
                    report.stopped.get_or_insert_with(|| {
                        PollFailure::new(MirrorErrorCode::Internal, "a line's poll failed")
                    });
                }
                LineOutcome::MissingUpstream => {
                    report.stopped.get_or_insert_with(|| {
                        PollFailure::new(
                            MirrorErrorCode::NotFound,
                            "a followed branch is missing upstream",
                        )
                    });
                }
                LineOutcome::UpToDate => {}
            }
        }
        if let Some(line) = &self.first
            && let Ok(state) = self.db.line(line)
            && let Some(sha) = state.upstream
        {
            report.imported_sha = Some(sha.to_hex());
            report.changeset = self.db.changeset_of(sha).ok().flatten();
        }
        Ok(report)
    }
}
