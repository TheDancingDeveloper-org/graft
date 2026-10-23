//! GC protection records (`docs/design.md` §8.6, work item G3.11).
//!
//! Every write keeps the protection of what it changes current, in the
//! transaction of the change itself:
//!
//! - a changeset holds an object row for every object on the paths it
//!   changes, and pins each non-landed parent (`write.rs`);
//! - a record gets `expires_at` when it stops being a draft, and loses it
//!   when it is adopted; a land-attempt candidate gets it when it is
//!   written; landing clears it and the record's pins (the store does);
//! - before it commits, a write claims every doomed object it references or
//!   puts, and refuses or waits on one being deleted ([`Engine::claim`]).
//!
//! Nothing here deletes or collects: that is the sweep's (G3.2).
//! [`Engine::protect_repo`] backfills a repository written before
//! protection records, and sets its `gc_protection` marker.

use std::collections::{HashMap, HashSet};

use graft_model::{Change, Changeset, ChangesetId, EntryKind, ObjectId, ObjectSource, TreeId};
use graft_proto::ApiError;
use graft_store::{
    ChangesetRecord, MAX_PAGE, MetaStore, MetaTxn, ObjectStore, Precondition, RecordState, RepoId,
    TxnOutcome, Write,
};

use crate::source::Trees;
use crate::workspace::{CAS_RETRIES, contended};
use crate::{Engine, errors};

/// The prefix of a claim row's holder: `claim:<expiry ms>:<tag>`.
pub const CLAIM_HOLDER: &str = "claim:";

/// How many times a write waits for the delete of an object it derives
/// before it gives up with `Unavailable`.
const DELETE_WAITS: u32 = 32;

/// One wait for a delete to finish, ms: a delete is one frame removal and
/// one transaction.
pub(crate) const DELETE_WAIT_MS: u64 = 25;

/// The holder of a claim row.
pub(crate) fn claim_holder(expires_ms: i64, tag: &str) -> String {
    format!("{CLAIM_HOLDER}{expires_ms}:{tag}")
}

/// What [`Engine::protect_repo`] did.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ProtectReport {
    /// The repository had the marker already, so nothing was done.
    pub already: bool,
    /// Non-landed records read.
    pub records: u64,
    /// Pins set, summed over the records.
    pub pins: u64,
    /// Records given an expiry.
    pub expiries: u64,
    /// Object rows the row check added. Today's write path writes every row
    /// it checks, so this should be 0.
    pub rows_added: u64,
}

/// The distinct parents of a changeset, in order.
pub(crate) fn distinct_parents(cs: &Changeset) -> Vec<ChangesetId> {
    let mut seen = HashSet::new();
    cs.parents
        .iter()
        .copied()
        .filter(|p| seen.insert(*p))
        .collect()
}

impl<M: MetaStore, O: ObjectStore> Engine<M, O> {
    /// The expiry of a record that stops being a draft at `end_ms`.
    pub(crate) fn retained_until(&self, end_ms: i64) -> Option<i64> {
        let window = i64::try_from(self.config.draft_retention_ms).unwrap_or(i64::MAX);
        Some(end_ms.saturating_add(window))
    }

    /// Claims every doomed object among `ids` with a row that lapses after
    /// the claim time (design §8.6, revival): one transaction that clears
    /// their marks, guarded by each still being doomed and not deleting.
    /// Returns the ids being deleted, which cannot be claimed. One mark read
    /// per id; a write only for an object that is doomed.
    pub(crate) async fn claim(
        &self,
        repo: RepoId,
        ids: &[ObjectId],
        tag: &str,
    ) -> Result<Vec<ObjectId>, ApiError> {
        for _ in 0..CAS_RETRIES {
            let marks = self
                .meta
                .doom_marks(repo, ids)
                .await
                .map_err(errors::store)?;
            let (deleting, doomed): (Vec<_>, Vec<_>) =
                marks.into_iter().partition(|(_, mark)| mark.deleting);
            let deleting: Vec<ObjectId> = deleting.into_iter().map(|(id, _)| id).collect();
            if doomed.is_empty() {
                return Ok(deleting);
            }
            let claim_ms = i64::try_from(self.config.claim_ms).unwrap_or(i64::MAX);
            let expires_at_ms = self.clock.now().ms.saturating_add(claim_ms);
            let holder = claim_holder(expires_at_ms, tag);
            let txn = MetaTxn {
                preconditions: doomed
                    .iter()
                    .map(|(id, _)| Precondition::ObjectDoomed(*id))
                    .collect(),
                writes: doomed
                    .iter()
                    .map(|(id, _)| Write::AddHolderUntil {
                        object: *id,
                        holder: holder.clone(),
                        expires_at_ms,
                    })
                    .collect(),
            };
            match self.meta.commit(repo, txn).await.map_err(errors::store)? {
                TxnOutcome::Committed(_) => return Ok(deleting),
                // One became `deleting`, or a row revived it: read again.
                TxnOutcome::PreconditionFailed(_) => {}
            }
        }
        Err(contended())
    }

    /// Claims what a new changeset references before its commit. `named`
    /// are the blobs it names by id: one being deleted is `MissingObjects`.
    /// `own` are the objects it has the frames of (the trees it derives,
    /// inline blobs): for one being deleted it waits for the delete to
    /// finish, and the caller's frame check puts it again.
    pub(crate) async fn claim_for_write(
        &self,
        repo: RepoId,
        named: &[ObjectId],
        own: &[ObjectId],
        tag: &str,
    ) -> Result<(), ApiError> {
        let own_set: HashSet<ObjectId> = own.iter().copied().collect();
        let mut ids: Vec<ObjectId> = own.to_vec();
        ids.extend(named.iter().filter(|id| !own_set.contains(id)));
        for wait in 0..=DELETE_WAITS {
            let deleting = self.claim(repo, &ids, tag).await?;
            if deleting.is_empty() {
                return Ok(());
            }
            let missing: Vec<ObjectId> = deleting
                .iter()
                .copied()
                .filter(|id| !own_set.contains(id))
                .collect();
            if !missing.is_empty() {
                return Err(ApiError::MissingObjects { ids: missing });
            }
            if wait < DELETE_WAITS {
                self.sleeper.sleep(DELETE_WAIT_MS).await;
            }
        }
        Err(ApiError::Unavailable {
            retry_after_ms: DELETE_WAIT_MS,
        })
    }

    /// The objects on the paths `cs` changes, in its own snapshot: each
    /// tree from the root to each change, and each blob it puts. These are
    /// the rows a changeset holds. Reads O(changes × depth) trees of `cs`
    /// only, never a parent.
    async fn changed_path_objects(
        &self,
        repo: RepoId,
        cs: &Changeset,
    ) -> Result<HashSet<ObjectId>, ApiError> {
        let trees = Trees::new(&*self.objects, repo);
        let mut out = HashSet::new();
        for change in &cs.changes {
            out.insert(cs.root.0);
            let components: Vec<&str> = change.path().components().collect();
            let mut dir = cs.root;
            for (i, name) in components.iter().enumerate() {
                let tree = trees.tree(dir).await.map_err(errors::source)?;
                let Some(entry) = tree.get(name) else {
                    break;
                };
                if i + 1 == components.len() {
                    if let Change::Put { kind, .. } = change
                        && *kind != EntryKind::Submodule
                    {
                        out.insert(entry.id);
                    }
                    break;
                }
                if !entry.kind.is_tree() {
                    break;
                }
                dir = TreeId(entry.id);
                out.insert(dir.0);
            }
        }
        Ok(out)
    }

    /// Backfills the protection records of a repository written before
    /// them (design §8.6, "Migration of existing repositories"), at
    /// start-up and before the server serves it. It reads only non-landed
    /// records, through the state index, and their own trees; never landed
    /// history. Then, in batches of `batch` records:
    ///
    /// 1. pins on each non-landed record: the non-landed records that name
    ///    it as a parent, set outright, so a crashed run can simply run
    ///    again;
    /// 2. `expires_at`: the migration time plus the retention window for
    ///    abandoned and superseded records, plus the land-attempt lease for
    ///    drafts a lease holds;
    /// 3. the row check: any object on a record's changed paths without a
    ///    row held by that record gets one (reported, and expected to be
    ///    none);
    /// 4. the `gc_protection` marker, in the last batch's transaction.
    ///
    /// Costs O(non-landed records × changes × depth), once. A repository
    /// that has the marker is left alone.
    pub async fn protect_repo(&self, repo: &str, batch: u32) -> Result<ProtectReport, ApiError> {
        let repo = self.repo_id(repo).await?;
        let mut report = ProtectReport::default();
        if self.meta.gc_protection(repo).await.map_err(errors::store)? {
            report.already = true;
            return Ok(report);
        }
        let now = self.clock.now().ms;

        let mut records: Vec<(ChangesetId, ChangesetRecord, Changeset)> = Vec::new();
        for state in [
            RecordState::Draft,
            RecordState::Superseded,
            RecordState::Abandoned,
        ] {
            let mut after = None;
            loop {
                let page = self
                    .meta
                    .list_records(repo, state, after, MAX_PAGE)
                    .await
                    .map_err(errors::store)?;
                let Some((last, _)) = page.last() else {
                    break;
                };
                after = Some(*last);
                for (id, record) in page {
                    let (cs, _) = self.changeset(repo, id).await?;
                    records.push((id, record, cs));
                }
            }
        }
        report.records = records.len() as u64;

        let live: HashSet<ChangesetId> = records.iter().map(|(id, _, _)| *id).collect();
        let mut pins: HashMap<ChangesetId, u32> = HashMap::new();
        for (_, _, cs) in &records {
            for parent in distinct_parents(cs) {
                if live.contains(&parent) {
                    *pins.entry(parent).or_default() += 1;
                }
            }
        }

        let land_lease = i64::try_from(self.config.land_lease_ms).unwrap_or(i64::MAX);
        let batch = batch.clamp(1, MAX_PAGE) as usize;
        let mut writes = Vec::new();
        for (n, (id, record, cs)) in records.iter().enumerate() {
            let count = pins.get(id).copied().unwrap_or(0);
            report.pins += u64::from(count);
            writes.push(Write::SetPins {
                id: *id,
                pins: count,
            });
            let expires_at_ms = match record.state {
                RecordState::Abandoned | RecordState::Superseded => self.retained_until(now),
                RecordState::Draft
                    if record
                        .holder
                        .as_deref()
                        .is_some_and(|h| h.starts_with("land:")) =>
                {
                    Some(now.saturating_add(land_lease))
                }
                _ => None,
            };
            if expires_at_ms.is_some() {
                report.expiries += 1;
            }
            writes.push(Write::SetExpiry {
                id: *id,
                expires_at_ms,
            });

            let holder = id.to_string();
            let mut have = HashSet::new();
            let mut after = None;
            loop {
                let page = self
                    .meta
                    .held_objects(repo, &holder, after, MAX_PAGE)
                    .await
                    .map_err(errors::store)?;
                let Some(last) = page.last() else {
                    break;
                };
                after = Some(*last);
                have.extend(page);
            }
            let mut missing: Vec<ObjectId> = self
                .changed_path_objects(repo, cs)
                .await?
                .into_iter()
                .filter(|o| !have.contains(o))
                .collect();
            missing.sort();
            report.rows_added += missing.len() as u64;
            writes.extend(missing.into_iter().map(|object| Write::AddHolder {
                object,
                holder: holder.clone(),
            }));

            if (n + 1) % batch == 0 && n + 1 < records.len() {
                self.commit_backfill(repo, std::mem::take(&mut writes))
                    .await?;
            }
        }
        writes.push(Write::SetGcProtection(true));
        self.commit_backfill(repo, writes).await?;
        Ok(report)
    }

    async fn commit_backfill(&self, repo: RepoId, writes: Vec<Write>) -> Result<(), ApiError> {
        let txn = MetaTxn {
            preconditions: Vec::new(),
            writes,
        };
        match self.meta.commit(repo, txn).await.map_err(errors::store)? {
            TxnOutcome::Committed(_) => Ok(()),
            TxnOutcome::PreconditionFailed(_) => Err(contended()),
        }
    }
}
