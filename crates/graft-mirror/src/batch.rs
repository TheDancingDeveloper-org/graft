//! Batch mode (decision D21, `docs/architecture.md` §3c): agents land on a
//! graft **integration line** forked from the followed GitHub `main`, and
//! the batch reaches GitHub as **one squashed PR** that the operator merges.
//! GitHub CI runs once per batch.
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
//! 4. **Export** the line's net change against the base as one commit on
//!    GitHub's current `main`: the integration tip's git tree (written by
//!    `graft-git`'s deterministic exporter) with `main` as the only parent.
//!    The commit object is built byte for byte from recorded fields
//!    (`graft_git::export::commit_object`), stored in the local mirror and
//!    recorded before anything is pushed, so a retry pushes the same sha.
//! 5. **Push** branch `graft/batch-<n>` with a lease that requires the
//!    branch not to exist, and **open a PR** into the base (adopting one
//!    already on the branch). The squashed commit's message lists every
//!    land in the batch (changeset, author, session, work item,
//!    attestations); the PR body lists as many as fit under
//!    [`MAX_PR_BODY`] and names the commit message for the rest. Both end
//!    with `Graft-Batch` trailers. The mirror never merges or queues it.
//!
//! Costs follow the change: the sync reads one diff and one rebase; the land
//! list is a first-parent `Log` from the tip back to the batch start, capped
//! at [`MAX_LANDS`]; attestations come from a bounded read of the journal's
//! `Attested` entries from the oldest land in the batch, capped at
//! [`MAX_ATTEST_PAGES`] pages and marked as truncated beyond that.

use std::collections::{BTreeMap, HashMap};

use graft_git::GitSha;
use graft_git::export::{Committer, commit_object};
use graft_git::exporter::{Exporter, LandTimes, SESSION_KEY, WORK_ITEM_KEY};
use graft_git::map::{ImportedCommit, MirrorMap};
use graft_model::{Author, AuthorKind, Change, ChangesetId, Metadata, Timestamp};
use graft_proto::api::{AdminApi, ObjectApi, RepoApi};
use graft_proto::types::{
    ChangesetView, CreateChangeset, Diff, GetChangesets, Log, MergeMode, Page, Pair, Rebase,
    RefKind, Watch,
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
    /// The squashed commit is built and recorded; nothing pushed yet.
    Planned,
    /// `graft/batch-<n>` holds the squashed commit.
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
    /// When the batch was planned (ms): the squashed commit's date.
    pub created_ms: i64,
    /// `graft/batch-<n>`.
    pub branch: String,
    /// The PR title.
    pub title: String,
    /// The PR body (also the squashed commit's message body).
    pub body: String,
    /// The squashed commit.
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
}

/// A run's outcome, with what it settled and synced on the way.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BatchRun {
    /// The earlier batch this run found merged or rejected.
    pub settled: Option<BatchRecord>,
    /// The sync changeset the integration line moved to, if it synced.
    pub synced: Option<ChangesetId>,
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
            outcome: BatchOutcome::NothingToExport,
        };
        if let Some(stopped) = self.follow().await? {
            run.outcome = BatchOutcome::BaseStopped(stopped);
            return Ok(run);
        }
        if let Some(mut rec) = self.db.last_batch(&self.config.line)?
            && !rec.state.is_terminal()
        {
            self.advance(&mut rec).await?;
            if !rec.state.is_terminal() {
                run.outcome = BatchOutcome::Waiting(rec);
                return Ok(run);
            }
            run.settled = Some(rec);
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
        if let Some(rec) = self.plan(&state, tip, base_tip).await? {
            let mut rec = rec;
            self.advance(&mut rec).await?;
            run.outcome = BatchOutcome::Opened(rec);
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
                    authored_at: Timestamp {
                        ms: (self.clock)(),
                        offset_min: 0,
                    },
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
        let net = self.diff(state.sync_from, tip).await?;
        let target = if net.is_empty() {
            base_tip
        } else {
            let message = format!(
                "graft: net change of line {} since {}",
                self.config.line, state.sync_from
            );
            let squashed = self
                .create(vec![state.sync_from], net, message, base_tip)
                .await?;
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

    /// Builds and records the next batch: the land list, the squashed commit
    /// in the local mirror, then the record. `None` when the line holds
    /// nothing the base lacks.
    async fn plan(
        &mut self,
        state: &BatchLine,
        tip: ChangesetId,
        base_tip: ChangesetId,
    ) -> Result<Option<BatchRecord>, MirrorError> {
        let tip_view = self.view(tip).await?;
        let base_view = self.view(base_tip).await?;
        if tip_view.changeset.root == base_view.changeset.root {
            return Ok(None);
        }
        let base_sha = self.db.sha_of(base_tip)?.ok_or_else(|| {
            config_err(format!(
                "base tip {base_tip} has no git commit: follow {} first",
                self.config.base.branch
            ))
        })?;
        let (ids, capped) = self.lands(tip, state.start).await?;
        let views = self.views(&ids).await?;
        let mut lands = Vec::new();
        // Each attested id (a land, or the draft it was rebased from) and
        // the land it counts for.
        let mut wanted = HashMap::new();
        let mut from_lsn = u64::MAX;
        for id in &ids {
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
            });
        }
        if lands.is_empty() {
            return Ok(None);
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
        let (body, message_body) = render(
            &BatchHeader {
                number,
                line: self.config.line.clone(),
                branch: self.config.base.branch.clone(),
                base: base_tip,
                base_sha,
                start: state.start,
                tip,
                capped,
                truncated,
            },
            &lands,
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
            let tip_sha = exporter.export_changeset(tip).await?;
            self.mirror.commit_tree(tip_sha).await?
        };
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
        let squash_sha = self.mirror.write_commit(&raw).await?;

        let rec = BatchRecord {
            number,
            line: self.config.line.clone(),
            base_branch: self.config.base.branch.clone(),
            base: base_tip,
            base_sha,
            start: state.start,
            tip,
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
        if !self.db.insert_batch(&rec)? {
            return Err(config_err(format!(
                "another run opened a batch for line {} first",
                self.config.line
            )));
        }
        tracing::info!(
            repo = %self.config.repo, line = %self.config.line, batch = number,
            lands = rec.lands, squash = %squash_sha, "mirror batch planned"
        );
        Ok(Some(rec))
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

    /// Pushes the squashed commit to `graft/batch-<n>`, on a lease that
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
            let next = BatchLine {
                synced: state.synced,
                sync_from: rec.tip,
                start: rec.tip,
            };
            self.db.settle_batch(rec, &rec.line, &next)?;
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
}

/// One table row for a land. Every cell is bounded, so a row is too.
fn land_row(land: &LandLine) -> String {
    let cell = |s: &str| clean_text(s, FIELD_CHARS, false).replace('|', "\\|");
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
    let checks = if checks.is_empty() {
        "none".to_owned()
    } else {
        checks.join(", ")
    };
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

/// The PR body and the squashed commit's message body. Both say what the
/// batch is, list its lands (newest first) and end with the trailers. The
/// commit message lists every land. The PR body lists as many as fit in
/// [`MAX_PR_BODY`] and points to the commit message for the rest.
fn render(h: &BatchHeader, lands: &[LandLine]) -> (String, String) {
    let line = clean_text(&h.line, FIELD_CHARS, false);
    let branch = clean_text(&h.branch, FIELD_CHARS, false);
    let mut head = format!(
        "Batch {} of graft line `{line}`, squashed into one commit by graft-mirror \
         (decision D21). graft never merges or queues this PR: the operator merges it.\n\n",
        h.number
    );
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
    let note = |n: usize| {
        format!(
            "\n{n} more lands are listed in the squashed commit's message \
             (`git log -1 {BRANCH_PREFIX}{}`).\n",
            h.number
        )
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
    fn dates_are_utc_seconds() {
        assert_eq!(git_time(1_700_000_000_999), "1700000000 +0000");
    }
}
