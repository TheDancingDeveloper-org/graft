//! Per-land check refs on a checked line (WI-2624, v0.2 exit design §1–2).
//!
//! A line whose server config has `checks = "post-land"` gets one upstream
//! branch per unsettled land, `refs/heads/<prefix><changeset>`, pointing at
//! that land's export. A post-land checker dispatches a CI run on the ref,
//! so the run's commit is the land's recorded export and the server's CI
//! token exchange can pin an attest-only token to the land.
//!
//! This is a side output of batch mode on a git-primary repository, not
//! mode-B export: it never claims a line's authority, and it refuses a line
//! whose `GetLineConfig` is not `post-land`.
//!
//! Each poll, for each configured line:
//!
//! 1. Read the journal from the line's durable cursor, `RefMoved` entries
//!    only, a bounded page at a time. A move of the line marks it dirty, in
//!    the same statement that saves the cursor.
//! 2. When it is dirty, or refs are held, read the line's `GetLandChecks` rows
//!    from the settled position (every row before it is settled and its ref
//!    deleted). The rows, not the journal, say which moves are lands: a
//!    land gets a row in its own transaction, and a move that is not a land
//!    (pr-batch's sync merge, an admin move) gets none, so it never gets a
//!    ref that nothing would settle.
//! 3. For each row still pending (or bad and not yet reverted) with no ref:
//!    export the land with `graft-git`'s deterministic exporter, record
//!    `(changeset, sha)` through `RecordExports`, record the ref in the
//!    mirror DB, then push it with a lease that requires the branch not to
//!    exist. Every land is exported, not only the newest: bisect needs the
//!    interior lands' refs.
//! 4. For each row settled (good, reverted, or bad with its revert landed)
//!    whose ref is held: delete it upstream with a lease on its sha, then
//!    forget it. The settled position moves past the settled prefix.
//!
//! Live refs are therefore at most the unsettled lands, which the
//! checker's window bounds. A crash anywhere re-runs idempotently: the
//! mirror map makes the export byte-identical, `RecordExports` accepts the
//! same pair again, and a refused lease whose branch already holds the same
//! sha is this mirror's own earlier push.
//!
//! Costs follow the change: the journal read is O(entries since the
//! cursor); the rows read is O(rows from the oldest unsettled land), capped
//! at [`MAX_ROWS`]; the first poll of a line finds the journal's end and
//! the rows' end with O(log n) one-entry reads, never a scan.

use std::collections::HashMap;
use std::future::Future;
use std::time::Duration;

use graft_git::GitSha;
use graft_git::export::Committer;
use graft_git::exporter::Exporter;
use graft_model::ChangesetId;
use graft_proto::api::{ObjectApi, RepoApi};
use graft_proto::types::{
    GetLandChecks, GetLineConfig, LandCheck, LandCheckState, LineChecksMode, Page, Watch,
};
use graft_proto::{ApiError, CallContext};
use serde::Deserialize;

use crate::credentials::{CredentialProvider, SecretToken};
use crate::db::{CheckLine, CheckRef, MirrorDb};
use crate::export::{WrittenAt, default_committer, journal_end, system_ms};
use crate::follow::POLL_INTERVAL;
use crate::git::{Mirror, PushOutcome};
use crate::{ExportLog, MirrorError};

/// Journal entries per `Watch` page.
pub const PAGE: u32 = 256;

/// Pages one poll reads per line at most; the next poll goes on.
pub const MAX_PAGES: usize = 64;

/// `GetLandChecks` rows one poll reads per line at most, from the oldest
/// unsettled land. Lands past it get their refs once older lands settle.
pub const MAX_ROWS: u32 = 1024;

/// One-row probes the first poll spends finding the rows' end.
const MAX_PROBES: usize = 128;

/// The prefix every check ref must sit under: the CI token exchange's
/// binding covers `refs/heads/graft/`.
pub const REQUIRED_PREFIX: &str = "graft/";

/// The journal kind that says a line moved.
const REF_MOVED: &str = "RefMoved";

/// One checked line and where its refs go.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CheckRefLine {
    /// The graft line (its server config must be `post-land`).
    pub line: String,
    /// The upstream branch prefix: a land's ref is
    /// `refs/heads/<prefix><changeset>`. Under [`REQUIRED_PREFIX`], ending
    /// in `/`.
    pub prefix: String,
}

impl CheckRefLine {
    /// `line`'s refs under the default prefix `graft/check/<line>/`.
    pub fn new(line: impl Into<String>) -> Self {
        let line = line.into();
        CheckRefLine {
            prefix: default_prefix(&line),
            line,
        }
    }

    /// The branch (under `refs/heads/`) of `changeset`'s ref.
    pub fn branch(&self, changeset: ChangesetId) -> String {
        format!("{}{changeset}", self.prefix)
    }
}

/// `graft/check/<line>/`.
pub fn default_prefix(line: &str) -> String {
    format!("graft/check/{line}/")
}

/// What to keep check refs for.
#[derive(Clone, Debug)]
pub struct CheckRefsConfig {
    /// The graft repository.
    pub repo: String,
    /// The checked lines.
    pub lines: Vec<CheckRefLine>,
    /// Time between polls.
    pub interval: Duration,
    /// The committer of native changesets (as export writes them).
    pub committer: Committer,
}

impl CheckRefsConfig {
    /// Check refs for `lines` of `repo`, every [`POLL_INTERVAL`].
    pub fn new(repo: impl Into<String>, lines: Vec<CheckRefLine>) -> Self {
        CheckRefsConfig {
            repo: repo.into(),
            lines,
            interval: POLL_INTERVAL,
            committer: default_committer(),
        }
    }
}

/// What one poll did for one line.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct CheckRefsReport {
    /// Refs pushed this poll: each land and its exported commit.
    pub created: Vec<(ChangesetId, GitSha)>,
    /// Lands whose refs were deleted this poll (settled).
    pub deleted: Vec<ChangesetId>,
    /// Refs the line holds after the poll.
    pub held: usize,
}

/// One line's poll result.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum CheckRefsOutcome {
    /// The poll ran.
    Polled(CheckRefsReport),
    /// The poll failed for this line (described); the next poll retries.
    Failed(String),
}

/// The part of a `RefMoved` body this reads.
#[derive(Deserialize)]
struct RefMoved {
    name: String,
}

/// Whether `row` is settled: no check of it will run again.
fn settled(row: &LandCheck) -> bool {
    match row.state {
        LandCheckState::Good | LandCheckState::Reverted => true,
        LandCheckState::Bad => row.revert.is_some(),
        _ => false,
    }
}

fn config_err(what: impl Into<String>) -> MirrorError {
    MirrorError::Config(what.into())
}

/// Keeps per-land check refs for checked lines of one repository.
pub struct CheckRefs<'a, A, C> {
    api: &'a A,
    cx: CallContext,
    db: &'a MirrorDb,
    mirror: Mirror,
    credentials: &'a C,
    exports: &'a dyn ExportLog,
    config: CheckRefsConfig,
    clock: fn() -> i64,
    calls: u64,
    verified: bool,
}

impl<'a, A, C> CheckRefs<'a, A, C>
where
    A: RepoApi + ObjectApi,
    C: CredentialProvider,
{
    /// Check refs for `config.lines`, recording each export in `exports`
    /// before it is pushed. Claims no authority: the lines stay as they are
    /// (batch mode's integration lines).
    pub fn new(
        api: &'a A,
        cx: CallContext,
        db: &'a MirrorDb,
        mirror: Mirror,
        credentials: &'a C,
        exports: &'a dyn ExportLog,
        config: CheckRefsConfig,
    ) -> Self {
        CheckRefs {
            api,
            cx,
            db,
            mirror,
            credentials,
            exports,
            config,
            clock: system_ms,
            calls: 0,
            verified: false,
        }
    }

    /// Reads time from `clock` (milliseconds since the epoch).
    pub fn with_clock(mut self, clock: fn() -> i64) -> Self {
        self.clock = clock;
        self
    }

    /// The time between polls.
    pub fn interval(&self) -> Duration {
        self.config.interval
    }

    /// Refuses unless every line's `GetLineConfig` is `post-land`, with a
    /// one-line error. A server without `GetLineConfig` cannot be checked,
    /// so it is refused too.
    pub async fn verify(&mut self) -> Result<(), MirrorError> {
        for l in self.config.lines.clone() {
            let cx = self.call("line-config");
            let cfg = self
                .api
                .get_line_config(
                    &cx,
                    GetLineConfig {
                        repo: self.config.repo.clone(),
                        line: l.line.clone(),
                    },
                )
                .await;
            match cfg {
                Ok(cfg) if cfg.checks == LineChecksMode::PostLand => {}
                Ok(_) => {
                    return Err(config_err(format!(
                        "line {} checks are off; check refs need a post-land line",
                        l.line
                    )));
                }
                Err(ApiError::Rejected { policy, .. }) if policy == "unimplemented" => {
                    return Err(config_err(format!(
                        "the server does not implement GetLineConfig; line {} cannot be \
                         verified as post-land, so check refs refuse it",
                        l.line
                    )));
                }
                Err(e) => return Err(e.into()),
            }
        }
        self.verified = true;
        Ok(())
    }

    /// Polls until `shutdown` completes, every [`CheckRefsConfig::interval`].
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
                tracing::warn!(repo = %self.config.repo, error = %e, "mirror check refs poll failed");
            }
        }
    }

    /// One poll of every line. Fails when a line is not post-land (checked
    /// once); per-line failures are reported in the outcomes.
    pub async fn poll_once(&mut self) -> Result<Vec<(String, CheckRefsOutcome)>, MirrorError> {
        if !self.verified {
            self.verify().await?;
        }
        self.mirror.ensure().await?;
        let lines = self.config.lines.clone();
        let mut out = Vec::with_capacity(lines.len());
        for l in &lines {
            let outcome = match self.poll_line(l).await {
                Ok(report) => CheckRefsOutcome::Polled(report),
                Err(e) => {
                    tracing::warn!(
                        repo = %self.config.repo, line = %l.line, error = %e,
                        "mirror check refs of a line failed; retried on the next poll"
                    );
                    CheckRefsOutcome::Failed(e.to_string())
                }
            };
            out.push((l.line.clone(), outcome));
        }
        Ok(out)
    }

    fn call(&mut self, what: &str) -> CallContext {
        self.calls += 1;
        let mut cx = self.cx.clone();
        cx.request_id = format!("check-refs-{}-{}-{what}", (self.clock)(), self.calls);
        cx
    }

    async fn poll_line(&mut self, l: &CheckRefLine) -> Result<CheckRefsReport, MirrorError> {
        let mut state = match self.db.check_line(&l.line)? {
            Some(state) => state,
            None => self.bootstrap(l).await?,
        };
        self.read_journal(l, &mut state).await?;
        let mut held: HashMap<ChangesetId, CheckRef> = self
            .db
            .check_refs(&l.line)?
            .into_iter()
            .map(|r| (r.changeset, r))
            .collect();
        let mut report = CheckRefsReport::default();
        if state.dirty || !held.is_empty() {
            // A capped read leaves the line dirty: the rest is read next poll.
            let capped = self.settle(l, &mut state, &mut held, &mut report).await?;
            state.dirty = capped;
            self.db.set_check_line(&l.line, &state)?;
        }
        report.held = held.len();
        Ok(report)
    }

    /// The first state of a line: the rows' end, then the journal's end,
    /// and `dirty`. A land after the rows' end is read from the rows,
    /// whichever side of the journal's end it falls. Lands before it are not
    /// given refs.
    async fn bootstrap(&mut self, l: &CheckRefLine) -> Result<CheckLine, MirrorError> {
        let pos = self.rows_end(&l.line).await?;
        let cx = self.call("journal-end");
        let lsn = journal_end(self.api, &cx, &self.config.repo).await?;
        let state = CheckLine {
            lsn,
            pos,
            dirty: true,
        };
        self.db.set_check_line(&l.line, &state)?;
        tracing::info!(
            repo = %self.config.repo, line = %l.line, lsn, pos,
            "mirror check refs started"
        );
        Ok(state)
    }

    /// Reads `RefMoved` entries from the cursor, at most [`MAX_PAGES`]
    /// pages, and saves the cursor with `dirty` set when `l.line` moved.
    async fn read_journal(
        &mut self,
        l: &CheckRefLine,
        state: &mut CheckLine,
    ) -> Result<(), MirrorError> {
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
            for e in &page.entries {
                let m: RefMoved = serde_json::from_str(&e.body).map_err(|_| {
                    config_err(format!("journal entry {} is not a RefMoved body", e.lsn))
                })?;
                state.dirty |= m.name == l.line;
            }
            if page.next_lsn <= state.lsn {
                break;
            }
            state.lsn = page.next_lsn;
            self.db.set_check_line(&l.line, state)?;
        }
        Ok(())
    }

    /// Reads the rows from the settled position: gives each unsettled land
    /// its ref, deletes the refs of settled lands, and moves the settled
    /// position past the settled prefix. Whether the read stopped at
    /// [`MAX_ROWS`].
    async fn settle(
        &mut self,
        l: &CheckRefLine,
        state: &mut CheckLine,
        held: &mut HashMap<ChangesetId, CheckRef>,
        report: &mut CheckRefsReport,
    ) -> Result<bool, MirrorError> {
        let cx = self.call("land-checks");
        let rows = graft_client::land_checks(
            self.api,
            &cx,
            GetLandChecks {
                repo: self.config.repo.clone(),
                line: l.line.clone(),
                from_pos: state.pos,
                page: Page {
                    limit: 0,
                    token: None,
                },
            },
            MAX_ROWS,
        )
        .await?;
        let mut prefix = true;
        for row in &rows {
            if settled(row) {
                if let Some(r) = held.remove(&row.changeset) {
                    self.delete(l, &r).await?;
                    report.deleted.push(r.changeset);
                }
                if prefix {
                    state.pos = row.pos + 1;
                    self.db.set_check_line(&l.line, state)?;
                }
                continue;
            }
            prefix = false;
            match held.get(&row.changeset).copied() {
                Some(r) if r.pushed => {}
                Some(r) => {
                    let r = self.push(l, r).await?;
                    held.insert(r.changeset, r);
                }
                None => {
                    let r = self.create(l, row.changeset).await?;
                    report.created.push((r.changeset, r.sha));
                    held.insert(r.changeset, r);
                }
            }
        }
        Ok(rows.len() >= MAX_ROWS as usize)
    }

    /// Exports `land`, records the export with the server and in the
    /// mirror DB, then pushes its ref.
    async fn create(
        &mut self,
        l: &CheckRefLine,
        land: ChangesetId,
    ) -> Result<CheckRef, MirrorError> {
        let sha = self.export(land).await?;
        let cx = self.call("record-export");
        self.exports
            .record_exports(&cx, &self.config.repo, &[(land, sha)])
            .await?;
        let r = CheckRef {
            changeset: land,
            sha,
            pushed: false,
        };
        self.db.put_check_ref(&l.line, &r)?;
        self.push(l, r).await
    }

    /// Writes `land` and its missing ancestors into the local mirror, and
    /// returns its git sha. The mirror map keeps the result, so a re-run
    /// gives the same sha.
    async fn export(&mut self, land: ChangesetId) -> Result<GitSha, MirrorError> {
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
        Ok(exporter.export_changeset(land).await?)
    }

    async fn token(&self) -> Result<Option<SecretToken>, MirrorError> {
        if self.mirror.is_https() {
            Ok(Some(self.credentials.token().await?))
        } else {
            Ok(None)
        }
    }

    /// Pushes `r`'s ref with a lease that requires it not to exist. A
    /// refused lease is fine when the branch already holds `r.sha` (an
    /// earlier push this mirror did not record); anything else is an error,
    /// and nothing is forced.
    async fn push(&mut self, l: &CheckRefLine, mut r: CheckRef) -> Result<CheckRef, MirrorError> {
        let branch = l.branch(r.changeset);
        let token = self.token().await?;
        let bearer = token.as_ref().map(SecretToken::expose);
        let outcome = self.mirror.push_lease(&branch, r.sha, None, bearer).await?;
        if outcome == PushOutcome::Rejected {
            let upstream = self.upstream(&branch).await?;
            if upstream != Some(r.sha) {
                return Err(config_err(format!(
                    "check ref {branch} exists upstream at {}, not the land's export {}; \
                     nothing was forced",
                    upstream.map_or_else(|| "nothing".to_owned(), |s| s.to_hex()),
                    r.sha
                )));
            }
        }
        r.pushed = true;
        self.db.put_check_ref(&l.line, &r)?;
        tracing::info!(
            repo = %self.config.repo, line = %l.line, branch = %branch, sha = %r.sha,
            "mirror check ref pushed"
        );
        Ok(r)
    }

    /// Deletes `r`'s ref with a lease on its sha, then forgets it. A branch
    /// already gone is forgotten; one moved out of band is left as it is
    /// (logged) and forgotten.
    async fn delete(&mut self, l: &CheckRefLine, r: &CheckRef) -> Result<(), MirrorError> {
        let branch = l.branch(r.changeset);
        let token = self.token().await?;
        let bearer = token.as_ref().map(SecretToken::expose);
        let outcome = self.mirror.delete_lease(&branch, r.sha, bearer).await;
        if !matches!(outcome, Ok(PushOutcome::Pushed)) {
            match self.upstream(&branch).await? {
                None => {}
                Some(up) if up == r.sha => {
                    // Still there at our sha: the delete itself failed.
                    return Err(match outcome {
                        Err(e) => e.into(),
                        Ok(_) => config_err(format!("check ref {branch} was not deleted")),
                    });
                }
                Some(up) => tracing::warn!(
                    repo = %self.config.repo, line = %l.line, branch = %branch,
                    expected = %r.sha, upstream = %up,
                    "check ref moved out of band; left in place, not forced"
                ),
            }
        }
        self.db.delete_check_ref(&l.line, r.changeset)?;
        Ok(())
    }

    async fn upstream(&self, branch: &str) -> Result<Option<GitSha>, MirrorError> {
        Ok(self
            .mirror
            .ls_remote(&[branch])
            .await?
            .pop()
            .and_then(|(_, s)| s))
    }

    /// The position after the line's last `GetLandChecks` row, by doubling
    /// then bisecting on one-row reads: O(log n) calls, never a scan.
    async fn rows_end(&mut self, line: &str) -> Result<u64, MirrorError> {
        let Some(mut lo) = self.row_at_or_after(line, 0).await? else {
            return Ok(0);
        };
        // A row is at `lo`; none is at or after `hi`.
        let mut hi = None;
        let mut step = 1u64;
        for _ in 0..MAX_PROBES {
            let p = lo.saturating_add(step);
            match self.row_at_or_after(line, p).await? {
                Some(pos) => lo = pos,
                None => {
                    hi = Some(p);
                    break;
                }
            }
            step = step.saturating_mul(2);
        }
        let mut hi = hi.ok_or_else(|| config_err(format!("line {line}: no end of land rows")))?;
        for _ in 0..MAX_PROBES {
            if hi - lo <= 1 {
                break;
            }
            let mid = lo + (hi - lo) / 2;
            match self.row_at_or_after(line, mid).await? {
                Some(pos) => lo = pos,
                None => hi = mid,
            }
        }
        Ok(lo + 1)
    }

    /// The position of the first row at or after `pos`, if any.
    async fn row_at_or_after(&mut self, line: &str, pos: u64) -> Result<Option<u64>, MirrorError> {
        let cx = self.call("land-checks");
        let page = self
            .api
            .get_land_checks(
                &cx,
                GetLandChecks {
                    repo: self.config.repo.clone(),
                    line: line.to_owned(),
                    from_pos: pos,
                    page: Page {
                        limit: 1,
                        token: None,
                    },
                },
            )
            .await?;
        Ok(page.rows.first().map(|r| r.pos))
    }
}
