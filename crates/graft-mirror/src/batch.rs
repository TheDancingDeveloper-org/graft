//! Batch mode (decision D21, `docs/architecture.md` §3c): agents land on a
//! graft **integration line** forked from the followed GitHub `main`, and
//! the batch reaches GitHub as **one PR** carrying one commit per land
//! (decision D26), which oversight reviews and `land-pr` rebase-merges
//! (decision D28). GitHub CI runs once per batch.
//!
//! `graft-mirror pr-batch <line> --base main` runs [`Batcher::run`]:
//!
//! 1. **Follow** the base branch once, so graft's base line holds GitHub's
//!    `main`. A stopped (diverged) base stops the run; nothing is forced.
//! 2. **Track** the line's last batch. A merged PR whose merge commit follow
//!    has imported settles the batch as `merged`, and the next batch counts
//!    lands from that batch's tip. A PR closed unmerged settles it as
//!    `rejected` and leaves the line as it is, so its lands stay in the next
//!    batch. A PR still open means the run waits: one batch per line is open
//!    at a time.
//! 3. **Sync** the integration line onto a moved base. The line's net change
//!    since the base it last absorbed (one changeset, `diff` of the two
//!    snapshots) is rebased onto the base tip with graft's `Rebase` (line
//!    merge, decision D16). A conflict is reported with its paths and
//!    nothing moves. Otherwise a merge changeset (first parent the line tip,
//!    second the base tip) carries the line to the rebased snapshot, and the
//!    line moves to it by compare-and-swap: a fast-forward, never a rewrite.
//! 4. **Export** the batch as a linear chain on GitHub's current `main`,
//!    one commit per land, oldest first (D26). Each land's own change (the
//!    diff from its first parent) is replayed onto the base as an unlanded
//!    changeset, and each replay's git tree (written by `graft-git`'s
//!    deterministic exporter) becomes that land's commit: the land's author
//!    and author time, the mirror as committer at the same time, the land's
//!    message, and `Graft-Changeset`, `Graft-Session`, `Graft-Work-Item`,
//!    `Graft-Checks` and `Graft-Batch` trailers. A revert land is its own
//!    commit. The last commit's tree must equal the cut tip's tree (the
//!    root check). When it does not (a land's change does not replay onto a
//!    moved base), or the batch has more than [`MAX_COMMITS`] lands, the
//!    batch is exported as one squashed commit instead, as D21 did, and the
//!    PR body and the commit say why. Commit objects are built byte for
//!    byte from recorded fields (`graft_git::export::commit_object`),
//!    stored in the local mirror and recorded (the chain in
//!    `batch_commits`) before anything is pushed, so a retry pushes the
//!    same shas.
//! 5. **Push** branch `graft/batch-<n>` with a lease that requires the
//!    branch not to exist, and **open a PR** into the base (adopting one
//!    already on the branch). The PR body lists the lands (changeset,
//!    author, session, work item, attestations) as many as fit under
//!    [`MAX_PR_BODY`], and names the branch's commits (or the squashed
//!    commit's message, which lists every land) for the rest. It ends with
//!    `Graft-Batch` trailers. The mirror never merges or queues it.
//! 6. **Settle.** Once follow has imported the PR's merge, each per-land
//!    commit is mapped to the commit it merged as: a rebase-merge's new
//!    commits by their `Graft-Changeset` trailer, a merge commit's by its
//!    own shas. A squash-merge maps the batch tip only, as D21 did.
//!
//! Dropping a land (WI-2340): `--exclude <changeset>` lands a `Revert` of it
//! on the integration line, as any land is, so the lands after it stay. Any
//! run that finds an open batch holding a land whose revert is on the line
//! but not in the batch (this run's drop, or one an interrupted run left)
//! supersedes it: its PR is closed with a comment, unless GitHub is already
//! merging it, and the next batch is cut from the same start, carrying the
//! land and its revert and naming the drop and its reason in the PR body. A
//! dropped land and its revert are never split across batches. Phase 1
//! reads no checks to choose a drop (D22): the operator names it.
//!
//! Cutting (WI-2340): [`BatchConfig::max_lands`] caps the lands one batch
//! takes, oldest first; the rest wait for the next batch. The cut point is a
//! line position, and the batch is exported against the base the line held
//! there (the newest sync at or before it, else the base at the batch
//! start). [`BatchConfig::max_age_ms`] holds a batch back until its oldest
//! land (by author time) is that old or it has `max_lands` lands.
//!
//! Costs follow the change: the sync reads one diff and one rebase; the land
//! list is a first-parent `Log` from the tip back to the batch start, capped
//! at [`MAX_LANDS`]; attestations come from a bounded read of the journal's
//! `Attested` entries from the oldest land in the batch, capped at
//! [`MAX_ATTEST_PAGES`] pages and marked as truncated beyond that.

use std::collections::{BTreeMap, HashMap, HashSet};

use graft_git::GitSha;
use graft_git::export::{Committer, commit_object};
use graft_git::exporter::{Exporter, LandTimes, SESSION_KEY, WORK_ITEM_KEY};
use graft_git::map::{ImportedCommit, MirrorMap};
use graft_model::{Author, AuthorKind, Change, ChangesetId, Metadata, Timestamp};
use graft_proto::api::{AdminApi, ObjectApi, RepoApi};
use graft_proto::types::{
    ChangesetView, CreateChangeset, Diff, GetChangesets, Log, MergeMode, Page, Pair, Rebase,
    RefKind, Revert, Watch,
};
use graft_proto::{ApiError, CallContext};
use serde::Deserialize;

use crate::credentials::SecretToken;
use crate::db::MirrorDb;
use crate::export::default_committer;
use crate::follow::{FollowConfig, FollowLine, Follower, LineOutcome};
use crate::git::{Mirror, PushOutcome};
use crate::github::{Forge, Pull};
use crate::text::clean_text;
use crate::{MirrorError, current_ref, move_ref};

/// The authority batch mode claims for an integration line: nothing else
/// (export, follow) writes it from the mirror.
pub const BATCH: &str = "batch";

/// The metadata key on a sync changeset, naming the base tip it absorbed.
/// The land list skips changesets that carry it.
pub const SYNC_KEY: &str = "mirror.batch-sync";

/// The metadata key graft's `Revert` puts on a revert, naming the changeset
/// it inverts (graft-engine's `revert::REVERTS`).
pub const REVERTS_KEY: &str = "graft.reverts";

/// The reason recorded for an `--exclude` given none.
pub const DEFAULT_DROP_REASON: &str = "excluded by the operator";

/// Dropped lands the PR body names; more are counted.
pub const MAX_DROPS_LISTED: usize = 50;

/// Per-land commits one batch carries (D26). The cut takes at most this
/// many lands; a batch with more (one cut could not be applied to) is
/// squashed.
pub const MAX_COMMITS: usize = 500;

/// The trailer naming a per-land commit's land.
pub const CHANGESET_TRAILER: &str = "Graft-Changeset:";

/// The longest land message a per-land commit keeps, in characters.
const MESSAGE_CHARS: usize = 8000;

/// The metadata key on a replay changeset (unlanded), naming the land it
/// replays.
pub const REPLAY_KEY: &str = "mirror.batch-replay";

/// The prefix of a batch branch: `graft/batch-<n>`.
pub const BRANCH_PREFIX: &str = "graft/batch-";

/// The trailer naming the batch, in the squashed commit and the PR body.
pub const BATCH_TRAILER: &str = "Graft-Batch:";

/// The longest PR body graft-mirror writes, in characters: under GitHub's
/// 65,536, with room for the trailers. Lands that do not fit are listed in
/// the squashed commit's message, which lists every land.
pub const MAX_PR_BODY: usize = 60_000;

/// Lands one batch lists. A batch with more is still exported; the PR body
/// and the commit message list the newest `MAX_LANDS` and say how many more
/// there are, at least.
pub const MAX_LANDS: usize = 5000;

/// Checks listed per land; more are counted.
pub const MAX_CHECKS: usize = 8;

/// Journal pages read for attestations, at most.
pub const MAX_ATTEST_PAGES: usize = 16;

/// Entries per page (journal, log, diff).
const PAGE: u32 = 256;

/// Changesets per `GetChangesets` call.
const FETCH: usize = 256;

/// The longest PR title.
const TITLE_CHARS: usize = 120;

/// The longest text kept from one land's subject or one metadata value.
const FIELD_CHARS: usize = 100;

/// Where a batch is.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BatchState {
    /// The commits are built and recorded; nothing pushed yet.
    Planned,
    /// `graft/batch-<n>` holds the batch's commits.
    Pushed,
    /// A PR is open on the branch, waiting for the operator.
    Open,
    /// The PR merged and follow has imported its merge commit. Final.
    Merged,
    /// The PR closed unmerged, or the branch could not be written. Final.
    Rejected,
}

impl BatchState {
    /// The stored name.
    pub fn as_str(self) -> &'static str {
        match self {
            BatchState::Planned => "planned",
            BatchState::Pushed => "pushed",
            BatchState::Open => "open",
            BatchState::Merged => "merged",
            BatchState::Rejected => "rejected",
        }
    }

    /// The state with this stored name.
    pub fn parse(s: &str) -> Option<BatchState> {
        Some(match s {
            "planned" => BatchState::Planned,
            "pushed" => BatchState::Pushed,
            "open" => BatchState::Open,
            "merged" => BatchState::Merged,
            "rejected" => BatchState::Rejected,
            _ => return None,
        })
    }

    /// Whether no further step will run.
    pub fn is_terminal(self) -> bool {
        matches!(self, BatchState::Merged | BatchState::Rejected)
    }
}

/// One batch and what the mirror has done for it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BatchRecord {
    /// The batch number (unique across lines).
    pub number: u64,
    /// The integration line.
    pub line: String,
    /// The upstream branch the PR merges into.
    pub base_branch: String,
    /// The base line's changeset the squash is computed against.
    pub base: ChangesetId,
    /// Its git sha: the squashed commit's parent.
    pub base_sha: GitSha,
    /// The integration line position the batch counts lands after.
    pub start: ChangesetId,
    /// The integration tip the batch exports.
    pub tip: ChangesetId,
    /// Lands in the batch (at most [`MAX_LANDS`]).
    pub lands: u64,
    /// When the batch was planned (ms): a squashed commit's date.
    pub created_ms: i64,
    /// `graft/batch-<n>`.
    pub branch: String,
    /// The PR title.
    pub title: String,
    /// The PR body (also a squashed commit's message body).
    pub body: String,
    /// The branch head: the last per-land commit (D26), or the squashed
    /// commit when the batch fell back to one.
    pub squash_sha: GitSha,
    /// The PR number, once opened.
    pub pr_number: Option<u64>,
    /// The PR's web address.
    pub pr_url: Option<String>,
    /// Where the batch is.
    pub state: BatchState,
    /// Why it is where it is.
    pub detail: String,
    /// The PR's merge commit, once merged.
    pub merge_sha: Option<GitSha>,
    /// The changeset follow imported that merge commit as.
    pub merged: Option<ChangesetId>,
}

/// One per-land commit of a batch (D26).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct BatchCommit {
    /// Its place in the chain, from 0 (the oldest land).
    pub seq: u32,
    /// The land it carries.
    pub land: ChangesetId,
    /// The commit pushed on the batch branch.
    pub sha: GitSha,
    /// The commit it reached the base branch as, once the PR merged and
    /// follow imported it: a rebase-merge's new commit, or `sha` itself.
    pub merged: Option<GitSha>,
}

/// Batch mode's record of one integration line.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct BatchLine {
    /// The base line changeset the integration line has absorbed.
    pub synced: ChangesetId,
    /// The changeset the next sync diffs the line from: its snapshot is
    /// what the integration line holds of the base. The fork point, the
    /// last synced base tip, or the tip of the last merged batch.
    pub sync_from: ChangesetId,
    /// The next batch counts lands after this changeset.
    pub start: ChangesetId,
    /// The base line changeset whose snapshot the line held at `start`: a
    /// batch cut before the line's newest sync is exported against it.
    /// `None` for a line recorded before WI-2340: such a batch is not cut
    /// before a sync until its next merge records it.
    pub start_base: Option<ChangesetId>,
    /// A merged batch cut before the line's newest sync: the next sync
    /// starts from that batch rebased onto `synced`, not from `sync_from`.
    pub sync_batch: Option<u64>,
}

/// What batch mode works on.
#[derive(Clone, Debug)]
pub struct BatchConfig {
    /// The graft repository.
    pub repo: String,
    /// The integration line.
    pub line: String,
    /// The followed base: the upstream branch (`main`) and its graft line.
    pub base: FollowLine,
    /// The follow settings for the base (its `lines` are replaced by
    /// `base`).
    pub follow: FollowConfig,
    /// The identity of the squashed commit and of sync changesets.
    pub committer: Committer,
    /// Lands to drop from the line's batches: each is reverted on the line.
    pub exclude: Vec<ChangesetId>,
    /// Why the lands are dropped (named in the PR body).
    pub drop_reason: Option<String>,
    /// The most lands one batch takes; newer lands wait for the next one.
    pub max_lands: Option<usize>,
    /// Hold a batch until its oldest land is this old (ms, by author time),
    /// unless it already has `max_lands` lands.
    pub max_age_ms: Option<i64>,
}

impl BatchConfig {
    /// Batches `line` of `follow.repo` against `base`, as
    /// [`default_committer`].
    pub fn new(line: impl Into<String>, base: FollowLine, follow: FollowConfig) -> Self {
        BatchConfig {
            repo: follow.repo.clone(),
            line: line.into(),
            base,
            follow,
            committer: default_committer(),
            exclude: Vec::new(),
            drop_reason: None,
            max_lands: None,
            max_age_ms: None,
        }
    }
}

/// What a run did.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum BatchOutcome {
    /// A new batch is exported and its PR is open.
    Opened(BatchRecord),
    /// The line's last batch is still open (or merged on GitHub and not yet
    /// followed); no new batch was made.
    Waiting(BatchRecord),
    /// The integration line holds nothing the base lacks.
    NothingToExport,
    /// The line's change does not rebase onto the moved base; nothing
    /// moved.
    Conflict {
        /// The base tip it was rebased onto.
        onto: ChangesetId,
        /// The conflicting paths.
        paths: Vec<String>,
    },
    /// Follow stopped the base line (described); nothing was exported.
    BaseStopped(String),
    /// The pending lands are fewer than `max_lands` and younger than
    /// `max_age_ms`; no batch yet.
    NotDue {
        /// Lands waiting.
        lands: usize,
        /// The oldest one's author time (ms).
        oldest_ms: i64,
    },
}

/// A run's outcome, with what it settled and synced on the way.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BatchRun {
    /// The earlier batch this run found merged or rejected.
    pub settled: Option<BatchRecord>,
    /// The sync changeset the integration line moved to, if it synced.
    pub synced: Option<ChangesetId>,
    /// Lands this run dropped, each with the revert it landed.
    pub dropped: Vec<(ChangesetId, ChangesetId)>,
    /// What happened.
    pub outcome: BatchOutcome,
}

/// The body of an `Attested` journal entry (graft-engine's durable format).
#[derive(Deserialize)]
struct Attested {
    changeset: String,
    check: String,
    passed: bool,
}

/// One land, as the PR body lists it.
struct LandLine {
    id: ChangesetId,
    author: String,
    session: Option<String>,
    work_item: Option<String>,
    subject: String,
    /// Each check's latest result, by name.
    checks: BTreeMap<String, bool>,
    /// Author time (ms).
    authored_ms: i64,
    /// The changeset this land reverts, when it is a revert.
    reverts: Option<ChangesetId>,
    /// Its first parent: its own change is the diff from it.
    parent: Option<ChangesetId>,
    /// The author's name and email for a git ident.
    ident: (String, String),
    /// Author time, with its offset.
    authored: Timestamp,
    /// The whole message, cleaned and bounded.
    message: String,
}

/// A land dropped from a batch: it and its revert are both in the batch.
struct Dropped {
    land: ChangesetId,
    revert: ChangesetId,
    reason: String,
}

/// Exported native changesets of the integration line are dated by the
/// batch's planning time; the mirror map keeps the result.
struct Stamp(i64);

impl LandTimes for Stamp {
    fn landed_at(&self, _id: ChangesetId) -> Result<Timestamp, graft_git::GitError> {
        Ok(Timestamp {
            ms: self.0,
            offset_min: 0,
        })
    }
}

fn config_err(what: impl Into<String>) -> MirrorError {
    MirrorError::Config(what.into())
}

fn system_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_millis() as i64)
}

/// `<seconds> +0000`, as git writes a UTC date.
fn git_time(ms: i64) -> String {
    format!("{} +0000", ms.div_euclid(1000))
}

/// `<seconds> <+hhmm>`, as git writes a date with its offset.
fn git_time_at(t: Timestamp) -> String {
    let off = i32::from(t.offset_min);
    let sign = if off < 0 { '-' } else { '+' };
    let off = off.abs();
    format!(
        "{} {sign}{:02}{:02}",
        t.ms.div_euclid(1000),
        off / 60,
        off % 60
    )
}

/// A name or email made safe for a git ident line: no angle brackets,
/// newlines or other control characters.
fn ident_part(s: &str) -> String {
    let out: String = s
        .chars()
        .filter(|c| !c.is_control() && *c != '<' && *c != '>')
        .take(FIELD_CHARS)
        .collect();
    let out = out.trim().to_owned();
    if out.is_empty() {
        "unknown".to_owned()
    } else {
        out
    }
}

/// A short form of a changeset id for listings.
fn short(id: ChangesetId) -> String {
    id.to_string().chars().take(13).collect()
}

/// Batches one integration line into squashed GitHub PRs.
pub struct Batcher<'a, A, F> {
    api: &'a A,
    cx: CallContext,
    db: &'a MirrorDb,
    mirror: Mirror,
    forge: &'a F,
    config: BatchConfig,
    clock: fn() -> i64,
    calls: u64,
}

impl<'a, A, F> Batcher<'a, A, F>
where
    A: AdminApi + ObjectApi + RepoApi,
    F: Forge,
{
    /// A batcher for `config.line`. Claims the line for batch mode: a line
    /// another authority claims is refused, and so is a line that is its own
    /// base.
    pub fn new(
        api: &'a A,
        cx: CallContext,
        db: &'a MirrorDb,
        mirror: Mirror,
        forge: &'a F,
        config: BatchConfig,
    ) -> Result<Self, MirrorError> {
        if config.line == config.base.line {
            return Err(config_err(format!(
                "line {} is the base line; batch an integration line forked from it",
                config.line
            )));
        }
        let holder = db.claim_authority(&config.line, BATCH)?;
        if holder != BATCH {
            return Err(config_err(format!(
                "line {} is claimed by {holder}; it cannot also be batched",
                config.line
            )));
        }
        Ok(Batcher {
            api,
            cx,
            db,
            mirror,
            forge,
            config,
            clock: system_ms,
            calls: 0,
        })
    }

    /// Reads time from `clock` (milliseconds since the epoch).
    pub fn with_clock(mut self, clock: fn() -> i64) -> Self {
        self.clock = clock;
        self
    }

    /// Follows the base, settles or advances the last batch, syncs the line
    /// and, when nothing is open, exports a new batch and opens its PR.
    pub async fn run(&mut self) -> Result<BatchRun, MirrorError> {
        let mut run = BatchRun {
            settled: None,
            synced: None,
            dropped: Vec::new(),
            outcome: BatchOutcome::NothingToExport,
        };
        if let Some(stopped) = self.follow().await? {
            run.outcome = BatchOutcome::BaseStopped(stopped);
            return Ok(run);
        }
        let mut open = None;
        if let Some(mut rec) = self.db.last_batch(&self.config.line)?
            && !rec.state.is_terminal()
        {
            self.advance(&mut rec).await?;
            if rec.state.is_terminal() {
                run.settled = Some(rec);
            } else {
                open = Some(rec);
            }
        }
        let mut conflict = None;
        if !self.config.exclude.is_empty()
            && let Err(c) = self.drop_lands(&mut run.dropped).await?
        {
            conflict = Some(c);
        }
        // Checked on every run, not only after this run's drops: a run that
        // died or conflicted part way may have left reverts on the line that
        // the open batch lacks.
        if let Some(mut rec) = open {
            let stale = self.stale_drops(&rec).await?;
            if stale.is_empty() || !self.supersede(&mut rec, &stale).await? {
                run.outcome = conflict.unwrap_or(BatchOutcome::Waiting(rec));
                return Ok(run);
            }
            run.settled = Some(rec);
        }
        if let Some(c) = conflict {
            run.outcome = c;
            return Ok(run);
        }
        let base_tip = self.tip(&self.config.base.line.clone()).await?;
        let mut tip = self.tip(&self.config.line.clone()).await?;
        let mut state = self.line_state(tip, base_tip).await?;
        if state.synced != base_tip {
            match self.sync(&mut state, tip, base_tip).await? {
                Ok(moved) => {
                    if let Some(m) = moved {
                        tip = m;
                        run.synced = Some(m);
                    }
                }
                Err(conflict) => {
                    run.outcome = conflict;
                    return Ok(run);
                }
            }
        }
        match self.plan(&state, tip, base_tip).await? {
            Ok(Some(mut rec)) => {
                self.advance(&mut rec).await?;
                run.outcome = BatchOutcome::Opened(rec);
            }
            Ok(None) => {}
            Err(outcome) => run.outcome = outcome,
        }
        Ok(run)
    }

    /// Advances the line's last batch without making a new one: push and
    /// open it if that was interrupted, then read its PR. Returns it.
    pub async fn track(&mut self) -> Result<Option<BatchRecord>, MirrorError> {
        let Some(mut rec) = self.db.last_batch(&self.config.line)? else {
            return Ok(None);
        };
        if !rec.state.is_terminal() {
            self.advance(&mut rec).await?;
        }
        Ok(Some(rec))
    }

    fn call(&mut self, what: &str) -> CallContext {
        self.calls += 1;
        let mut cx = self.cx.clone();
        cx.request_id = format!("batch-{}-{}-{what}", (self.clock)(), self.calls);
        cx
    }

    /// One follow poll of the base. `Some` (described) when the base line is
    /// stopped.
    async fn follow(&mut self) -> Result<Option<String>, MirrorError> {
        self.check_mapped().await?;
        let mut config = self.config.follow.clone();
        config.repo = self.config.repo.clone();
        config.lines = vec![self.config.base.clone()];
        let mut follower = Follower::new(
            self.api,
            self.cx.clone(),
            self.db,
            self.mirror.clone(),
            config,
        )
        .with_clock(self.clock);
        let outcomes = follower.poll_once().await?;
        match outcomes.into_iter().next().map(|(_, o)| o) {
            Some(LineOutcome::UpToDate | LineOutcome::Imported { .. }) => Ok(None),
            Some(LineOutcome::Diverged { reason, upstream }) => Ok(Some(format!(
                "follow stopped line {} ({}) on upstream {upstream}",
                self.config.base.line,
                reason.as_str()
            ))),
            Some(LineOutcome::MissingUpstream) => Err(config_err(format!(
                "upstream has no branch {}",
                self.config.base.branch
            ))),
            Some(LineOutcome::Failed(what)) => Err(config_err(format!(
                "following {} failed: {what}",
                self.config.base.branch
            ))),
            None => Err(config_err("follow reported no line")),
        }
    }

    /// Refuses a DB that cannot map the base: graft's base line has a tip
    /// the DB has no commit for, and the DB has never followed the line or
    /// followed it to a changeset this repo does not hold. Follow alone
    /// would call that `graft-moved`, which hides an empty or wrong DB.
    async fn check_mapped(&mut self) -> Result<(), MirrorError> {
        let base = self.config.base.line.clone();
        let cx = self.call("mapped");
        let repo = self.config.repo.clone();
        let Some(tip) = current_ref(self.api, &cx, &repo, &base).await? else {
            return Ok(());
        };
        if self.db.sha_of(tip)?.is_some() {
            return Ok(());
        }
        let followed = match self.db.line(&base)?.upstream {
            None => None,
            Some(up) => self.db.changeset_of(up)?,
        };
        if let Some(c) = followed
            && !self.views(&[c]).await?.is_empty()
        {
            // The DB belongs here and graft really moved: follow says so.
            return Ok(());
        }
        Err(MirrorError::Unmapped {
            db: self.db.path().to_owned(),
            line: base,
            empty: self.db.commit_count()? == 0,
        })
    }

    async fn tip(&mut self, line: &str) -> Result<ChangesetId, MirrorError> {
        let cx = self.call("tip");
        let repo = self.config.repo.clone();
        current_ref(self.api, &cx, &repo, line)
            .await?
            .ok_or_else(|| config_err(format!("line {line} has no tip")))
    }

    /// The line's record, started at its fork point from the base on the
    /// first run.
    async fn line_state(
        &mut self,
        tip: ChangesetId,
        base_tip: ChangesetId,
    ) -> Result<BatchLine, MirrorError> {
        if let Some(state) = self.db.batch_line(&self.config.line)? {
            return Ok(state);
        }
        let cx = self.call("merge-base");
        let fork = self
            .api
            .merge_base(
                &cx,
                Pair {
                    repo: self.config.repo.clone(),
                    a: tip,
                    b: base_tip,
                },
            )
            .await?
            .ok_or_else(|| {
                config_err(format!(
                    "line {} does not fork from {}",
                    self.config.line, self.config.base.line
                ))
            })?;
        let state = BatchLine {
            synced: fork,
            sync_from: fork,
            start: fork,
            start_base: Some(fork),
            sync_batch: None,
        };
        self.db.set_batch_line(&self.config.line, &state)?;
        tracing::info!(
            repo = %self.config.repo, line = %self.config.line, fork = %fork,
            "mirror batch mode started"
        );
        Ok(state)
    }

    async fn view(&mut self, id: ChangesetId) -> Result<ChangesetView, MirrorError> {
        let mut found = self.views(&[id]).await?;
        found
            .remove(&id)
            .ok_or_else(|| config_err(format!("changeset {id} is not in graft")))
    }

    /// The views of `ids`, batched. Missing ids are left out.
    async fn views(
        &mut self,
        ids: &[ChangesetId],
    ) -> Result<HashMap<ChangesetId, ChangesetView>, MirrorError> {
        let mut out = HashMap::with_capacity(ids.len());
        for chunk in ids.chunks(FETCH) {
            let cx = self.call("get");
            let got = self
                .api
                .get_changesets(
                    &cx,
                    GetChangesets {
                        repo: self.config.repo.clone(),
                        ids: chunk.to_vec(),
                    },
                )
                .await?;
            for v in got.found {
                out.insert(v.changeset.id(), v);
            }
        }
        Ok(out)
    }

    /// Every change from snapshot `from` to snapshot `to`, page by page.
    async fn diff(
        &mut self,
        from: ChangesetId,
        to: ChangesetId,
    ) -> Result<Vec<Change>, MirrorError> {
        let mut out = Vec::new();
        let mut token = None;
        loop {
            let cx = self.call("diff");
            let page = self
                .api
                .diff(
                    &cx,
                    Diff {
                        repo: self.config.repo.clone(),
                        from,
                        to,
                        paths: Vec::new(),
                        patch: None,
                        page: Page {
                            limit: PAGE,
                            token: token.take(),
                        },
                    },
                )
                .await?;
            out.extend(page.files.into_iter().map(|f| f.change));
            match page.next {
                Some(t) => token = Some(t),
                None => return Ok(out),
            }
        }
    }

    /// A sync changeset on `parents` with `changes`, by the mirror.
    async fn create(
        &mut self,
        parents: Vec<ChangesetId>,
        changes: Vec<Change>,
        message: String,
        absorbed: ChangesetId,
    ) -> Result<ChangesetId, MirrorError> {
        let mut metadata = Metadata::new();
        metadata.insert(SYNC_KEY.to_owned(), absorbed.to_string());
        let at = Timestamp {
            ms: (self.clock)(),
            offset_min: 0,
        };
        Ok(self
            .create_with(parents, changes, message, metadata, at)
            .await?)
    }

    /// A changeset on `parents` with `changes`, by the mirror.
    async fn create_with(
        &mut self,
        parents: Vec<ChangesetId>,
        changes: Vec<Change>,
        message: String,
        metadata: Metadata,
        authored_at: Timestamp,
    ) -> Result<ChangesetId, ApiError> {
        let cx = self.call("create");
        let created = self
            .api
            .create_changeset(
                &cx,
                CreateChangeset {
                    repo: self.config.repo.clone(),
                    parents,
                    changes,
                    author: Author {
                        name: self.config.committer.name.clone(),
                        email: self.config.committer.email.clone(),
                        kind: AuthorKind::Bot,
                    },
                    authored_at,
                    message,
                    metadata,
                    inline_blobs: Vec::new(),
                    workspace: None,
                },
            )
            .await?;
        Ok(created.id)
    }

    /// Carries the integration line onto `base_tip`. `Ok(Ok(Some(m)))` when
    /// the line moved to the sync changeset `m`, `Ok(Ok(None))` when only
    /// the record changed (the line already holds the base's snapshot), and
    /// `Ok(Err(_))` on a conflict, with nothing moved.
    async fn sync(
        &mut self,
        state: &mut BatchLine,
        tip: ChangesetId,
        base_tip: ChangesetId,
    ) -> Result<Result<Option<ChangesetId>, BatchOutcome>, MirrorError> {
        // The line's own change, as one changeset on the snapshot it holds
        // of the base, rebased onto the base tip.
        let from = match state.sync_batch {
            Some(number) => {
                let rec = self
                    .db
                    .batch(number)?
                    .ok_or_else(|| config_err(format!("batch {number} is not recorded")))?;
                self.merged_on_synced(&rec, state).await?
            }
            None => state.sync_from,
        };
        let net = self.diff(from, tip).await?;
        let target = if net.is_empty() {
            base_tip
        } else {
            let message = format!(
                "graft: net change of line {} since {from}",
                self.config.line
            );
            let squashed = self.create(vec![from], net, message, base_tip).await?;
            let cx = self.call("rebase");
            match self
                .api
                .rebase(
                    &cx,
                    Rebase {
                        repo: self.config.repo.clone(),
                        changeset: squashed,
                        onto: base_tip,
                        merge: MergeMode::Line,
                        dry_run: false,
                    },
                )
                .await
            {
                Ok(id) => id,
                Err(ApiError::Conflict { paths, onto }) => {
                    let paths: Vec<String> = paths
                        .into_iter()
                        .map(|p| clean_text(&p.path, 300, false))
                        .collect();
                    tracing::warn!(
                        repo = %self.config.repo, line = %self.config.line, onto = %onto,
                        conflicts = paths.len(),
                        "mirror batch: the line does not rebase onto the moved base; nothing moved"
                    );
                    return Ok(Err(BatchOutcome::Conflict { onto, paths }));
                }
                Err(e) => return Err(e.into()),
            }
        };
        let changes = self.diff(tip, target).await?;
        let moved = if changes.is_empty() {
            None
        } else {
            let message = format!(
                "graft: sync line {} onto {} {base_tip}",
                self.config.line, self.config.base.line
            );
            let merge = self
                .create(vec![tip, base_tip], changes, message, base_tip)
                .await?;
            let cx = self.call("update-ref");
            let repo = self.config.repo.clone();
            let line = self.config.line.clone();
            match move_ref(self.api, &cx, &repo, &line, Some(tip), merge, RefKind::Line).await {
                Ok(()) => {}
                Err(ApiError::RefConflict { .. }) => {
                    return Err(config_err(format!(
                        "line {line} moved during the sync; run pr-batch again"
                    )));
                }
                Err(e) => return Err(e.into()),
            }
            Some(merge)
        };
        state.synced = base_tip;
        state.sync_from = base_tip;
        state.sync_batch = None;
        self.db.set_batch_line(&self.config.line, state)?;
        tracing::info!(
            repo = %self.config.repo, line = %self.config.line, onto = %base_tip,
            moved = ?moved, "mirror batch: line synced onto the base"
        );
        Ok(Ok(moved))
    }

    /// The lands on the integration line after `start`, newest first, at
    /// most [`MAX_LANDS`], with whether the count stopped at the cap. Sync
    /// changesets are left out.
    async fn lands(
        &mut self,
        tip: ChangesetId,
        start: ChangesetId,
    ) -> Result<(Vec<ChangesetId>, bool), MirrorError> {
        let mut ids = Vec::new();
        let mut token = None;
        let capped = loop {
            let cx = self.call("log");
            let page = self
                .api
                .log(
                    &cx,
                    Log {
                        repo: self.config.repo.clone(),
                        from: tip,
                        until: Some(start),
                        paths: Vec::new(),
                        page: Page {
                            limit: PAGE,
                            token: token.take(),
                        },
                    },
                )
                .await?;
            ids.extend(page.changesets.iter().map(|c| c.id));
            if ids.len() > MAX_LANDS {
                ids.truncate(MAX_LANDS);
                break true;
            }
            match page.next {
                Some(t) => token = Some(t),
                None => break false,
            }
        };
        Ok((ids, capped))
    }

    /// Each land's latest result per check, from the journal's `Attested`
    /// entries at or after `from_lsn`. A land rebased at landing takes its
    /// draft's attestations. `true` when the read stopped at its cap.
    async fn attestations(
        &mut self,
        from_lsn: u64,
        wanted: &HashMap<ChangesetId, ChangesetId>,
    ) -> Result<(HashMap<ChangesetId, BTreeMap<String, bool>>, bool), MirrorError> {
        let mut out: HashMap<ChangesetId, BTreeMap<String, bool>> = HashMap::new();
        let mut lsn = from_lsn;
        for _ in 0..MAX_ATTEST_PAGES {
            let cx = self.call("attested");
            let page = self
                .api
                .watch(
                    &cx,
                    Watch {
                        repo: self.config.repo.clone(),
                        from_lsn: lsn,
                        kinds: vec!["Attested".to_owned()],
                        limit: PAGE,
                        wait_ms: 0,
                    },
                )
                .await?;
            for e in &page.entries {
                let Ok(a) = serde_json::from_str::<Attested>(&e.body) else {
                    continue;
                };
                let Ok(id) = ChangesetId::parse(&a.changeset) else {
                    continue;
                };
                if let Some(land) = wanted.get(&id) {
                    out.entry(*land)
                        .or_default()
                        .insert(clean_text(&a.check, FIELD_CHARS, false), a.passed);
                }
            }
            if page.next_lsn <= lsn {
                return Ok((out, false));
            }
            lsn = page.next_lsn;
        }
        Ok((out, true))
    }

    /// The lands among `ids` (newest first), sync changesets left out, with
    /// each one's latest check results; `true` when the attestation read
    /// stopped at its cap.
    async fn describe(
        &mut self,
        ids: &[ChangesetId],
        views: &HashMap<ChangesetId, ChangesetView>,
    ) -> Result<(Vec<LandLine>, bool), MirrorError> {
        let mut lands = Vec::new();
        // Each attested id (a land, or the draft it was rebased from) and
        // the land it counts for.
        let mut wanted = HashMap::new();
        let mut from_lsn = u64::MAX;
        for id in ids {
            let Some(v) = views.get(id) else { continue };
            let cs = &v.changeset;
            if is_sync(&cs.metadata) {
                continue;
            }
            from_lsn = from_lsn.min(v.lsn);
            wanted.insert(*id, *id);
            if let Some(draft) = cs
                .metadata
                .get("graft.rebased-from")
                .and_then(|d| ChangesetId::parse(d).ok())
            {
                wanted.insert(draft, *id);
            }
            let meta = |key: &str| {
                cs.metadata
                    .get(key)
                    .map(|v| clean_text(v, FIELD_CHARS, false))
            };
            lands.push(LandLine {
                id: *id,
                author: clean_text(&cs.author.name, FIELD_CHARS, false),
                session: meta(SESSION_KEY),
                work_item: meta(WORK_ITEM_KEY),
                subject: clean_text(
                    cs.message.lines().next().unwrap_or("").trim(),
                    FIELD_CHARS,
                    false,
                ),
                checks: BTreeMap::new(),
                authored_ms: cs.authored_at.ms,
                reverts: cs
                    .metadata
                    .get(REVERTS_KEY)
                    .and_then(|r| ChangesetId::parse(r).ok()),
                parent: cs.first_parent(),
                ident: (ident_part(&cs.author.name), ident_part(&cs.author.email)),
                authored: cs.authored_at,
                message: clean_text(&cs.message, MESSAGE_CHARS, true),
            });
        }
        if lands.is_empty() {
            return Ok((lands, false));
        }
        // Drafts rebased at landing were attested before the land existed:
        // read from the oldest of them too.
        let drafts: Vec<ChangesetId> = wanted
            .iter()
            .filter(|(d, l)| d != l)
            .map(|(d, _)| *d)
            .collect();
        for v in self.views(&drafts).await?.values() {
            from_lsn = from_lsn.min(v.lsn);
        }
        let (checks, truncated) = self.attestations(from_lsn, &wanted).await?;
        for land in &mut lands {
            if let Some(c) = checks.get(&land.id) {
                land.checks = c.clone();
            }
        }
        Ok((lands, truncated))
    }

    /// Reverts each land the operator named on the
    /// integration line, newest first, each a compare-and-swap move of the
    /// line, and records why. A land already reverted on the line is
    /// skipped. `Ok(Err(_))` on a revert that conflicts with later lands;
    /// the reverts before it stay.
    async fn drop_lands(
        &mut self,
        dropped: &mut Vec<(ChangesetId, ChangesetId)>,
    ) -> Result<Result<(), BatchOutcome>, MirrorError> {
        let base_tip = self.tip(&self.config.base.line.clone()).await?;
        let mut tip = self.tip(&self.config.line.clone()).await?;
        let state = self.line_state(tip, base_tip).await?;
        let (ids, _) = self.lands(tip, state.start).await?;
        let views = self.views(&ids).await?;
        // The lands (not sync changesets) and the ones already reverted.
        let mut lands = HashSet::new();
        let mut reverted = HashSet::new();
        for v in ids.iter().filter_map(|id| views.get(id)) {
            let meta = &v.changeset.metadata;
            if is_sync(meta) {
                continue;
            }
            lands.insert(v.changeset.id());
            if let Some(r) = meta
                .get(REVERTS_KEY)
                .and_then(|r| ChangesetId::parse(r).ok())
            {
                reverted.insert(r);
            }
        }
        let named = clean_text(
            self.config
                .drop_reason
                .as_deref()
                .unwrap_or(DEFAULT_DROP_REASON),
            FIELD_CHARS,
            false,
        );
        let mut targets: Vec<(ChangesetId, String)> = Vec::new();
        for id in self.config.exclude.clone() {
            if !lands.contains(&id) {
                return Err(config_err(format!(
                    "{id} is not a land in line {}'s next batch",
                    self.config.line
                )));
            }
            if !reverted.contains(&id) && !targets.iter().any(|(t, _)| *t == id) {
                targets.push((id, named.clone()));
            }
        }
        // Newest first, as the land list is.
        let order: HashMap<ChangesetId, usize> =
            ids.iter().enumerate().map(|(i, id)| (*id, i)).collect();
        targets.sort_by_key(|(id, _)| order.get(id).copied().unwrap_or(usize::MAX));
        for (land, reason) in targets {
            let cx = self.call("revert");
            let revert = match self
                .api
                .revert(
                    &cx,
                    Revert {
                        repo: self.config.repo.clone(),
                        changeset: land,
                        onto: tip,
                        workspace: None,
                    },
                )
                .await
            {
                Ok(id) => id,
                Err(ApiError::Conflict { paths, onto }) => {
                    let paths: Vec<String> = paths
                        .into_iter()
                        .map(|p| clean_text(&p.path, 300, false))
                        .collect();
                    tracing::warn!(
                        repo = %self.config.repo, line = %self.config.line, land = %land,
                        conflicts = paths.len(),
                        "mirror batch: the land does not revert cleanly on the line; not dropped"
                    );
                    return Ok(Err(BatchOutcome::Conflict { onto, paths }));
                }
                Err(e) => return Err(e.into()),
            };
            let repo = self.config.repo.clone();
            let line = self.config.line.clone();
            // The reason first: a run that dies after the move still names
            // it. A row whose revert never reached the line is overwritten
            // by the retry's revert.
            self.db.put_batch_drop(&line, land, revert, &reason)?;
            let cx = self.call("update-ref");
            match move_ref(
                self.api,
                &cx,
                &repo,
                &line,
                Some(tip),
                revert,
                RefKind::Line,
            )
            .await
            {
                Ok(()) => {}
                Err(ApiError::RefConflict { .. }) => {
                    return Err(config_err(format!(
                        "line {line} moved while dropping {land}; run pr-batch again"
                    )));
                }
                Err(e) => return Err(e.into()),
            }
            tracing::info!(
                repo = %repo, line = %line, land = %land, revert = %revert,
                "mirror batch: land dropped (reverted on the line)"
            );
            dropped.push((land, revert));
            tip = revert;
        }
        Ok(Ok(()))
    }

    /// The lands of open batch `rec` reverted on the line after its tip,
    /// each with its revert: what makes the batch stale. Reads the lands
    /// since the batch's tip, and the batch's own only when one of those is
    /// a revert.
    async fn stale_drops(
        &mut self,
        rec: &BatchRecord,
    ) -> Result<Vec<(ChangesetId, ChangesetId)>, MirrorError> {
        let tip = self.tip(&self.config.line.clone()).await?;
        if tip == rec.tip {
            return Ok(Vec::new());
        }
        let (newer, _) = self.lands(tip, rec.tip).await?;
        let views = self.views(&newer).await?;
        let mut reverts: Vec<(ChangesetId, ChangesetId)> = newer
            .iter()
            .filter_map(|id| {
                let target = views.get(id)?.changeset.metadata.get(REVERTS_KEY)?;
                Some((ChangesetId::parse(target).ok()?, *id))
            })
            .collect();
        if reverts.is_empty() {
            return Ok(reverts);
        }
        let (held, _) = self.lands(rec.tip, rec.start).await?;
        let held: HashSet<ChangesetId> = held.into_iter().collect();
        reverts.retain(|(land, _)| held.contains(land));
        reverts.reverse();
        Ok(reverts)
    }

    /// Closes an open batch whose lands changed under it (a land dropped),
    /// with a comment saying so, and rejects it: the next batch is cut from
    /// the same start. `false`, with nothing closed, when GitHub has it in
    /// the merge queue or set to auto-merge, or it merged meanwhile: the
    /// reverts then reach GitHub in the next batch.
    async fn supersede(
        &mut self,
        rec: &mut BatchRecord,
        dropped: &[(ChangesetId, ChangesetId)],
    ) -> Result<bool, MirrorError> {
        let Some(number) = rec.pr_number else {
            return Err(config_err("an open batch has no PR"));
        };
        let pull = self.forge.get_pull(number).await?;
        let busy = if pull.merged || !pull.open {
            Some("merged or closed on GitHub since this run read it")
        } else if self.forge.merging(&pull.node_id).await? {
            Some("in GitHub's merge queue or set to auto-merge")
        } else {
            None
        };
        if let Some(why) = busy {
            rec.detail =
                format!("not superseded: {why}; the dropped lands' reverts go in the next batch");
            self.db.put_batch(rec)?;
            tracing::warn!(
                line = %rec.line, batch = rec.number, pr = number,
                "mirror batch: a stale batch is merging; left as it is"
            );
            return Ok(false);
        }
        let names: Vec<String> = dropped
            .iter()
            .take(MAX_DROPS_LISTED)
            .map(|(l, _)| format!("`{}`", short(*l)))
            .collect();
        let detail = format!(
            "superseded: land{} {}{} dropped from line {} (reverted); recut as a new batch",
            if dropped.len() == 1 { "" } else { "s" },
            names.join(", "),
            if dropped.len() > MAX_DROPS_LISTED {
                format!(" and {} more", dropped.len() - MAX_DROPS_LISTED)
            } else {
                String::new()
            },
            clean_text(&rec.line, FIELD_CHARS, false)
        );
        self.forge.close_pr(number, &detail).await?;
        self.reject(rec, detail)?;
        Ok(true)
    }

    /// Builds and records the next batch: the land list, the cut, the
    /// squashed commit in the local mirror, then the record. `Ok(None)` when
    /// the line holds nothing the base lacks, `Err(NotDue)` when the batch
    /// is held for size or age.
    async fn plan(
        &mut self,
        state: &BatchLine,
        tip: ChangesetId,
        base_tip: ChangesetId,
    ) -> Result<Result<Option<BatchRecord>, BatchOutcome>, MirrorError> {
        let tip_view = self.view(tip).await?;
        let base_view = self.view(base_tip).await?;
        if tip_view.changeset.root == base_view.changeset.root {
            return Ok(Ok(None));
        }
        let (ids, capped) = self.lands(tip, state.start).await?;
        let views = self.views(&ids).await?;
        let (mut lands, truncated) = self.describe(&ids, &views).await?;
        if lands.is_empty() {
            return Ok(Ok(None));
        }
        // A batch carries at most MAX_COMMITS per-land commits (D26), so the
        // cut always applies that cap.
        let max_lands = self
            .config
            .max_lands
            .unwrap_or(MAX_COMMITS)
            .min(MAX_COMMITS);
        if let Some(age) = self.config.max_age_ms {
            let oldest_ms = lands.iter().map(|l| l.authored_ms).min().unwrap_or(0);
            let full = lands.len() >= max_lands;
            if !full && (self.clock)().saturating_sub(oldest_ms) < age {
                return Ok(Err(BatchOutcome::NotDue {
                    lands: lands.len(),
                    oldest_ms,
                }));
            }
        }
        let mut uncut = None;
        let (cut, base, held) = match Some(max_lands) {
            Some(max) if lands.len() > max.max(1) && capped => {
                uncut = Some(format!(
                    "the line has more than {MAX_LANDS} lands to batch, so the oldest are \
                     not read and the {max}-land cap is not applied: this batch takes them all"
                ));
                (tip, base_tip, 0)
            }
            Some(max) if lands.len() > max.max(1) => {
                let keep = keep_with_reverts(&lands, max.max(1));
                let cut = lands[lands.len() - keep].id;
                // The base the line held at the cut: the newest sync at or
                // before it, else the base at the batch start.
                let pos = ids.iter().position(|i| *i == cut).unwrap_or(0);
                let held_base = ids[pos..]
                    .iter()
                    .filter_map(|i| views.get(i))
                    .find_map(|v| v.changeset.metadata.get(SYNC_KEY))
                    .and_then(|b| ChangesetId::parse(b).ok())
                    .or(state.start_base);
                match held_base {
                    Some(base) => {
                        let held = lands.len() - keep;
                        lands.drain(..held);
                        (cut, base, held)
                    }
                    None => {
                        uncut = Some(format!(
                            "the line's record predates batch cuts, so the base at the cut \
                             is unknown and the {max}-land cap is not applied: this batch \
                             takes them all"
                        ));
                        (tip, base_tip, 0)
                    }
                }
            }
            _ => (tip, base_tip, 0),
        };
        if let Some(why) = &uncut {
            tracing::warn!(
                repo = %self.config.repo, line = %self.config.line, "mirror batch: {why}"
            );
        }
        let base_sha = self.db.sha_of(base)?.ok_or_else(|| {
            config_err(format!(
                "base {base} has no git commit: follow {} first",
                self.config.base.branch
            ))
        })?;
        let in_batch: HashMap<ChangesetId, ChangesetId> = lands
            .iter()
            .filter_map(|l| l.reverts.map(|r| (r, l.id)))
            .collect();
        let mut drops = Vec::new();
        for l in lands.iter().rev() {
            if let Some(revert) = in_batch.get(&l.id) {
                let reason = match self.db.batch_drop_reason(&self.config.line, l.id)? {
                    Some(r) => clean_text(&r, FIELD_CHARS, false),
                    None => "reverted on the line".to_owned(),
                };
                drops.push(Dropped {
                    land: l.id,
                    revert: *revert,
                    reason,
                });
            }
        }

        let number = self.db.next_batch_number()?;
        let created_ms = (self.clock)();
        let branch = format!("{BRANCH_PREFIX}{number}");
        let line = clean_text(&self.config.line, FIELD_CHARS, false);
        let title = clean_text(
            &format!(
                "graft batch {number}: {} land{} from {line}",
                lands.len(),
                if lands.len() == 1 { "" } else { "s" }
            ),
            TITLE_CHARS,
            false,
        );
        // The tree: the integration tip, exported deterministically.
        let tree = {
            let cx = self.call("export");
            let stamp = Stamp(created_ms);
            let repo = self.config.repo.clone();
            let committer = self.config.committer.clone();
            let mut exporter = Exporter::new(
                self.api,
                &cx,
                self.db,
                &repo,
                &committer,
                &stamp,
                self.mirror.dir(),
            )?;
            let tip_sha = exporter.export_changeset(cut).await?;
            self.mirror.commit_tree(tip_sha).await?
        };
        let (fallback, commits) = match self
            .chain(number, base, base_sha, tree, created_ms, &lands)
            .await?
        {
            Ok(commits) => (None, commits),
            Err(why) => {
                tracing::warn!(
                    repo = %self.config.repo, line = %self.config.line, batch = number,
                    "mirror batch: exported squashed: {why}"
                );
                (Some(why), Vec::new())
            }
        };
        let (body, message_body) = render(
            &BatchHeader {
                number,
                line: self.config.line.clone(),
                branch: self.config.base.branch.clone(),
                base,
                base_sha,
                start: state.start,
                tip: cut,
                capped,
                truncated,
                drops,
                held,
                uncut,
                max_lands,
                fallback: fallback.clone(),
            },
            &lands,
        );
        let squash_sha = match commits.last() {
            Some(c) => c.sha,
            None => {
                let ident = format!(
                    "{} <{}> {}",
                    self.config.committer.name,
                    self.config.committer.email,
                    git_time(created_ms)
                );
                let record = ImportedCommit {
                    sha: GitSha([0; 20]),
                    headers: format!("author {ident}\ncommitter {ident}\n\n").into_bytes(),
                    message: None,
                    raw_trees: Vec::new(),
                };
                let message = format!("{title}\n\n{message_body}");
                let raw = commit_object(tree, &[base_sha], &record, &message);
                self.mirror.write_commit(&raw).await?
            }
        };

        let rec = BatchRecord {
            number,
            line: self.config.line.clone(),
            base_branch: self.config.base.branch.clone(),
            base,
            base_sha,
            start: state.start,
            tip: cut,
            lands: lands.len() as u64,
            created_ms,
            branch,
            title,
            body,
            squash_sha,
            pr_number: None,
            pr_url: None,
            state: BatchState::Planned,
            detail: String::new(),
            merge_sha: None,
            merged: None,
        };
        if !self.db.insert_batch(&rec, &commits)? {
            return Err(config_err(format!(
                "another run opened a batch for line {} first",
                self.config.line
            )));
        }
        tracing::info!(
            repo = %self.config.repo, line = %self.config.line, batch = number,
            lands = rec.lands, held, commits = commits.len(), head = %squash_sha,
            "mirror batch planned"
        );
        Ok(Ok(Some(rec)))
    }

    /// The batch's per-land commits on `base_sha`, oldest first, written to
    /// the mirror (D26). Each land's own change is replayed onto the base as
    /// an unlanded changeset, and its tree is the land's commit's tree; the
    /// last tree must be `tree`, the cut tip's. `Ok(Err(why))` when the
    /// batch is to be squashed instead. One diff, one create and one export
    /// per land, at most [`MAX_COMMITS`].
    async fn chain(
        &mut self,
        number: u64,
        base: ChangesetId,
        base_sha: GitSha,
        tree: GitSha,
        created_ms: i64,
        lands: &[LandLine],
    ) -> Result<Result<Vec<BatchCommit>, String>, MirrorError> {
        if lands.len() > MAX_COMMITS {
            return Ok(Err(format!(
                "the batch has {} lands, more than the {MAX_COMMITS} per-land commits a \
                 batch carries",
                lands.len()
            )));
        }
        // Oldest first: each replay sits on the one before it.
        let mut replays = Vec::with_capacity(lands.len());
        let mut prev = base;
        for land in lands.iter().rev() {
            let Some(parent) = land.parent else {
                return Ok(Err(format!("land {} has no parent", short(land.id))));
            };
            let changes = self.diff(parent, land.id).await?;
            if !changes.is_empty() {
                let mut metadata = Metadata::new();
                metadata.insert(REPLAY_KEY.to_owned(), land.id.to_string());
                let message = format!("graft: replay land {} for batch {number}", land.id);
                prev = match self
                    .create_with(vec![prev], changes, message, metadata, land.authored)
                    .await
                {
                    Ok(id) => id,
                    Err(ApiError::InvalidChange { .. } | ApiError::InvalidPath { .. }) => {
                        return Ok(Err(format!(
                            "land {} does not replay onto the base",
                            short(land.id)
                        )));
                    }
                    Err(e) => return Err(e.into()),
                };
            }
            replays.push(prev);
        }
        let mut trees = Vec::with_capacity(replays.len());
        {
            let cx = self.call("export-chain");
            let stamp = Stamp(created_ms);
            let repo = self.config.repo.clone();
            let committer = self.config.committer.clone();
            let mut exporter = Exporter::new(
                self.api,
                &cx,
                self.db,
                &repo,
                &committer,
                &stamp,
                self.mirror.dir(),
            )?;
            for r in &replays {
                let sha = exporter.export_changeset(*r).await?;
                trees.push(self.mirror.commit_tree(sha).await?);
            }
        }
        if trees.last() != Some(&tree) {
            return Ok(Err(
                "replaying the lands onto the base does not reproduce the integration tip"
                    .to_owned(),
            ));
        }
        let mut parent = base_sha;
        let mut out = Vec::with_capacity(trees.len());
        for (seq, (land, tree)) in lands.iter().rev().zip(&trees).enumerate() {
            let raw = land_commit(number, land, *tree, parent, &self.config.committer);
            let sha = self.mirror.write_commit(&raw).await?;
            out.push(BatchCommit {
                seq: seq as u32,
                land: land.id,
                sha,
                merged: None,
            });
            parent = sha;
        }
        Ok(Ok(out))
    }

    /// Where each of batch `rec`'s per-land commits reached the base
    /// branch, given the PR's merge commit `merge` (imported by follow, so
    /// in the mirror). A rebase-merge leaves one new commit per land, newest
    /// at `merge`, each naming its land in its only `Graft-Changeset`
    /// trailer; a merge commit has the branch head as its second parent and
    /// keeps the commits as they are. A squash-merge maps nothing. Reads at
    /// most one commit per land.
    async fn map_merged(
        &self,
        rec: &BatchRecord,
        merge: GitSha,
    ) -> Result<Vec<BatchCommit>, MirrorError> {
        let mut commits = self.db.batch_commits(rec.number)?;
        if commits.is_empty() {
            return Ok(commits);
        }
        let Ok(raw) = self.mirror.read_commit(merge).await else {
            return Ok(Vec::new());
        };
        let (parents, _) = parse_commit(&raw);
        if parents.len() == 2 && parents[1] == rec.squash_sha {
            for c in &mut commits {
                c.merged = Some(c.sha);
            }
            return Ok(commits);
        }
        let at: HashMap<ChangesetId, usize> = commits
            .iter()
            .enumerate()
            .map(|(i, c)| (c.land, i))
            .collect();
        let mut cur = merge;
        for _ in 0..commits.len() {
            let Ok(raw) = self.mirror.read_commit(cur).await else {
                break;
            };
            let (parents, land) = parse_commit(&raw);
            let Some(i) = land.and_then(|l| at.get(&l).copied()) else {
                break;
            };
            commits[i].merged = Some(cur);
            match parents.first() {
                Some(p) => cur = *p,
                None => break,
            }
        }
        Ok(commits)
    }

    /// Runs whichever step the batch is at, until it waits on the operator
    /// or is final.
    async fn advance(&mut self, rec: &mut BatchRecord) -> Result<(), MirrorError> {
        if rec.state == BatchState::Planned {
            self.push(rec).await?;
        }
        if rec.state == BatchState::Pushed {
            self.open(rec).await?;
        }
        if rec.state == BatchState::Open {
            self.observe(rec).await?;
        }
        Ok(())
    }

    /// Pushes the batch's head commit to `graft/batch-<n>`, on a lease that
    /// requires the branch not to exist. The same commit already there is
    /// fine (a retry); other commits are not overwritten, and the batch is
    /// rejected.
    async fn push(&mut self, rec: &mut BatchRecord) -> Result<(), MirrorError> {
        let token = if self.mirror.is_https() {
            Some(self.forge.token().await?)
        } else {
            None
        };
        let bearer = token.as_ref().map(SecretToken::expose);
        let outcome = self
            .mirror
            .push_lease(&rec.branch, rec.squash_sha, None, bearer)
            .await?;
        if outcome == PushOutcome::Rejected {
            let there = self.mirror.ls_remote(&[rec.branch.as_str()]).await?;
            if there.first().and_then(|(_, s)| *s) != Some(rec.squash_sha) {
                return self.reject(
                    rec,
                    format!(
                        "branch {} exists with other commits; not overwritten",
                        rec.branch
                    ),
                );
            }
        }
        rec.state = BatchState::Pushed;
        self.db.put_batch(rec)?;
        Ok(())
    }

    /// Opens the PR, or adopts the one already on the branch. Never merges
    /// or queues it.
    async fn open(&mut self, rec: &mut BatchRecord) -> Result<(), MirrorError> {
        let pull = match self.forge.find_pull(&rec.branch).await? {
            Some(pull) => pull,
            None => {
                let new = crate::github::NewPull {
                    title: rec.title.clone(),
                    body: rec.body.clone(),
                    head: rec.branch.clone(),
                    base: rec.base_branch.clone(),
                };
                self.forge.create_pull(&new).await?
            }
        };
        self.record_pull(rec, &pull);
        rec.state = BatchState::Open;
        rec.detail = "waiting for the operator to merge".to_owned();
        self.db.put_batch(rec)?;
        tracing::info!(
            line = %rec.line, batch = rec.number, pr = pull.number,
            "mirror batch: PR open"
        );
        Ok(())
    }

    fn record_pull(&self, rec: &mut BatchRecord, pull: &Pull) {
        rec.pr_number = Some(pull.number);
        rec.pr_url = Some(clean_text(&pull.url, 300, false));
    }

    /// Reads the PR. Merged, with its merge commit imported by follow: the
    /// batch is merged, and the next one counts from its tip. Closed
    /// unmerged: rejected, and the line is left as it is.
    async fn observe(&mut self, rec: &mut BatchRecord) -> Result<(), MirrorError> {
        let number = rec
            .pr_number
            .ok_or_else(|| config_err("an open batch has no PR"))?;
        let pull = self.forge.get_pull(number).await?;
        self.record_pull(rec, &pull);
        if pull.merged {
            let merge = pull
                .merge_commit
                .as_deref()
                .and_then(GitSha::from_hex)
                .ok_or_else(|| config_err("GitHub merged the PR without a merge commit"))?;
            rec.merge_sha = Some(merge);
            let Some(merged) = self.db.changeset_of(merge)? else {
                rec.detail = format!("merged on GitHub as {merge}; waiting for follow");
                self.db.put_batch(rec)?;
                return Ok(());
            };
            let commits = self.map_merged(rec, merge).await?;
            rec.merged = Some(merged);
            rec.state = BatchState::Merged;
            rec.detail = format!("merged as {merge} (graft {merged})");
            // The merge commit's snapshot is the batch tip's (plus whatever
            // GitHub's main gained meanwhile), so the next sync diffs from
            // the tip and the next batch counts lands after it.
            let state = self
                .db
                .batch_line(&rec.line)?
                .ok_or_else(|| config_err(format!("line {} has no batch record", rec.line)))?;
            // A batch cut before the line's newest sync: the next sync
            // rebuilds its starting point from this batch (see
            // `merged_on_synced`) rather than keep an unlanded changeset
            // across runs, which nothing would protect from collection.
            let cut_before_sync = rec.base != state.synced;
            let next = BatchLine {
                synced: state.synced,
                sync_from: if cut_before_sync {
                    state.sync_from
                } else {
                    rec.tip
                },
                start: rec.tip,
                start_base: Some(rec.base),
                sync_batch: cut_before_sync.then_some(rec.number),
            };
            self.db.settle_batch(rec, &rec.line, &next, &commits)?;
            tracing::info!(
                line = %rec.line, batch = rec.number, merge = %merge,
                "mirror batch merged"
            );
            return Ok(());
        }
        if !pull.open {
            return self.reject(rec, format!("closed without merging: {}", pull.url));
        }
        if pull.head_sha != rec.squash_sha.to_hex() {
            rec.detail = format!(
                "waiting for the operator; the branch was changed outside graft (head {})",
                clean_text(&pull.head_sha, 40, false)
            );
        }
        self.db.put_batch(rec)?;
        Ok(())
    }

    /// The sync point after merging batch `rec`, cut before the line's
    /// newest sync: the batch's change rebased onto the base the line
    /// absorbed since. Built by the sync that uses it, from landed
    /// changesets only (`rec.base`, `rec.tip`, `state.synced`), so nothing
    /// unlanded outlives the run. The line past the cut holds that base plus the
    /// batch, so the next sync diffs only the lands after the batch, and
    /// none of the merged ones is applied again. When the batch does not
    /// rebase onto that base (the line's own sync merged it), the old sync
    /// point is kept and the next sync says where it conflicts.
    async fn merged_on_synced(
        &mut self,
        rec: &BatchRecord,
        state: &BatchLine,
    ) -> Result<ChangesetId, MirrorError> {
        let net = self.diff(rec.base, rec.tip).await?;
        if net.is_empty() {
            return Ok(state.synced);
        }
        let message = format!(
            "graft: batch {} of line {} on {}",
            rec.number, rec.line, state.synced
        );
        let squashed = self
            .create(vec![rec.base], net, message, state.synced)
            .await?;
        let cx = self.call("rebase");
        match self
            .api
            .rebase(
                &cx,
                Rebase {
                    repo: self.config.repo.clone(),
                    changeset: squashed,
                    onto: state.synced,
                    merge: MergeMode::Line,
                    dry_run: false,
                },
            )
            .await
        {
            Ok(id) => Ok(id),
            Err(ApiError::Conflict { paths, .. }) => {
                tracing::warn!(
                    line = %rec.line, batch = rec.number, conflicts = paths.len(),
                    "mirror batch: the merged batch does not rebase onto the line's base; \
                     the next sync starts from the old base"
                );
                Ok(state.sync_from)
            }
            Err(e) => Err(e.into()),
        }
    }

    fn reject(&self, rec: &mut BatchRecord, detail: String) -> Result<(), MirrorError> {
        let detail = clean_text(&detail, 500, false);
        tracing::warn!(
            line = %rec.line, batch = rec.number, detail = %detail,
            "mirror batch rejected; the line is left as it is"
        );
        rec.state = BatchState::Rejected;
        rec.detail = detail;
        self.db.put_batch(rec)?;
        Ok(())
    }
}

/// What a batch's PR body and commit message say before the land list.
struct BatchHeader {
    number: u64,
    line: String,
    branch: String,
    base: ChangesetId,
    base_sha: GitSha,
    start: ChangesetId,
    tip: ChangesetId,
    /// The land count stopped at [`MAX_LANDS`].
    capped: bool,
    /// The attestation read stopped at its cap.
    truncated: bool,
    /// Lands dropped from the batch (each reverted in it), oldest first.
    drops: Vec<Dropped>,
    /// Newer lands left for the next batch by the cut.
    held: usize,
    /// Why a configured cap was not applied.
    uncut: Option<String>,
    /// The cut's land cap.
    max_lands: usize,
    /// Why the batch is one squashed commit instead of one per land.
    fallback: Option<String>,
}

/// How many of `lands` (newest first) a batch capped at `max` takes, oldest
/// first: `max`, or more so no dropped land is split from its revert.
fn keep_with_reverts(lands: &[LandLine], max: usize) -> usize {
    let mut keep = max.min(lands.len());
    loop {
        let first_kept = lands.len() - keep;
        let kept: HashSet<ChangesetId> = lands[first_kept..].iter().map(|l| l.id).collect();
        // The oldest revert, among the held lands, of a land in the batch.
        let Some(i) = lands[..first_kept]
            .iter()
            .rposition(|l| l.reverts.is_some_and(|r| kept.contains(&r)))
        else {
            return keep;
        };
        keep = lands.len() - i;
    }
}

/// A land's latest check results, bounded: `none`, or `<check> passed`
/// and `<check> failed`, comma-separated.
fn checks_text(land: &LandLine) -> String {
    let mut checks: Vec<String> = land
        .checks
        .iter()
        .take(MAX_CHECKS)
        .map(|(c, ok)| {
            let c = clean_text(c, FIELD_CHARS, false);
            format!("{c} {}", if *ok { "passed" } else { "failed" })
        })
        .collect();
    if land.checks.len() > MAX_CHECKS {
        checks.push(format!("+{} more", land.checks.len() - MAX_CHECKS));
    }
    if checks.is_empty() {
        "none".to_owned()
    } else {
        checks.join(", ")
    }
}

/// One table row for a land. Every cell is bounded, so a row is too.
fn land_row(land: &LandLine) -> String {
    let cell = |s: &str| clean_text(s, FIELD_CHARS, false).replace('|', "\\|");
    let checks = checks_text(land);
    format!(
        "| `{}` | {} | {} | {} | {} | {} |\n",
        short(land.id),
        cell(&land.author),
        cell(land.session.as_deref().unwrap_or("-")),
        cell(land.work_item.as_deref().unwrap_or("-")),
        checks.replace('|', "\\|"),
        cell(&land.subject),
    )
}

/// A land's per-land commit message (D26): its own message (or a stand-in
/// when it has none), then the `Graft-*` trailers.
fn land_message(number: u64, land: &LandLine) -> String {
    let text = land.message.trim();
    let mut m = if text.is_empty() {
        format!("graft land {}", short(land.id))
    } else {
        text.to_owned()
    };
    m.push_str(&format!("\n\n{CHANGESET_TRAILER} {}\n", land.id));
    if let Some(s) = &land.session {
        m.push_str(&format!("Graft-Session: {s}\n"));
    }
    if let Some(w) = &land.work_item {
        m.push_str(&format!("Graft-Work-Item: {w}\n"));
    }
    m.push_str(&format!("Graft-Checks: {}\n", checks_text(land)));
    m.push_str(&format!("{BATCH_TRAILER} {number}\n"));
    m
}

/// A land's per-land commit object on `parent`: the land's author and
/// author time, `committer` at the same time, and [`land_message`]. Built
/// from recorded fields only, so the same land on the same parent gives the
/// same sha.
fn land_commit(
    number: u64,
    land: &LandLine,
    tree: GitSha,
    parent: GitSha,
    committer: &Committer,
) -> Vec<u8> {
    let when = git_time_at(land.authored);
    let headers = format!(
        "author {} <{}> {when}\ncommitter {} <{}> {when}\n\n",
        land.ident.0, land.ident.1, committer.name, committer.email
    );
    let record = ImportedCommit {
        sha: GitSha([0; 20]),
        headers: headers.into_bytes(),
        message: None,
        raw_trees: Vec::new(),
    };
    commit_object(tree, &[parent], &record, &land_message(number, land))
}

/// A raw commit's parents, and the land its message names when it has
/// exactly one `Graft-Changeset` trailer line (a squash-merge of several
/// lands names more and maps to none).
fn parse_commit(raw: &[u8]) -> (Vec<GitSha>, Option<ChangesetId>) {
    let text = String::from_utf8_lossy(raw);
    let (head, message) = text.split_once("\n\n").unwrap_or((text.as_ref(), ""));
    let parents = head
        .lines()
        .filter_map(|l| l.strip_prefix("parent "))
        .filter_map(GitSha::from_hex)
        .collect();
    let mut named = message
        .lines()
        .filter_map(|l| l.strip_prefix(CHANGESET_TRAILER))
        .map(str::trim);
    let land = match (named.next(), named.next()) {
        (Some(id), None) => ChangesetId::parse(id).ok(),
        _ => None,
    };
    (parents, land)
}

/// The PR body and the squashed commit's message body. Both say what the
/// batch is, list its lands (newest first) and end with the trailers. The
/// commit message lists every land. The PR body lists as many as fit in
/// [`MAX_PR_BODY`] and points to the commit message for the rest.
fn render(h: &BatchHeader, lands: &[LandLine]) -> (String, String) {
    let line = clean_text(&h.line, FIELD_CHARS, false);
    let branch = clean_text(&h.branch, FIELD_CHARS, false);
    let mut head = match &h.fallback {
        None => format!(
            "Batch {} of graft line `{line}`, one commit per land, oldest first, by \
             graft-mirror (decision D26). Merge it with rebase-merge to keep the commits \
             (D28). graft never merges or queues this PR.\n\n",
            h.number
        ),
        Some(why) => format!(
            "Batch {} of graft line `{line}`, squashed into one commit by graft-mirror \
             (the D26 fallback: {}). graft never merges or queues this PR.\n\n",
            h.number,
            clean_text(why, 300, false)
        ),
    };
    head.push_str(&format!(
        "- Base: `{branch}` at {} (graft {})\n",
        h.base_sha, h.base
    ));
    head.push_str(&format!(
        "- Lands: {}{} ({}..{})\n",
        lands.len(),
        if h.capped { "+" } else { "" },
        h.start,
        h.tip
    ));
    if h.capped {
        head.push_str(&format!(
            "- Only the newest {MAX_LANDS} lands are listed; the batch has more\n"
        ));
    }
    if h.truncated {
        head.push_str(
            "- Attestations: the journal read stopped at its cap; the list may be incomplete\n",
        );
    }
    if h.held > 0 {
        head.push_str(&format!(
            "- Cut at {} lands: {} newer {} for the next batch\n",
            h.max_lands,
            h.held,
            if h.held == 1 {
                "land waits"
            } else {
                "lands wait"
            }
        ));
    }
    if let Some(why) = &h.uncut {
        head.push_str(&format!("- Not cut: {why}\n"));
    }
    if !h.drops.is_empty() {
        head.push_str(&format!(
            "- Dropped: {} land{}, each reverted on the line (both are in this batch, so \
             they cancel out)\n",
            h.drops.len(),
            if h.drops.len() == 1 { "" } else { "s" }
        ));
        for d in h.drops.iter().take(MAX_DROPS_LISTED) {
            head.push_str(&format!(
                "  - `{}` reverted by `{}`: {}\n",
                short(d.land),
                short(d.revert),
                clean_text(&d.reason, FIELD_CHARS, false).replace('|', "\\|")
            ));
        }
        if h.drops.len() > MAX_DROPS_LISTED {
            head.push_str(&format!(
                "  - and {} more\n",
                h.drops.len() - MAX_DROPS_LISTED
            ));
        }
    }
    head.push_str("\n| Changeset | Author | Session | Work item | Attestations | Subject |\n");
    head.push_str("| --- | --- | --- | --- | --- | --- |\n");
    let trailers = format!(
        "\n{BATCH_TRAILER} {}\nGraft-Line: {line}\nGraft-Changeset: {}\n",
        h.number, h.tip
    );
    let rows: Vec<String> = lands.iter().map(land_row).collect();

    let mut message = head.clone();
    for r in &rows {
        message.push_str(r);
    }
    message.push_str(&trailers);

    // The note for lands the PR body has no room for, at its longest.
    let note = |n: usize| match h.fallback {
        None => format!(
            "\n{n} more lands are on the branch, one commit each \
             (`git log {}..{BRANCH_PREFIX}{}`).\n",
            h.base_sha, h.number
        ),
        Some(_) => format!(
            "\n{n} more lands are listed in the squashed commit's message \
             (`git log -1 {BRANCH_PREFIX}{}`).\n",
            h.number
        ),
    };
    let reserve = note(lands.len()).chars().count() + trailers.chars().count();
    let mut body = head;
    let mut len = body.chars().count();
    let mut listed = 0;
    for r in &rows {
        let n = r.chars().count();
        if len + n + reserve > MAX_PR_BODY {
            break;
        }
        body.push_str(r);
        len += n;
        listed += 1;
    }
    if listed < rows.len() {
        body.push_str(&note(rows.len() - listed));
    }
    body.push_str(&trailers);
    (body, message)
}

/// Whether a changeset with `metadata` is a sync changeset the mirror made.
pub fn is_sync(metadata: &Metadata) -> bool {
    metadata.contains_key(SYNC_KEY)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn states_round_trip() {
        for s in [
            BatchState::Planned,
            BatchState::Pushed,
            BatchState::Open,
            BatchState::Merged,
            BatchState::Rejected,
        ] {
            assert_eq!(BatchState::parse(s.as_str()), Some(s));
        }
        assert!(BatchState::Merged.is_terminal());
        assert!(!BatchState::Open.is_terminal());
        assert_eq!(BatchState::parse("landed"), None);
    }

    fn header(capped: bool) -> BatchHeader {
        let id = ChangesetId(graft_model::ObjectId([7; 32]));
        BatchHeader {
            number: 9,
            line: "integration".to_owned(),
            branch: "main".to_owned(),
            base: id,
            base_sha: GitSha([1; 20]),
            start: id,
            tip: id,
            capped,
            truncated: false,
            drops: Vec::new(),
            held: 0,
            uncut: None,
            max_lands: 0,
            fallback: Some("test".to_owned()),
        }
    }

    fn land(n: u32, long: bool) -> LandLine {
        let mut bytes = [0u8; 32];
        bytes[..4].copy_from_slice(&n.to_be_bytes());
        let text = |s: &str| if long { s.repeat(200) } else { s.to_owned() };
        let mut checks = BTreeMap::new();
        for c in 0..if long { 40 } else { 1 } {
            checks.insert(text(&format!("ci/check-{c}|")), c % 2 == 0);
        }
        LandLine {
            id: ChangesetId(graft_model::ObjectId(bytes)),
            author: text("Tess|"),
            session: Some(text("ses_x")),
            work_item: Some(text("WI-1")),
            subject: text("subject "),
            checks,
            authored_ms: n as i64,
            reverts: None,
            parent: None,
            ident: ("Tess".to_owned(), "tess@example.com".to_owned()),
            authored: Timestamp {
                ms: 1_700_000_000_000 + i64::from(n),
                offset_min: 600,
            },
            message: text("subject "),
        }
    }

    #[test]
    fn a_small_batch_lists_every_land_in_both() {
        let lands: Vec<LandLine> = (0..3).map(|n| land(n, false)).collect();
        let (body, message) = render(&header(false), &lands);
        assert_eq!(body, message);
        for l in &lands {
            assert!(body.contains(&short(l.id)));
        }
        assert!(body.ends_with(&format!("Graft-Changeset: {}\n", header(false).tip)));
        assert!(body.contains("Graft-Batch: 9\n"));
    }

    #[test]
    fn a_large_batch_fits_github_and_the_commit_lists_every_land() {
        let lands: Vec<LandLine> = (0..MAX_LANDS as u32).map(|n| land(n, true)).collect();
        let (body, message) = render(&header(true), &lands);
        assert!(
            body.chars().count() <= MAX_PR_BODY,
            "{}",
            body.chars().count()
        );
        assert!(body.chars().count() < 65_536);
        assert!(body.contains("more lands are listed in the squashed commit's message"));
        assert!(body.contains("Graft-Batch: 9\nGraft-Line: integration\n"));
        assert!(body.contains("Only the newest 5000 lands are listed"));
        let rows = |text: &str| -> Vec<String> {
            text.lines()
                .filter(|l| l.starts_with("| `c"))
                .map(|l| l[3..16].to_owned())
                .collect()
        };
        let expected: Vec<String> = lands.iter().map(|l| short(l.id)).collect();
        assert_eq!(
            rows(&message),
            expected,
            "the commit lists every land in order"
        );
        // Cells are bounded and cannot break the table.
        let row = land_row(&lands[0]);
        assert_eq!(row.matches(" | ").count(), 5, "{row}");
        assert!(row.contains("+32 more"));
        assert!(row.chars().count() < 2000, "{}", row.chars().count());
        // The listed rows plus the note account for every land.
        let listed = rows(&body);
        assert!(!listed.is_empty());
        assert_eq!(listed[..], expected[..listed.len()]);
        let note = format!("\n{} more lands are listed", MAX_LANDS - listed.len());
        assert!(body.contains(&note), "{note}");
    }

    #[test]
    fn a_land_commit_is_deterministic_and_names_its_land() {
        let l = land(4, false);
        let committer = Committer {
            name: "graft-mirror".to_owned(),
            email: "mirror@graft".to_owned(),
        };
        let raw = land_commit(3, &l, GitSha([2; 20]), GitSha([1; 20]), &committer);
        assert_eq!(
            raw,
            land_commit(3, &l, GitSha([2; 20]), GitSha([1; 20]), &committer)
        );
        let text = String::from_utf8(raw.clone()).unwrap();
        assert!(
            text.contains("author Tess <tess@example.com> 1700000000 +1000\n"),
            "{text}"
        );
        assert!(text.contains("committer graft-mirror <mirror@graft> 1700000000 +1000\n"));
        assert!(text.ends_with("Graft-Checks: ci/check-0| passed\nGraft-Batch: 3\n"));
        assert_eq!(parse_commit(&raw), (vec![GitSha([1; 20])], Some(l.id)));
        // A squash-merge message naming two lands maps to none.
        let two = format!("{text}\n{CHANGESET_TRAILER} {}\n", land(5, false).id);
        assert_eq!(parse_commit(two.as_bytes()).1, None);
    }

    #[test]
    fn idents_and_offsets_are_safe() {
        assert_eq!(ident_part("Ev<il>\nName"), "EvilName");
        assert_eq!(ident_part(" \n"), "unknown");
        let t = Timestamp {
            ms: 1_000,
            offset_min: -330,
        };
        assert_eq!(git_time_at(t), "1 -0530");
    }

    #[test]
    fn dates_are_utc_seconds() {
        assert_eq!(git_time(1_700_000_000_999), "1700000000 +0000");
    }
}
