//! The real repository task: today's [`Follower::poll_once`] over
//! `<data>/<repo>/mirror.git` and `mirror.sqlite`, its ref moves fenced by
//! the lease epoch, its credential handed to git through a pipe for one
//! poll.

use std::sync::Arc;

use graft_git::map::MirrorMap;
use graft_proto::api::{AdminApi, ObjectApi, RepoApi};
use graft_proto::pb::MirrorErrorCode;
use graft_proto::{CallContext, Principal, PrincipalKind};

use super::batch_task::{BatchRunner, LeaseCredential, github_repo};
use super::status::{PollFailure, classify};
use super::task::{PollReport, PollStep, TaskCtx};
use super::{Control, DB_FILE, GIT_DIR, Spawner};
use crate::ExportLog;
use crate::credentials::{CredentialPipe, SecretToken};
use crate::db::MirrorDb;
use crate::follow::{Divergence, FollowConfig, FollowLine, Follower, LineOutcome, POLL_INTERVAL};
use crate::git::Mirror;
use crate::github::{GITHUB_API, HttpForge, RetryPolicy};

/// Changesets per `ImportChangesets` call: the bound on one import step.
pub const IMPORT_BATCH: usize = 256;

/// Starts follower tasks calling `api`.
pub struct FollowSpawner<A> {
    api: Arc<A>,
    cx: fn(&str) -> CallContext,
    exports: Option<Arc<dyn ExportLog>>,
}

impl<A> FollowSpawner<A> {
    /// Tasks call the graft API `api`.
    pub fn new(api: Arc<A>) -> Self {
        FollowSpawner {
            api,
            cx: task_cx,
            exports: None,
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

impl<A, C> Spawner<C> for FollowSpawner<A>
where
    A: AdminApi + ObjectApi + RepoApi + Send + Sync + 'static,
    C: Control,
{
    fn spawn(&self, task: TaskCtx<C>) {
        let api = self.api.clone();
        let cx = self.cx;
        let exports = self.exports.clone();
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
            // Batch cuts need a GitHub upstream (its PRs) and a followed base.
            let forge = match (github_repo(&task.row.upstream), &base) {
                (Some((owner, name)), Some(_)) if task.config.claim_requests => {
                    let credential = LeaseCredential::new(
                        task.control.clone(),
                        &task.row.repo,
                        &task.row.upstream,
                        task.epoch,
                    );
                    match HttpForge::new(
                        GITHUB_API,
                        &owner,
                        &name,
                        credential,
                        RetryPolicy::default(),
                    ) {
                        Ok(f) => Some(f),
                        Err(_) => return task.broken("the GitHub client could not be built"),
                    }
                }
                _ => None,
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
