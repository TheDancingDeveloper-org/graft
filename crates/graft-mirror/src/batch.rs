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
//!    one commit per land, oldest first (D26). Each land is rebased with
//!    line merge (D16) onto the replay before it, starting at the base, as
//!    an unlanded changeset: a cherry-pick of the land's own change that
//!    keeps what the base changed in the same files. Each replay's git tree
//!    (written by `graft-git`'s deterministic exporter) becomes that land's
//!    commit: the land's author and author time, the mirror as committer at
//!    the same time, the land's message, and `Graft-Changeset`,
//!    `Graft-Session`, `Graft-Work-Item` and `Graft-Batch` trailers. A
//!    revert land is its own commit. The last commit's tree must equal the
//!    cut tip's tree (the root check). When a land does not merge onto the
//!    replay before it, the root check fails, or the batch has more than
//!    [`MAX_COMMITS`] lands, the
//!    batch is exported as one squashed commit instead, as D21 did, and the
//!    PR body and the commit say why. Commit objects are built byte for
//!    byte from recorded fields (`graft_git::export::commit_object`),
//!    stored in the local mirror and recorded (the chain in
//!    `batch_commits`) before anything is pushed, so a retry pushes the
//!    same shas. With [`Batcher::with_export_log`], each commit is also
//!    recorded with the server as its land's export (`RecordExports`, in
//!    chunks within the server's cap) before the push, so a CI run on a
//!    batch commit can exchange its token for that land (D40).
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
//! Checked lines (WI-2599, design §8.7 step 6, phase 2): with
//! [`BatchConfig::checked`] (`--checks post-land`, for a line whose server
//! config has `checks = "post-land"`), the cut is the newest land such that
//! every land up to it is `good` in `GetLandChecks`, or is reverted by a land
//! inside the cut. Pending lands and a bad land with no revert yet wait; a
//! run with nothing settled is [`BatchOutcome::Unsettled`]. `max_lands`
//! applies within the settled prefix, and an append to an open batch takes
//! only settled lands. The rows are read from the batch start's position,
//! found by one-row probes (O(log line length)), then once, paged, for at
//! most the batch's lands.
//!
//! Costs follow the change: the sync reads one diff and one rebase; the land
//! list is a first-parent `Log` from the tip back to the batch start, capped
//! at [`MAX_LANDS`]; attestations come from a bounded read of the journal's
//! `Attested` entries from the oldest land in the batch, capped at
//! [`MAX_ATTEST_PAGES`] pages and marked as truncated beyond that.

mod batcher;
mod render;
mod types;

#[cfg(test)]
mod tests;

pub use batcher::Batcher;
#[cfg(test)]
pub(crate) use batcher::{attribution_cell, attribution_trailer, coauthor_lines};
pub use types::{
    AuthorIdentity, BATCH, BATCH_TRAILER, BRANCH_PREFIX, BatchCommit, BatchConfig, BatchLine,
    BatchOutcome, BatchRecord, BatchRun, BatchState, CHANGESET_TRAILER, DEFAULT_DROP_REASON,
    MAX_ATTEST_PAGES, MAX_CHECKS, MAX_COMMITS, MAX_DROPS_LISTED, MAX_LANDS, MAX_PR_BODY,
    REVERTS_KEY, SYNC_KEY, is_sync,
};
