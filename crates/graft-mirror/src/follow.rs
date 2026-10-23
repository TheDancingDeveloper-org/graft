//! Follow, git-primary mode A (`docs/design.md` §11.3): upstream (GitHub)
//! is authoritative, and graft's lines track it.
//!
//! Each poll runs one `git ls-remote` for every followed branch. A branch
//! whose upstream commit is the one the mirror DB says graft already has
//! costs nothing more. For a branch that moved:
//!
//! 1. `git fetch` it into the local bare mirror (under
//!    [`crate::git::UPSTREAM_NS`]).
//! 2. Resolve the graft line and map its tip to a git sha through the mirror
//!    DB. A tip the mirror did not put there is divergence.
//! 3. Check that the upstream commit descends from that sha. If not
//!    (a force-push), the line **stops**: `graft_mirror_divergence` goes to
//!    1, an error is logged, the mirror DB records the upstream commit, and
//!    graft is not touched. A later poll that finds upstream back on a
//!    fast-forward of graft resumes the line.
//! 4. Import the new commits with `graft-git` (the mirror DB is the mirror
//!    map, so nothing already imported is sent again), then move the line by
//!    admin `UpdateRef` CAS from the tip read in step 2.
//!
//! Lag: `graft_mirror_lag_changesets` is the number of upstream commits graft
//! lacks, and `graft_mirror_lag_seconds` is how long the line has been
//! behind (since follow first saw upstream ahead), 0 when caught up.

use std::future::Future;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use graft_git::GitSha;
use graft_git::import::{ImportConfig, import};
use graft_git::map::MirrorMap;
use graft_model::ChangesetId;
use graft_proto::api::{AdminApi, ObjectApi, RepoApi};
use graft_proto::types::RefKind;
use graft_proto::{ApiError, CallContext};
use graft_telemetry::Metrics;

use crate::db::{LineState, MirrorDb};
use crate::git::Mirror;
use crate::{MirrorError, current_ref, move_ref};

/// The poll interval of phase 1 (decision D9).
pub const POLL_INTERVAL: Duration = Duration::from_secs(15);

/// One upstream branch followed into one graft line.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FollowLine {
    /// The upstream branch (`main` for `refs/heads/main`).
    pub branch: String,
    /// The graft line.
    pub line: String,
}

/// What to follow.
#[derive(Clone, Debug)]
pub struct FollowConfig {
    /// The graft repository.
    pub repo: String,
    /// The followed branches.
    pub lines: Vec<FollowLine>,
    /// Commits allowed to fail the fidelity check.
    pub allow_lossy: Vec<GitSha>,
    /// Changesets per `ImportChangesets` batch.
    pub batch: usize,
    /// Time between polls.
    pub interval: Duration,
}

/// Why a line stopped.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Divergence {
    /// Upstream does not descend from the commit graft's line holds.
    NonFastForward,
    /// graft's line names a changeset the mirror did not import.
    GraftMoved,
    /// Upstream descends from graft's tip, but not along the first parent,
    /// so graft refuses the line move.
    NotFirstParent,
    /// Still stopped on the upstream commit an earlier poll refused.
    Held,
}

impl Divergence {
    /// A stable name for logs.
    pub fn as_str(self) -> &'static str {
        match self {
            Divergence::NonFastForward => "non-fast-forward",
            Divergence::GraftMoved => "graft-moved",
            Divergence::NotFirstParent => "not-first-parent",
            Divergence::Held => "held",
        }
    }
}

/// What one poll did for one line.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum LineOutcome {
    /// graft already holds the upstream commit.
    UpToDate,
    /// New commits imported and the line moved.
    Imported {
        /// Commits imported by this poll.
        commits: u64,
        /// The line's tip before (`None`: the line was created).
        from: Option<ChangesetId>,
        /// Its tip now.
        to: ChangesetId,
    },
    /// The line is stopped; graft was not changed.
    Diverged {
        /// Why.
        reason: Divergence,
        /// The upstream commit.
        upstream: GitSha,
    },
    /// Upstream has no such branch; nothing was done.
    MissingUpstream,
    /// The poll failed for this line (described); the next poll retries.
    Failed(String),
}

/// Follows one upstream into one graft repository.
pub struct Follower<'a, A> {
    api: &'a A,
    cx: CallContext,
    db: &'a MirrorDb,
    mirror: Mirror,
    config: FollowConfig,
    metrics: Option<Metrics>,
    clock: fn() -> i64,
    polls: u64,
}

fn system_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_millis() as i64)
}

impl<'a, A> Follower<'a, A>
where
    A: AdminApi + ObjectApi + RepoApi,
{
    /// A follower calling `api` as `cx` (each call gets its own request id).
    pub fn new(
        api: &'a A,
        cx: CallContext,
        db: &'a MirrorDb,
        mirror: Mirror,
        config: FollowConfig,
    ) -> Self {
        Follower {
            api,
            cx,
            db,
            mirror,
            config,
            metrics: None,
            clock: system_ms,
            polls: 0,
        }
    }

    /// Exports lag and divergence to `metrics`.
    pub fn with_metrics(mut self, metrics: Metrics) -> Self {
        self.metrics = Some(metrics);
        self
    }

    /// Reads time from `clock` (milliseconds since the epoch).
    pub fn with_clock(mut self, clock: fn() -> i64) -> Self {
        self.clock = clock;
        self
    }

    /// Polls until `shutdown` completes, every [`FollowConfig::interval`].
    /// A failed poll is logged and retried at the next interval.
    pub async fn run(&mut self, shutdown: impl Future<Output = ()>) {
        let mut tick = tokio::time::interval(self.config.interval);
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        let mut shutdown = std::pin::pin!(shutdown);
        loop {
            tokio::select! {
                () = &mut shutdown => return,
                _ = tick.tick() => {}
            }
            if let Err(e) = self.poll_once().await {
                tracing::warn!(repo = %self.config.repo, error = %e, "mirror poll failed");
            }
        }
    }

    /// One poll of every line. Fails only when upstream cannot be listed;
    /// per-line failures are reported in the outcomes.
    pub async fn poll_once(&mut self) -> Result<Vec<(String, LineOutcome)>, MirrorError> {
        self.polls += 1;
        for l in &self.config.lines {
            let holder = self
                .db
                .claim_authority(&l.line, crate::authority::GIT_PRIMARY)?;
            if holder != crate::authority::GIT_PRIMARY {
                return Err(MirrorError::Config(format!(
                    "line {} is claimed by {holder}; follow cannot also write it",
                    l.line
                )));
            }
        }
        self.mirror.ensure().await?;
        let branches: Vec<&str> = self
            .config
            .lines
            .iter()
            .map(|l| l.branch.as_str())
            .collect();
        let heads = match self.mirror.ls_remote(&branches).await {
            Ok(heads) => heads,
            Err(e) => {
                // Lag keeps counting while upstream is unreachable.
                for l in &self.config.lines {
                    if let Ok(state) = self.db.line(&l.line) {
                        self.export_state(&l.line, &state);
                    }
                }
                return Err(e.into());
            }
        };
        let mut out = Vec::with_capacity(heads.len());
        for (l, (_, upstream)) in self.config.lines.iter().zip(heads) {
            let outcome = match self.poll_line(l, upstream).await {
                Ok(o) => o,
                Err(e) => {
                    tracing::warn!(
                        repo = %self.config.repo, line = %l.line, error = %e,
                        "mirror line poll failed"
                    );
                    LineOutcome::Failed(e.to_string())
                }
            };
            if let Ok(state) = self.db.line(&l.line) {
                self.export_state(&l.line, &state);
            }
            out.push((l.line.clone(), outcome));
        }
        Ok(out)
    }

    fn call(&self, what: &str) -> CallContext {
        let mut cx = self.cx.clone();
        cx.request_id = format!("mirror-{}-{}-{what}", (self.clock)(), self.polls);
        cx
    }

    async fn poll_line(
        &self,
        l: &FollowLine,
        upstream: Option<GitSha>,
    ) -> Result<LineOutcome, MirrorError> {
        let mut state = self.db.line(&l.line)?;
        let Some(up) = upstream else {
            return Ok(LineOutcome::MissingUpstream);
        };
        if state.upstream == Some(up) && state.diverged.is_none() {
            self.set_lag_changesets(&l.line, 0);
            return Ok(LineOutcome::UpToDate);
        }
        if state.diverged == Some(up) {
            // Still the same stray upstream commit: stay stopped, quietly.
            return Ok(LineOutcome::Diverged {
                reason: Divergence::Held,
                upstream: up,
            });
        }
        if state.behind_since_ms.is_none() {
            state.behind_since_ms = Some((self.clock)());
            self.db.set_line(&l.line, &state)?;
        }

        self.mirror.fetch(&[l.branch.as_str()]).await?;
        let repo = self.config.repo.clone();
        let cx = self.call("resolve");
        let tip = current_ref(self.api, &cx, &repo, &l.line).await?;
        let graft_sha = match tip {
            None => None,
            Some(id) => match self.db.sha_of(id)? {
                Some(sha) => Some(sha),
                None => return self.diverge(l, &mut state, up, Divergence::GraftMoved),
            },
        };
        if graft_sha == Some(up) {
            // Moved by a run that stopped before recording it.
            self.caught_up(l, &mut state, up)?;
            return Ok(LineOutcome::UpToDate);
        }
        if let Some(g) = graft_sha
            && !self.mirror.is_ancestor(g, up).await?
        {
            return self.diverge(l, &mut state, up, Divergence::NonFastForward);
        }
        let behind = self.mirror.count_between(graft_sha, up).await?;
        self.set_lag_changesets(&l.line, behind);

        let cx = self.call("import");
        let import_config = ImportConfig {
            git_dir: self.mirror.dir().to_owned(),
            repo: repo.clone(),
            lines: vec![(Mirror::upstream_ref(&l.branch), l.line.clone())],
            tags: false,
            allow_lossy: self.config.allow_lossy.clone(),
            batch: self.config.batch,
        };
        let report = import(self.api, &cx, self.db, &import_config).await?;
        let to = report
            .lines
            .first()
            .map(|(_, id)| *id)
            .ok_or_else(|| MirrorError::Config("import reported no line".to_owned()))?;
        let cx = self.call("update-ref");
        match move_ref(self.api, &cx, &repo, &l.line, tip, to, RefKind::Line).await {
            Ok(()) => {}
            Err(ApiError::Rejected { policy, .. }) if policy == "line-fast-forward" => {
                return self.diverge(l, &mut state, up, Divergence::NotFirstParent);
            }
            Err(e) => return Err(e.into()),
        }
        self.caught_up(l, &mut state, up)?;
        tracing::info!(
            repo = %repo, line = %l.line, upstream = %up, commits = report.commits,
            "mirror line moved"
        );
        Ok(LineOutcome::Imported {
            commits: report.commits,
            from: tip,
            to,
        })
    }

    fn caught_up(
        &self,
        l: &FollowLine,
        state: &mut LineState,
        up: GitSha,
    ) -> Result<(), MirrorError> {
        if state.diverged.is_some() {
            tracing::info!(
                repo = %self.config.repo, line = %l.line, upstream = %up,
                "mirror line resumed: upstream is a fast-forward again"
            );
        }
        *state = LineState {
            upstream: Some(up),
            behind_since_ms: None,
            diverged: None,
        };
        self.db.set_line(&l.line, state)?;
        self.set_lag_changesets(&l.line, 0);
        Ok(())
    }

    fn diverge(
        &self,
        l: &FollowLine,
        state: &mut LineState,
        up: GitSha,
        reason: Divergence,
    ) -> Result<LineOutcome, MirrorError> {
        state.diverged = Some(up);
        self.db.set_line(&l.line, state)?;
        tracing::error!(
            repo = %self.config.repo, line = %l.line, upstream = %up,
            reason = reason.as_str(),
            "mirror divergence: line stopped, graft unchanged"
        );
        Ok(LineOutcome::Diverged {
            reason,
            upstream: up,
        })
    }

    fn set_lag_changesets(&self, line: &str, n: u64) {
        if let Some(m) = &self.metrics {
            m.mirror_lag_changesets
                .with_label_values(&[line])
                .set(n as i64);
        }
    }

    fn export_state(&self, line: &str, state: &LineState) {
        let Some(m) = &self.metrics else { return };
        let lag_ms = state
            .behind_since_ms
            .map_or(0, |since| ((self.clock)() - since).max(0));
        m.mirror_lag_seconds
            .with_label_values(&[line])
            .set(lag_ms as f64 / 1000.0);
        m.mirror_divergence
            .with_label_values(&[line])
            .set(i64::from(state.diverged.is_some()));
    }
}
