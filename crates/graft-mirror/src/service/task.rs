//! One repository's task: poll, write status, run requests, wait, repeat,
//! until the service stops it (between polls, never inside one) or the
//! lease is lost.

use std::hash::BuildHasher;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use graft_model::ChangesetId;
use graft_proto::pb::{self, MirrorErrorCode};
use tokio::sync::{Notify, mpsc};

use super::control::Control;
use super::limits::Backoff;
use super::netguard;
use super::requests::{self, NoDesk, PullDesk, RequestRunner, Unwired};
use super::status::{PollFailure, StatusWriter};
use super::{ServeConfig, Shared};
use crate::credentials::SecretToken;

/// What one successful poll found.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct PollReport {
    /// The upstream commit graft's first followed line holds now.
    pub imported_sha: Option<String>,
    /// That line's changeset.
    pub changeset: Option<ChangesetId>,
    /// A line is stopped (diverged or force-pushed): reported, not retried
    /// faster or slower.
    pub stopped: Option<PollFailure>,
    /// Upstream requests the poll made (for the rate budget).
    pub requests: u32,
}

/// One poll of a repository: today's `poll_once`, handed the credential
/// for this poll only.
pub trait PollStep: Send {
    /// Polls once.
    fn poll(
        &mut self,
        credential: Option<SecretToken>,
    ) -> impl std::future::Future<Output = Result<PollReport, PollFailure>> + Send;
}

/// How the service steers a running task.
#[derive(Debug, Default)]
pub struct TaskCtl {
    stop: AtomicBool,
    lost: AtomicBool,
    wake: Notify,
}

impl TaskCtl {
    /// Asks the task to exit after its current step.
    pub fn stop(&self) {
        self.stop.store(true, Ordering::SeqCst);
        self.wake.notify_one();
    }

    /// The lease was lost: exit after the current step, write nothing more.
    pub fn lose(&self) {
        self.lost.store(true, Ordering::SeqCst);
        self.stop();
    }

    /// Whether the task should exit.
    pub fn stopping(&self) -> bool {
        self.stop.load(Ordering::SeqCst)
    }

    /// Whether the lease was lost.
    pub fn lost(&self) -> bool {
        self.lost.load(Ordering::SeqCst)
    }
}

/// Why a task exited.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ExitReason {
    /// The service stopped it.
    Stopped,
    /// Its lease was lost; it holds nothing.
    LeaseLost,
    /// It could not start (its directory or database).
    Broken,
}

/// A task's exit, sent to the service.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TaskExit {
    /// The repository.
    pub repo: String,
    /// The lease epoch it ran under.
    pub epoch: u64,
    /// Why.
    pub reason: ExitReason,
}

/// Everything a task needs; the service builds it, a
/// [`super::Spawner`] runs it.
pub struct TaskCtx<C> {
    /// The server.
    pub control: Arc<C>,
    /// Limits and clocks shared by every task.
    pub(crate) shared: Arc<Shared>,
    /// The service config.
    pub config: Arc<ServeConfig>,
    /// The row the task was started from.
    pub row: pb::Mirror,
    /// The lease epoch every write carries.
    pub epoch: u64,
    /// `<data>/<repo>`.
    pub dir: PathBuf,
    /// The service's handle.
    pub ctl: Arc<TaskCtl>,
    /// The service's metrics, when it exports them. A task writes its
    /// repository's series; the service drops them when the task stops.
    pub metrics: Option<super::ServiceMetrics>,
    pub(crate) exits: mpsc::UnboundedSender<TaskExit>,
}

impl<C: Control> TaskCtx<C> {
    /// The repository.
    pub fn repo(&self) -> &str {
        &self.row.repo
    }

    /// Reports that the task could not start.
    pub fn broken(self, why: &str) {
        tracing::error!(repo = %self.row.repo, why, "mirror task could not start");
        let _ = self.exits.send(TaskExit {
            repo: self.row.repo.clone(),
            epoch: self.epoch,
            reason: ExitReason::Broken,
        });
    }

    /// Runs `step` until stopped, with no forge for requests.
    pub async fn run<P: PollStep>(self, step: &mut P) {
        self.run_with(step, &NoDesk, &Unwired).await;
    }

    /// Runs `step` until stopped, running requests with `desk` and
    /// `runner` between polls when the config claims them.
    pub async fn run_with<P: PollStep, D: PullDesk, R: RequestRunner>(
        self,
        step: &mut P,
        desk: &D,
        runner: &R,
    ) {
        let reason = self.lp(step, desk, runner).await;
        tracing::info!(repo = %self.row.repo, ?reason, "mirror task stopped");
        let _ = self.exits.send(TaskExit {
            repo: self.row.repo.clone(),
            epoch: self.epoch,
            reason,
        });
    }

    fn interval(&self) -> Duration {
        Duration::from_secs(u64::from(self.row.interval_s)).max(self.config.min_interval)
    }

    async fn lp<P: PollStep, D: PullDesk, R: RequestRunner>(
        &self,
        step: &mut P,
        desk: &D,
        runner: &R,
    ) -> ExitReason {
        let (repo, upstream) = (self.row.repo.as_str(), self.row.upstream.as_str());
        let interval = self.interval();
        let mut failures = Backoff::new(interval, self.config.max_failure_backoff);
        let mut server = Backoff::new(Duration::from_secs(1), self.config.server_backoff_cap);
        let mut status = StatusWriter::default();
        let mut last_success: Option<i64> = None;
        let mut last = PollReport::default();
        let mut next = tokio::time::Instant::now();
        loop {
            if self.ctl.stopping() {
                break;
            }
            tokio::select! {
                () = tokio::time::sleep_until(next) => {}
                () = self.ctl.wake.notified() => {}
            }
            if self.ctl.stopping() {
                break;
            }
            let started = self.shared.now_ms();
            let (outcome, host_key) = self.poll(step).await;
            if self.ctl.lost() {
                break;
            }
            let now = self.shared.now_ms();
            // The next poll: the interval, stretched by the rate budget,
            // or a back-off.
            let mut wait = interval;
            if let Some(key) = &host_key {
                let stretch = self.shared.budget.lock().expect("budget").stretch(key, now);
                wait = wait.mul_f64(stretch);
            }
            let failure = match &outcome {
                Ok(report) => {
                    failures.reset();
                    server.reset();
                    last_success = Some(now);
                    last = report.clone();
                    report.stopped.clone()
                }
                Err(f) if f.code == MirrorErrorCode::LeaseLost => {
                    self.ctl.lose();
                    break;
                }
                Err(f) => {
                    let backoff = if f.server {
                        server.fail()
                    } else {
                        failures.fail()
                    };
                    wait = wait.max(backoff);
                    if let Some(after) = f.retry_after {
                        wait = wait.max(after);
                    }
                    Some(f.clone())
                }
            };
            let backoff_until = (wait > interval).then(|| now + wait.as_millis() as i64);
            next = tokio::time::Instant::now() + wait;
            if let Some(m) = &self.metrics {
                m.poll_finished(
                    repo,
                    last_success.map(|ms| ms as f64 / 1000.0),
                    failure.as_ref().map(|f| f.code),
                );
            }
            let st = pb::MirrorStatus {
                last_poll_at_ms: Some(started),
                last_success_at_ms: last_success,
                last_imported_sha: last.imported_sha.clone(),
                last_changeset: last.changeset.map(|c| c.0.0.to_vec()),
                lag_changesets: 0,
                lag_seconds: 0,
                error_code: failure
                    .as_ref()
                    .map_or(MirrorErrorCode::Unspecified, |f| f.code)
                    as i32,
                error_message: failure.as_ref().map(|f| f.message.clone()),
                backoff_until_ms: backoff_until,
                lease_epoch: self.epoch,
            };
            if status.should_write(&st, now) {
                match self.control.set_status(repo, upstream, st).await {
                    Ok(()) => {}
                    Err(e) if requests::is_lease_lost(&e) => {
                        self.ctl.lose();
                        break;
                    }
                    Err(e) => {
                        tracing::warn!(repo, error = ?e, "mirror status write failed");
                        status.unwritten();
                    }
                }
            }
            if self.config.claim_requests && !self.ctl.stopping() {
                match requests::run_turn(&*self.control, desk, runner, repo, self.epoch).await {
                    Ok(turn) => {
                        if let Some(m) = &self.metrics {
                            m.request_turn(
                                repo,
                                turn.retry || turn.finished >= requests::REQUESTS_PER_TURN,
                            );
                        }
                    }
                    Err(e) if requests::is_lease_lost(&e) => {
                        self.ctl.lose();
                        break;
                    }
                    Err(e) => tracing::warn!(repo, error = ?e, "mirror requests failed"),
                }
            }
        }
        if self.ctl.lost() {
            ExitReason::LeaseLost
        } else {
            ExitReason::Stopped
        }
    }

    /// One poll: the credential, the upstream check, the fetch slots, then
    /// `step` under the poll timeout. Returns the rate-budget key used.
    async fn poll<P: PollStep>(
        &self,
        step: &mut P,
    ) -> (Result<PollReport, PollFailure>, Option<(String, u64)>) {
        let (repo, upstream) = (self.row.repo.as_str(), self.row.upstream.as_str());
        let host = if self.config.allow_local_upstreams && netguard::target(upstream).is_err() {
            "local".to_owned()
        } else {
            match netguard::check(upstream, &self.shared.resolve).await {
                Ok(t) => t.host,
                Err(f) => return (Err(f), None),
            }
        };
        let credential = match self.control.credential(repo, upstream, self.epoch).await {
            Ok(c) => c,
            Err(e) => return (Err(PollFailure::server(&e)), None),
        };
        let fingerprint = credential
            .as_ref()
            .map_or(0, |c| self.shared.hasher.hash_one(c.expose()));
        let key = (host.clone(), fingerprint);
        let _permit = self.shared.slots.acquire(&host).await;
        // The credential moves into the step and is dropped with the poll.
        let out = match tokio::time::timeout(self.config.poll_timeout, step.poll(credential)).await
        {
            Ok(r) => r,
            Err(_) => Err(PollFailure::new(
                MirrorErrorCode::Timeout,
                "the poll did not finish in time",
            )),
        };
        let requests = out.as_ref().map_or(1, |r| r.requests.max(1));
        self.shared.budget.lock().expect("budget").record(
            key.clone(),
            self.shared.now_ms(),
            requests,
        );
        (out, Some(key))
    }
}
