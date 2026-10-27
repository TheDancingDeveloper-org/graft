//! Export, graft-primary mode B (`docs/design.md` §11.3): graft's line is
//! authoritative, and GitHub's branch is its exported mirror.
//!
//! Each poll, for each line:
//!
//! 1. Read the journal from the line's cursor, `RefMoved` entries only, a
//!    bounded page at a time. Moves of other refs, and moves at or below the
//!    ref version already exported, only advance the cursor.
//! 2. Export the newest move in the page with `graft-git`'s exporter. It
//!    writes every changeset upstream lacks, parents first, so each move's
//!    changesets reach git in line order. Each commit is recorded in the
//!    mirror DB before anything names it, so exporting it again always gives
//!    the same sha.
//! 3. Push with a **lease**: `--force-with-lease=<branch>:<last pushed sha>`.
//!    That is a compare-and-swap on GitHub's side, never a blind force.
//! 4. Record the cursor, the changeset, the sha and the ref version in one
//!    statement. A crash before that re-runs the page and pushes the same
//!    sha.
//!
//! A refused lease is read back. If upstream holds a commit this export
//! pushed and did not record (a crash between push and record), the push is
//! retried once on that lease. Anything else is **divergence**: someone wrote
//! to GitHub out of band. The line stops, `graft_mirror_divergence` goes to
//! 1, an error is logged, and nothing is forced. The cursor stays where it
//! was, so once an operator runs `reconcile` ([`crate::reconcile`]) export
//! resumes with no gap.
//!
//! The first poll of a line has no cursor. It finds the journal's end with
//! O(log n) one-entry reads (never a scan), then reads the line tip and
//! upstream. Upstream at the tip's commit, at an older commit of the line,
//! or absent is a clean start. Anything else is divergence.
//!
//! The committer date of a native changeset is the time export first wrote
//! it. graft's `Land` does not expose the land record's time to clients yet
//! (G3.1); the mirror DB keeps the record, so every later export is
//! byte-identical.

use std::future::Future;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use graft_git::GitSha;
use graft_git::export::Committer;
use graft_git::exporter::{Exporter, LandTimes};
use graft_git::map::MirrorMap;
use graft_model::{ChangesetId, Timestamp};
use graft_proto::api::{ObjectApi, RepoApi};
use graft_proto::types::{ListRefs, Page, Pair, ResolveRef, Watch};
use graft_proto::{ApiError, CallContext};
use graft_telemetry::Metrics;
use serde::Deserialize;

use crate::credentials::CredentialProvider;
use crate::db::{ExportState, MirrorDb};
use crate::follow::{FollowLine, POLL_INTERVAL};
use crate::git::{Mirror, PushOutcome};
use crate::{ExportLog, MirrorError, tag_ref};

/// The authority export claims for the lines it writes to GitHub.
pub const GRAFT_PRIMARY: &str = "graft-primary";

/// Journal entries per `Watch` page.
pub const PAGE: u32 = 256;

/// Pages one poll reads per line at most, so a poll is bounded; the next
/// poll goes on from the cursor.
pub const MAX_PAGES: usize = 64;

/// The journal kind export follows.
const REF_MOVED: &str = "RefMoved";

/// What to export.
#[derive(Clone, Debug)]
pub struct ExportConfig {
    /// The graft repository.
    pub repo: String,
    /// Each graft line and the upstream branch it is exported to.
    pub lines: Vec<FollowLine>,
    /// Time between polls.
    pub interval: Duration,
    /// The committer of native changesets.
    pub committer: Committer,
}

impl ExportConfig {
    /// Exports `lines` of `repo`, every [`POLL_INTERVAL`], as
    /// [`default_committer`].
    pub fn new(repo: impl Into<String>, lines: Vec<FollowLine>) -> Self {
        ExportConfig {
            repo: repo.into(),
            lines,
            interval: POLL_INTERVAL,
            committer: default_committer(),
        }
    }
}

/// `graft <graft@graft.invalid>`, the committer of native changesets.
pub fn default_committer() -> Committer {
    Committer {
        name: "graft".to_owned(),
        email: "graft@graft.invalid".to_owned(),
    }
}

/// What one poll did for one line.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ExportOutcome {
    /// Upstream already holds everything the line has.
    UpToDate,
    /// The line's new changesets were exported and upstream moved.
    Pushed {
        /// Upstream's commit before (`None`: the branch was created).
        from: Option<GitSha>,
        /// Upstream's commit now.
        to: GitSha,
        /// The changeset it is the export of.
        changeset: ChangesetId,
        /// Line moves this poll covered.
        moves: u64,
    },
    /// The lease was refused: the line stopped, and nothing was forced.
    Diverged {
        /// What the lease expected upstream to hold.
        expected: Option<GitSha>,
        /// What upstream holds.
        upstream: Option<GitSha>,
    },
    /// Still stopped by an earlier divergence, until `reconcile`.
    Held {
        /// What upstream held when it stopped.
        upstream: Option<GitSha>,
    },
    /// The poll failed for this line (described); the next poll retries.
    Failed(String),
}

/// The body of a `RefMoved` journal entry (graft-engine's durable format).
#[derive(Deserialize)]
struct RefMoved {
    name: String,
    new: String,
    version: u64,
}

/// Native changesets are dated when export first writes them; the mirror
/// map keeps the result.
pub(crate) struct WrittenAt(pub(crate) i64);

impl LandTimes for WrittenAt {
    fn landed_at(&self, _id: ChangesetId) -> Result<Timestamp, graft_git::GitError> {
        Ok(Timestamp {
            ms: self.0,
            offset_min: 0,
        })
    }
}

pub(crate) fn system_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_millis() as i64)
}

fn config_err(what: impl Into<String>) -> MirrorError {
    MirrorError::Config(what.into())
}

/// Exports graft-primary lines of one repository to one upstream.
pub struct Export<'a, A, C> {
    api: &'a A,
    cx: CallContext,
    db: &'a MirrorDb,
    mirror: Mirror,
    credentials: &'a C,
    config: ExportConfig,
    metrics: Option<Metrics>,
    clock: fn() -> i64,
    calls: u64,
    exports: Option<&'a dyn ExportLog>,
}

impl<'a, A, C> Export<'a, A, C>
where
    A: RepoApi + ObjectApi,
    C: CredentialProvider,
{
    /// An export of `config.lines`. Claims each line as graft-primary: a line
    /// another authority claims is refused, so two writers never both hold
    /// one line. `credentials` authenticate pushes to an HTTPS upstream.
    pub fn new(
        api: &'a A,
        cx: CallContext,
        db: &'a MirrorDb,
        mirror: Mirror,
        credentials: &'a C,
        config: ExportConfig,
    ) -> Result<Self, MirrorError> {
        for l in &config.lines {
            let holder = db.claim_authority(&l.line, GRAFT_PRIMARY)?;
            if holder != GRAFT_PRIMARY {
                return Err(config_err(format!(
                    "line {} is claimed by {holder}; export cannot also write it",
                    l.line
                )));
            }
        }
        Ok(Export {
            api,
            cx,
            db,
            mirror,
            credentials,
            config,
            metrics: None,
            clock: system_ms,
            calls: 0,
            exports: None,
        })
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

    /// Records each exported line tip's commit in `log`, before the push, so
    /// the post-land CI run the push starts can attest it.
    pub fn with_export_log(mut self, log: &'a dyn ExportLog) -> Self {
        self.exports = Some(log);
        self
    }

    /// Polls until `shutdown` completes, every [`ExportConfig::interval`].
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
                tracing::warn!(repo = %self.config.repo, error = %e, "mirror export poll failed");
            }
        }
    }

    /// One poll of every line. Per-line failures are reported in the
    /// outcomes; a line that fails does not hold up the others.
    pub async fn poll_once(&mut self) -> Result<Vec<(String, ExportOutcome)>, MirrorError> {
        self.mirror.ensure().await?;
        let lines = self.config.lines.clone();
        let mut out = Vec::with_capacity(lines.len());
        for l in &lines {
            let outcome = match self.poll_line(l).await {
                Ok(o) => o,
                Err(e) => {
                    tracing::warn!(
                        repo = %self.config.repo, line = %l.line, error = %e,
                        "mirror export of a line failed; retried on the next poll"
                    );
                    ExportOutcome::Failed(e.to_string())
                }
            };
            out.push((l.line.clone(), outcome));
        }
        Ok(out)
    }

    /// The time between polls.
    pub fn interval(&self) -> Duration {
        self.config.interval
    }

    /// Export's record of `line`, if it has started it.
    pub fn state(&self, line: &str) -> Result<Option<ExportState>, MirrorError> {
        Ok(self.db.export_state(line)?)
    }

    fn call(&mut self, what: &str) -> CallContext {
        self.calls += 1;
        let mut cx = self.cx.clone();
        cx.request_id = format!("export-{}-{}-{what}", (self.clock)(), self.calls);
        cx
    }

    async fn poll_line(&mut self, l: &FollowLine) -> Result<ExportOutcome, MirrorError> {
        // An authority switch can hand the line back to git-primary while
        // export runs; from then on export does not write it.
        if let Some(holder) = self.db.authority(&l.line)?
            && holder != GRAFT_PRIMARY
        {
            return Ok(ExportOutcome::Failed(format!(
                "line {} is claimed by {holder}; export does not write it",
                l.line
            )));
        }
        let (mut state, mut target) = match self.db.export_state(&l.line)? {
            Some(state) => (state, None),
            None => match self.bootstrap(l).await? {
                Ok(start) => start,
                Err(outcome) => return Ok(outcome),
            },
        };
        if state.diverged {
            self.set_divergence(&l.line, true);
            return Ok(ExportOutcome::Held {
                upstream: state.stray,
            });
        }
        let from = state.sha;
        let mut moves = 0;
        let mut pushed = None;
        let mut caught_up = false;
        for _ in 0..MAX_PAGES {
            let cx = self.call("watch");
            let page = self
                .api
                .watch(
                    &cx,
                    Watch {
                        repo: self.config.repo.clone(),
                        from_lsn: state.lsn,
                        kinds: vec![REF_MOVED.to_owned()],
                        limit: PAGE,
                        wait_ms: 0,
                    },
                )
                .await?;
            let floor = target.map_or(state.version, |(_, v)| v);
            for e in &page.entries {
                let moved: RefMoved = serde_json::from_str(&e.body).map_err(|_| {
                    config_err(format!("journal entry {} is not a RefMoved body", e.lsn))
                })?;
                if moved.name != l.line || moved.version <= floor.max(state.version) {
                    continue;
                }
                let id = ChangesetId::parse(&moved.new).map_err(|_| {
                    config_err(format!("journal entry {} names a malformed id", e.lsn))
                })?;
                target = Some((id, moved.version));
                moves += 1;
            }
            let advanced = page.next_lsn > state.lsn;
            let next_lsn = page.next_lsn.max(state.lsn);
            match target.take() {
                Some(t) => {
                    if let Some(outcome) = self.push(l, &mut state, t, next_lsn).await? {
                        return Ok(outcome);
                    }
                    pushed = Some(t.0);
                }
                None if advanced => {
                    state.lsn = next_lsn;
                    self.db.set_export_state(&l.line, &state)?;
                }
                None => {}
            }
            if !advanced {
                caught_up = true;
                break;
            }
        }
        self.set_divergence(&l.line, false);
        if caught_up {
            self.set_caught_up(&l.line);
        }
        Ok(match (pushed, state.sha) {
            (Some(changeset), Some(to)) if from != Some(to) => ExportOutcome::Pushed {
                from,
                to,
                changeset,
                moves,
            },
            _ => ExportOutcome::UpToDate,
        })
    }

    /// Writes `target` and its missing ancestors into the local mirror, and
    /// returns its git sha.
    async fn export(&mut self, target: ChangesetId) -> Result<GitSha, MirrorError> {
        let cx = self.call("export");
        let lands = WrittenAt((self.clock)());
        let repo = self.config.repo.clone();
        let mut exporter = Exporter::new(
            self.api,
            &cx,
            self.db,
            &repo,
            &self.config.committer,
            &lands,
            self.mirror.dir(),
        )?;
        Ok(exporter.export_changeset(target).await?)
    }

    async fn token(&self) -> Result<Option<crate::credentials::SecretToken>, MirrorError> {
        if self.mirror.is_https() {
            Ok(Some(self.credentials.token().await?))
        } else {
            Ok(None)
        }
    }

    /// Exports `target` (at ref version `version`) and pushes it on the
    /// lease, then records the cursor `next_lsn`. `Some` when the line
    /// diverged instead.
    async fn push(
        &mut self,
        l: &FollowLine,
        state: &mut ExportState,
        (target, version): (ChangesetId, u64),
        next_lsn: u64,
    ) -> Result<Option<ExportOutcome>, MirrorError> {
        let sha = self.export(target).await?;
        if state.sha != Some(sha) {
            if let Some(log) = self.exports {
                let cx = self.call("record-export");
                log.record_exports(&cx, &self.config.repo, &[(target, sha)])
                    .await?;
            }
            let token = self.token().await?;
            let bearer = token.as_ref().map(|t| t.expose());
            let outcome = self
                .mirror
                .push_lease(&l.branch, sha, state.sha, bearer)
                .await?;
            if outcome == PushOutcome::Rejected {
                let upstream = self.upstream(&l.branch).await?;
                let retry = match upstream {
                    Some(up) if up == sha => None,
                    Some(up) if self.own_push(state, up, target).await? => Some(up),
                    _ => return self.diverge(l, state, upstream).map(Some),
                };
                if let Some(lease) = retry {
                    let again = self
                        .mirror
                        .push_lease(&l.branch, sha, Some(lease), bearer)
                        .await?;
                    if again == PushOutcome::Rejected {
                        let upstream = self.upstream(&l.branch).await?;
                        if upstream != Some(sha) {
                            return self.diverge(l, state, upstream).map(Some);
                        }
                    }
                }
            }
            tracing::info!(
                repo = %self.config.repo, line = %l.line, upstream = %sha, changeset = %target,
                "mirror export pushed"
            );
        }
        state.lsn = next_lsn;
        state.changeset = Some(target);
        state.sha = Some(sha);
        state.version = state.version.max(version);
        self.db.set_export_state(&l.line, state)?;
        Ok(None)
    }

    async fn upstream(&self, branch: &str) -> Result<Option<GitSha>, MirrorError> {
        Ok(self
            .mirror
            .ls_remote(&[branch])
            .await?
            .pop()
            .and_then(|(_, s)| s))
    }

    /// Whether upstream's `up` is a push of this export that was not
    /// recorded: a commit of the line at or after the recorded lease and at
    /// or before `target`. Two ancestry reads on graft's index.
    async fn own_push(
        &mut self,
        state: &ExportState,
        up: GitSha,
        target: ChangesetId,
    ) -> Result<bool, MirrorError> {
        let Some(c) = self.db.changeset_of(up)? else {
            return Ok(false);
        };
        if let Some(prev) = state.changeset
            && !self.is_ancestor(prev, c).await?
        {
            return Ok(false);
        }
        self.is_ancestor(c, target).await
    }

    async fn is_ancestor(&mut self, a: ChangesetId, b: ChangesetId) -> Result<bool, MirrorError> {
        if a == b {
            return Ok(true);
        }
        let cx = self.call("ancestor");
        Ok(self
            .api
            .is_ancestor(
                &cx,
                Pair {
                    repo: self.config.repo.clone(),
                    a,
                    b,
                },
            )
            .await?)
    }

    fn diverge(
        &self,
        l: &FollowLine,
        state: &mut ExportState,
        upstream: Option<GitSha>,
    ) -> Result<ExportOutcome, MirrorError> {
        let expected = state.sha;
        state.diverged = true;
        state.stray = upstream;
        self.db.set_export_state(&l.line, state)?;
        self.set_divergence(&l.line, true);
        tracing::error!(
            repo = %self.config.repo, line = %l.line, branch = %l.branch,
            expected = ?expected, upstream = ?upstream,
            "mirror divergence: upstream was written out of band; export stopped, nothing forced"
        );
        Ok(ExportOutcome::Diverged { expected, upstream })
    }

    /// The first state of a line: the journal's end as the cursor, and the
    /// lease from upstream, checked against the line. `Ok(Err(_))` when the
    /// line cannot start (diverged, recorded). The second value is a move to
    /// export at once, when upstream is behind the tip.
    #[allow(clippy::type_complexity)]
    async fn bootstrap(
        &mut self,
        l: &FollowLine,
    ) -> Result<Result<(ExportState, Option<(ChangesetId, u64)>), ExportOutcome>, MirrorError> {
        // The end first: a move after it is read from the journal, and one
        // before it is in the tip read next.
        let lsn = self.journal_end().await?;
        let cx = self.call("resolve");
        let tip = match self
            .api
            .resolve_ref(
                &cx,
                ResolveRef {
                    repo: self.config.repo.clone(),
                    name: l.line.clone(),
                },
            )
            .await
        {
            Ok(view) => Some((view.target, view.version)),
            Err(ApiError::NotFound { .. }) => None,
            Err(e) => return Err(e.into()),
        };
        let upstream = self.upstream(&l.branch).await?;
        let mut state = ExportState {
            lsn,
            ..ExportState::default()
        };
        let start = match (tip, upstream) {
            (None, None) => None,
            (Some(t), None) => Some(t),
            (tip, Some(up)) => match (tip, self.db.changeset_of(up)?) {
                (Some((id, version)), Some(c)) if c == id => {
                    state.changeset = Some(c);
                    state.sha = Some(up);
                    state.version = version;
                    None
                }
                (Some((id, version)), Some(c)) if self.is_ancestor(c, id).await? => {
                    state.changeset = Some(c);
                    state.sha = Some(up);
                    Some((id, version))
                }
                _ => {
                    return Ok(Err(self.diverge(l, &mut state, Some(up))?));
                }
            },
        };
        self.db.set_export_state(&l.line, &state)?;
        tracing::info!(
            repo = %self.config.repo, line = %l.line, lsn, upstream = ?state.sha,
            "mirror export started"
        );
        Ok(Ok((state, start)))
    }

    /// The LSN after the journal's last entry ([`journal_end`]).
    async fn journal_end(&mut self) -> Result<u64, MirrorError> {
        let cx = self.call("journal-end");
        journal_end(self.api, &cx, &self.config.repo).await
    }

    fn set_divergence(&self, line: &str, on: bool) {
        if let Some(m) = &self.metrics {
            m.mirror_divergence
                .with_label_values(&[line])
                .set(i64::from(on));
        }
    }

    /// Export has read the journal to its end: no line move is waiting.
    fn set_caught_up(&self, line: &str) {
        if let Some(m) = &self.metrics {
            m.mirror_lag_changesets.with_label_values(&[line]).set(0);
            m.mirror_lag_seconds.with_label_values(&[line]).set(0.0);
        }
    }
}

/// The LSN after the journal's last entry, by doubling then bisecting on
/// one-entry reads: O(log n) calls, never a scan.
pub(crate) async fn journal_end<A: RepoApi>(
    api: &A,
    cx: &CallContext,
    repo: &str,
) -> Result<u64, MirrorError> {
    let has = |lsn: u64| async move {
        let page = api
            .watch(
                cx,
                Watch {
                    repo: repo.to_owned(),
                    from_lsn: lsn,
                    kinds: Vec::new(),
                    limit: 1,
                    wait_ms: 0,
                },
            )
            .await?;
        Ok::<_, MirrorError>(page.next_lsn > lsn)
    };
    if !has(1).await? {
        return Ok(1);
    }
    let (mut lo, mut hi) = (1u64, 2u64);
    while has(hi).await? {
        lo = hi;
        hi = hi
            .checked_mul(2)
            .ok_or_else(|| config_err("journal LSN overflow"))?;
    }
    while hi - lo > 1 {
        let mid = lo + (hi - lo) / 2;
        if has(mid).await? {
            lo = mid;
        } else {
            hi = mid;
        }
    }
    Ok(hi)
}

/// `graft-mirror export --to <bare repo>`: writes every line and tag of
/// `repo` into `dest` (`docs/design.md` §11.2, the exit path). Lines become
/// `refs/heads/<line>`, tags `refs/tags/<tag>`. Commits the mirror DB knows
/// are written as recorded, so the result matches what export pushed.
pub async fn export_all<A>(
    api: &A,
    cx: &CallContext,
    db: &MirrorDb,
    repo: &str,
    dest: &std::path::Path,
) -> Result<graft_git::exporter::ExportReport, MirrorError>
where
    A: RepoApi + ObjectApi,
{
    let mut refs = Vec::new();
    let mut token = None;
    loop {
        let page = api
            .list_refs(
                cx,
                ListRefs {
                    repo: repo.to_owned(),
                    prefix: None,
                    page: Page {
                        limit: 256,
                        token: token.clone(),
                    },
                },
            )
            .await?;
        let Some(last) = page.last() else { break };
        token = Some(last.name.clone());
        for r in page {
            let name = match r.kind {
                graft_proto::types::RefKind::Line => format!("refs/heads/{}", r.name),
                graft_proto::types::RefKind::Tag => match r.name.strip_prefix(&tag_ref("")) {
                    Some(tag) => format!("refs/tags/{tag}"),
                    None => continue,
                },
                _ => continue,
            };
            refs.push((name, r.target));
        }
    }
    let committer = default_committer();
    let lands = WrittenAt(system_ms());
    let mut exporter = Exporter::new(api, cx, db, repo, &committer, &lands, dest)?;
    Ok(exporter.export_refs(&refs).await?)
}
