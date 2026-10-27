//! Records, configuration and the small types a batch is made of.

use std::collections::BTreeMap;

use graft_git::GitSha;
use graft_git::export::Committer;
use graft_git::exporter::LandTimes;
use graft_model::{Author, ChangesetId, Metadata, Timestamp};
use serde::Deserialize;

use crate::MirrorError;
use crate::export::default_committer;
use crate::follow::{FollowConfig, FollowLine};

/// The authority batch mode claims for an integration line: nothing else
/// (export, follow) writes it from the mirror.
pub const BATCH: &str = "batch";

/// The metadata key on a sync changeset, naming the base tip it absorbed.
/// The land list skips changesets that carry it.
pub const SYNC_KEY: &str = "batch.sync-base";

/// The sync key written before D33 reserved `mirror.*` for git imports.
const LEGACY_SYNC_KEY: &str = "mirror.batch-sync";

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
pub(crate) const MESSAGE_CHARS: usize = 8000;

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

/// One-row `GetLandChecks` reads that find where a batch starts on a
/// checked line, per search phase: the exponential probe and the bisection
/// each take at most this many (O(log line length)).
pub(crate) const MAX_CHECK_PROBES: u32 = 64;

/// Entries per page (journal, log, diff).
pub(crate) const PAGE: u32 = 256;

/// Changesets per `GetChangesets` call.
pub(crate) const FETCH: usize = 256;

/// The longest PR title.
pub(crate) const TITLE_CHARS: usize = 120;

/// The longest text kept from one land's subject or one metadata value.
pub(crate) const FIELD_CHARS: usize = 100;

/// Where a batch is.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BatchState {
    /// The commits are built and recorded; nothing pushed yet.
    Planned,
    /// `graft/batch-<n>` holds the batch's commits.
    Pushed,
    /// A PR is open on the branch, waiting for review and `land-pr`.
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
    /// Lands present when the batch PR first opened.
    pub initial_lands: u64,
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
    /// The identity of the squashed commit and of sync changesets, and the
    /// last-resort author when neither the principal map nor
    /// [`default_author`] names one.
    pub committer: Committer,
    /// Git identities for authenticated principals (`[export.authors]`).
    pub authors: BTreeMap<String, AuthorIdentity>,
    /// Per-repo author when the authenticated principal has no mapping
    /// (`[export] default_author`).
    pub default_author: Option<AuthorIdentity>,
    /// Lands to drop from the line's batches: each is reverted on the line.
    pub exclude: Vec<ChangesetId>,
    /// Why the lands are dropped (named in the PR body).
    pub drop_reason: Option<String>,
    /// The most lands one batch takes; newer lands wait for the next one.
    pub max_lands: Option<usize>,
    /// Hold a batch until its oldest land is this old (ms, by author time),
    /// unless it already has `max_lands` lands.
    pub max_age_ms: Option<i64>,
    /// The line has `checks = "post-land"` on the server (design §8.7,
    /// phase 2): a batch is cut at the newest land whose prefix is settled
    /// in `GetLandChecks`. `false` batches as phase 1 does.
    pub checked: bool,
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
            authors: BTreeMap::new(),
            default_author: None,
            exclude: Vec::new(),
            drop_reason: None,
            max_lands: None,
            max_age_ms: None,
            checked: false,
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
    /// The line is checked and its oldest land's post-land checks are not
    /// settled (pending, or bad with no revert yet); no batch yet.
    Unsettled {
        /// Lands waiting.
        lands: usize,
        /// The oldest land the cut stops at.
        waiting: ChangesetId,
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
pub(crate) struct Attested {
    pub(crate) changeset: String,
    pub(crate) check: String,
    pub(crate) passed: bool,
}

/// One land, as the PR body lists it.
pub(crate) struct LandLine {
    pub(crate) id: ChangesetId,
    pub(crate) author: String,
    pub(crate) session: Option<String>,
    pub(crate) work_item: Option<String>,
    pub(crate) subject: String,
    /// Each check's latest result, by name.
    pub(crate) checks: BTreeMap<String, bool>,
    /// Author time (ms).
    pub(crate) authored_ms: i64,
    /// The changeset this land reverts, when it is a revert.
    pub(crate) reverts: Option<ChangesetId>,
    /// Its first parent: its own change is the diff from it.
    pub(crate) parent: Option<ChangesetId>,
    /// The author's name and email for a git ident.
    pub(crate) ident: (String, String),
    /// Author time, with its offset.
    pub(crate) authored: Timestamp,
    /// The whole message, cleaned for a commit (`@` kept) and bounded.
    pub(crate) message: String,
    /// The session, work item and model for commit trailers (`@` kept).
    pub(crate) trailer_session: Option<String>,
    pub(crate) trailer_work_item: Option<String>,
    pub(crate) trailer_model: Option<String>,
    /// `Co-authored-by` values recorded on the land, already one line each.
    pub(crate) trailer_coauthors: Vec<String>,
    /// Client-supplied author, only as a trailer (`Claimed-Author`).
    pub(crate) trailer_claimed_author: Option<String>,
    /// The `<src>@<changeset>` a merge landing merged (D29).
    pub(crate) trailer_merged_from: Option<String>,
}

/// A land dropped from a batch: it and its revert are both in the batch.
pub(crate) struct Dropped {
    pub(crate) land: ChangesetId,
    pub(crate) revert: ChangesetId,
    pub(crate) reason: String,
}

/// Exported native changesets of the integration line are dated by the
/// batch's planning time; the mirror map keeps the result.
pub(crate) struct Stamp(pub(crate) i64);

impl LandTimes for Stamp {
    fn landed_at(&self, _id: ChangesetId) -> Result<Timestamp, graft_git::GitError> {
        Ok(Timestamp {
            ms: self.0,
            offset_min: 0,
        })
    }
}

pub(crate) fn config_err(what: impl Into<String>) -> MirrorError {
    MirrorError::Config(what.into())
}

pub(crate) fn system_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_millis() as i64)
}

/// `<seconds> +0000`, as git writes a UTC date.
pub(crate) fn git_time(ms: i64) -> String {
    format!("{} +0000", ms.div_euclid(1000))
}

/// `<seconds> <+hhmm>`, as git writes a date with its offset.
pub(crate) fn git_time_at(t: Timestamp) -> String {
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
pub(crate) fn ident_part(s: &str) -> String {
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

/// One configured git identity for a principal (`[export.authors]`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AuthorIdentity {
    /// Display name written as the git author.
    pub name: String,
    /// Email written as the git author. A configured identity, never a secret.
    pub email: String,
}

/// The git ident of a land's per-land commit (D26).
///
/// The author is the land's authenticated principal (`RefMoved.actor`),
/// mapped through `[export.authors]`, then `[export] default_author`, then
/// the bot `committer`. A client-supplied name or email is never the git
/// author. `principal` is `None` when the journal has no actor for the land
/// (a land recorded before the actor field, or a no-op land that did not
/// move the ref); that falls through the same defaults.
pub(crate) fn land_ident(
    principal: Option<&str>,
    authors: &BTreeMap<String, AuthorIdentity>,
    default_author: Option<&AuthorIdentity>,
    committer: &Committer,
) -> (String, String) {
    let key = principal.map(str::trim).filter(|s| !s.is_empty());
    if let Some(mapped) = key.and_then(|k| authors.get(k)) {
        return (ident_part(&mapped.name), ident_part(&mapped.email));
    }
    if let Some(fallback) = default_author {
        return (ident_part(&fallback.name), ident_part(&fallback.email));
    }
    (ident_part(&committer.name), ident_part(&committer.email))
}

/// A claimed author as a trailer, only when it is not empty and not the
/// identity already written as the git author. Never a token.
pub(crate) fn claimed_author_trailer(author: &Author) -> Option<String> {
    let name = ident_part(&author.name);
    let email = author.email.trim();
    if email.is_empty() {
        return None;
    }
    let email = ident_part(email);
    if name == "unknown" && email == "unknown" {
        return None;
    }
    Some(format!("Claimed-Author: {name} <{email}>"))
}

/// A short form of a changeset id for listings.
pub(crate) fn short(id: ChangesetId) -> String {
    id.to_string().chars().take(13).collect()
}

/// Whether a changeset with `metadata` is a sync changeset the mirror made.
pub fn is_sync(metadata: &Metadata) -> bool {
    sync_base(metadata).is_some()
}

/// The base a sync changeset absorbed, including the pre-D33 key.
pub(crate) fn sync_base(metadata: &Metadata) -> Option<&str> {
    metadata
        .get(SYNC_KEY)
        .or_else(|| metadata.get(LEGACY_SYNC_KEY))
        .map(String::as_str)
}
