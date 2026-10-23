//! The sweep (`docs/design.md` §8.6): lapse, collect, expire uploads and
//! claims, and delete. Every step is one metadata transaction with its
//! journal entry, guarded by preconditions, so a crash between any two
//! leaves a consistent state, and a concurrent write either wins its own
//! precondition or makes the sweep's fail.
//!
//! A delete is two transactions around the frame delete: the first marks
//! the object `deleting` (it is still doomed and has no row), the second
//! removes the mark. A `deleting` mark a crashed pass left behind is
//! finished by the next delete step, or at start-up by
//! [`Collector::recover`]: the frame delete is idempotent.

use std::collections::{BTreeMap, HashSet};

use graft_model::{ChangesetId, ObjectId};
use graft_store::{
    ChangesetRecord, MetaStore, MetaTxn, ObjectStore, Precondition, RecordState, RepoId,
    StoreError, StoreResult, TxnOutcome, WorkspaceRecord, Write,
};

use crate::{Collector, GcError, Shape, distinct, held_by, held_objects, journal, ms};

/// How many times a step re-reads and retries after a concurrent write
/// added a row to an object it was about to doom.
const DOOM_ATTEMPTS: u32 = 4;

/// What a sweep pass did.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct SweepReport {
    /// Lapsed workspaces closed.
    pub lapsed: u64,
    /// Drafts those workspaces held, now abandoned.
    pub abandoned: u64,
    /// Records collected.
    pub collected: u64,
    /// Rows moved to a successor.
    pub moved: u64,
    /// Expired upload and claim rows removed.
    pub expired: u64,
    /// Objects doomed.
    pub doomed: u64,
    /// Objects deleted.
    pub deleted: u64,
    /// `deleting` marks a crashed pass left, finished.
    pub recovered: u64,
    /// Frame bytes reclaimed: `graft_gc_reclaimed_bytes_total`.
    pub bytes: u64,
    /// Steps skipped because a concurrent write changed what they read
    /// (a renewal, an adoption, a new child, a revival).
    pub skipped: u64,
    /// The pass stopped at its budget; the next continues.
    pub more: bool,
}

/// One pass over one repository.
struct Pass<'a, M, O, S> {
    c: &'a Collector<M, O>,
    repo: RepoId,
    shape: &'a S,
    budget: u32,
    report: SweepReport,
}

impl<M: MetaStore, O: ObjectStore> Collector<M, O> {
    /// One sweep pass over `repo`.
    ///
    /// It refuses a repository without the `gc_protection` marker
    /// ([`GcError::Unprotected`]), one whose deletion switch is off
    /// ([`GcError::DeletionOff`]), and one another sweep holds
    /// ([`GcError::Busy`]). With `verify`, it first checks the plan against
    /// the ancestry walk and sweeps nothing on a mismatch
    /// ([`GcError::Mismatch`]).
    pub async fn sweep<S: Shape>(
        &self,
        repo: RepoId,
        shape: &S,
        verify: bool,
    ) -> Result<SweepReport, GcError> {
        if !self.meta.gc_protection(repo).await? {
            return Err(GcError::Unprotected);
        }
        if !crate::deletion_enabled(&self.meta, repo).await? {
            return Err(GcError::DeletionOff);
        }
        let Some(_lease) = self.leases.try_acquire(repo) else {
            return Err(GcError::Busy);
        };
        if verify {
            let v = self.verify(repo, self.now(), shape).await?;
            if !v.matches() {
                return Err(GcError::Mismatch {
                    changesets: v.unsafe_changesets,
                    objects: v.unsafe_objects,
                });
            }
        }
        let mut pass = Pass {
            c: self,
            repo,
            shape,
            budget: self.config.pass_budget.max(1),
            report: SweepReport::default(),
        };
        pass.lapse().await?;
        pass.collect().await?;
        pass.expire_rows().await?;
        pass.delete().await?;
        Ok(pass.report)
    }

    /// Finishes every delete a crashed pass left `deleting`: the frame
    /// delete (idempotent), then the mark. Run at start-up, before the
    /// server serves; it needs neither the switch nor the lease, because it
    /// only completes deletes already decided. Returns how many it
    /// finished.
    pub async fn recover(&self, repo: RepoId) -> Result<u64, GcError> {
        let batch = self.batch();
        let mut finished = 0;
        let mut after = None;
        loop {
            let page = self.meta.old_doomed(repo, i64::MAX, after, batch).await?;
            let Some((last, mark)) = page.last() else {
                return Ok(finished);
            };
            after = Some((mark.since_ms, *last));
            for (object, mark) in page {
                if mark.deleting {
                    self.finish_delete(repo, object).await?;
                    finished += 1;
                }
            }
        }
    }

    /// The frame delete and the mark's removal, for an object marked
    /// `deleting`. Returns the frame bytes it removed.
    async fn finish_delete(&self, repo: RepoId, object: ObjectId) -> StoreResult<u64> {
        let bytes = self
            .objects
            .get(repo, object)
            .await?
            .map_or(0, |f| f.len() as u64);
        self.objects.delete(repo, object).await?;
        commit(
            &self.meta,
            repo,
            MetaTxn {
                preconditions: Vec::new(),
                writes: vec![
                    Write::Undoom(object),
                    journal::object_deleted(object, bytes),
                ],
            },
        )
        .await?;
        Ok(bytes)
    }
}

/// Commits a transaction with no preconditions.
async fn commit<M: MetaStore>(meta: &M, repo: RepoId, txn: MetaTxn) -> StoreResult<()> {
    match meta.commit(repo, txn).await? {
        TxnOutcome::Committed(_) => Ok(()),
        TxnOutcome::PreconditionFailed(_) => Err(StoreError::Backend(
            "an unconditional write failed a precondition".to_owned(),
        )),
    }
}

impl<M: MetaStore, O: ObjectStore, S: Shape> Pass<'_, M, O, S> {
    fn meta(&self) -> &M {
        &self.c.meta
    }

    /// Takes one transaction from the budget, or ends the pass.
    fn spend(&mut self) -> bool {
        if self.budget == 0 {
            self.report.more = true;
            return false;
        }
        self.budget -= 1;
        true
    }

    /// The rate limit, after each batch.
    async fn pause(&self) {
        if !self.c.config.pause.is_zero() {
            self.c.pace.pause(self.c.config.pause).await;
        }
    }

    // -----------------------------------------------------------------------
    // 0. Lapse.

    async fn lapse(&mut self) -> StoreResult<()> {
        let batch = self.c.batch();
        let mut after = None;
        loop {
            let now = self.c.now();
            let page = self
                .meta()
                .lapsed_workspaces(self.repo, now, after.clone(), batch)
                .await?;
            let Some(last) = page.last() else {
                return Ok(());
            };
            after = Some((last.lease_until_ms, last.name.clone()));
            for ws in page {
                if !self.spend() {
                    return Ok(());
                }
                self.close(&ws).await?;
            }
            self.pause().await;
        }
    }

    /// Closes a lapsed workspace as the engine does: its drafts become
    /// abandoned, expiring a retention after the lease ended, and the
    /// record goes, guarded by the workspace being unchanged (a renewal
    /// wins). The drafts are found from the head down first parents, so
    /// the cost is the stack's length.
    async fn close(&mut self, ws: &WorkspaceRecord) -> StoreResult<()> {
        let expires = ws
            .lease_until_ms
            .saturating_add(ms(self.c.config.draft_retention));
        let mut txn = MetaTxn {
            preconditions: vec![Precondition::WorkspaceIs {
                name: ws.name.clone(),
                expected: Some(ws.clone()),
            }],
            writes: Vec::new(),
        };
        let mut abandoned = 0;
        let mut at = ws.head;
        let mut walked = 0;
        while let Some(id) = at {
            if id == ws.base || walked >= self.c.config.max_stack {
                break;
            }
            walked += 1;
            let Some((content, record)) = self.meta().changeset(self.repo, id).await? else {
                break;
            };
            if matches!(record.state, RecordState::Landed | RecordState::Collected) {
                break;
            }
            if held_by(&record, &ws.name) {
                txn.preconditions.push(Precondition::RecordStateIs {
                    id,
                    state: RecordState::Draft,
                });
                txn.writes.push(Write::SetRecord {
                    id,
                    record: ChangesetRecord {
                        state: RecordState::Abandoned,
                        holder: None,
                        superseded_by: None,
                        ..record
                    },
                });
                txn.writes.push(Write::SetExpiry {
                    id,
                    expires_at_ms: Some(expires),
                });
                abandoned += 1;
            }
            at = self.shape.parents(&content)?.first().copied();
        }
        txn.writes.push(Write::DeleteWorkspace(ws.name.clone()));
        txn.writes
            .push(journal::workspace_closed(&ws.name, abandoned));
        match self.meta().commit(self.repo, txn).await? {
            TxnOutcome::Committed(_) => {
                self.report.lapsed += 1;
                self.report.abandoned += abandoned as u64;
            }
            TxnOutcome::PreconditionFailed(_) => self.report.skipped += 1,
        }
        Ok(())
    }

    // -----------------------------------------------------------------------
    // 1. Collect.

    async fn collect(&mut self) -> StoreResult<()> {
        let batch = self.c.batch();
        let mut after = None;
        loop {
            let now = self.c.now();
            let page = self
                .meta()
                .expired_records(self.repo, now, after, batch)
                .await?;
            let Some(last) = page.last() else {
                return Ok(());
            };
            after = Some(*last);
            for (_, id) in page {
                // A parent this collection unpins may be expired already;
                // it is taken now, so a long stack unwinds from the top.
                let mut work = vec![id];
                while let Some(id) = work.pop() {
                    if !self.spend() {
                        return Ok(());
                    }
                    for parent in self.collect_one(id).await? {
                        if self.collectable(parent).await? {
                            work.push(parent);
                        }
                    }
                }
            }
            self.pause().await;
        }
    }

    /// Whether `id` is expired with no pins, as the index would say.
    async fn collectable(&self, id: ChangesetId) -> StoreResult<bool> {
        let now = self.c.now();
        Ok(self
            .meta()
            .protection(self.repo, id)
            .await?
            .is_some_and(|p| p.pins == 0 && p.expires_at_ms.is_some_and(|e| e <= now)))
    }

    /// Collects one record: its rows go (or move to its successor), one pin
    /// comes off each non-landed parent, the objects left with no row are
    /// doomed, and the record becomes a tombstone, in one transaction
    /// guarded by the record and its protection being unchanged. Returns
    /// the non-landed parents it unpinned.
    async fn collect_one(&mut self, id: ChangesetId) -> StoreResult<Vec<ChangesetId>> {
        let (repo, batch) = (self.repo, self.c.batch());
        for attempt in 1..=DOOM_ATTEMPTS {
            let Some((content, record)) = self.meta().changeset(repo, id).await? else {
                return Ok(Vec::new());
            };
            if matches!(record.state, RecordState::Landed | RecordState::Collected) {
                return Ok(Vec::new());
            }
            let Some(protection) = self.meta().protection(repo, id).await? else {
                return Ok(Vec::new());
            };
            let now = self.c.now();
            if protection.pins > 0 || !protection.expires_at_ms.is_some_and(|e| e <= now) {
                return Ok(Vec::new());
            }
            let holder = id.to_string();
            let rows = held_objects(self.meta(), repo, &holder, batch).await?;
            let mut txn = MetaTxn {
                preconditions: vec![Precondition::RecordIs {
                    id,
                    record: record.clone(),
                    protection,
                }],
                writes: Vec::new(),
            };

            // Successors keep their objects.
            let mut moved: Vec<ObjectId> = Vec::new();
            if let (RecordState::Superseded, Some(s)) = (record.state, record.superseded_by)
                && !rows.is_empty()
                && let Some((to, successor)) = self.meta().changeset(repo, s).await?
                && successor.state != RecordState::Collected
            {
                let held: HashSet<ObjectId> = rows.iter().copied().collect();
                let mut seen = HashSet::new();
                moved = self
                    .shape
                    .retained(repo, &content, &to, &rows)
                    .await?
                    .into_iter()
                    .filter(|o| held.contains(o) && seen.insert(*o))
                    .collect();
                if !moved.is_empty() {
                    txn.preconditions.push(Precondition::RecordLive(s));
                    let holder = s.to_string();
                    txn.writes.extend(moved.iter().map(|o| Write::AddHolder {
                        object: *o,
                        holder: holder.clone(),
                    }));
                }
            }
            txn.writes.extend(rows.iter().map(|o| Write::RemoveHolder {
                object: *o,
                holder: holder.clone(),
            }));

            // One pin off each parent that is still there to hold one; the
            // store leaves a landed parent's alone.
            let mut unpinned = Vec::new();
            for p in distinct(id, self.shape.parents(&content)?) {
                let Some((_, parent)) = self.meta().changeset(repo, p).await? else {
                    continue;
                };
                if parent.state == RecordState::Collected {
                    continue;
                }
                txn.preconditions.push(Precondition::RecordLive(p));
                txn.writes.push(Write::Unpin(p));
                if parent.state != RecordState::Landed {
                    unpinned.push(p);
                }
            }

            // Doom what is left with no row.
            let kept: HashSet<ObjectId> = moved.iter().copied().collect();
            let mut doomed = Vec::new();
            for o in rows.iter().filter(|o| !kept.contains(o)) {
                if self.meta().holders(repo, *o).await? <= 1 {
                    doomed.push(*o);
                }
            }
            let since_ms = self.c.now();
            txn.writes.extend(doomed.iter().map(|o| Write::Doom {
                object: *o,
                since_ms,
            }));
            txn.writes.push(Write::SetRecord {
                id,
                record: ChangesetRecord {
                    state: RecordState::Collected,
                    holder: None,
                    ..record
                },
            });
            txn.writes
                .push(journal::collected(id, doomed.len(), moved.len()));

            match self.meta().commit(repo, txn).await {
                Ok(TxnOutcome::Committed(_)) => {
                    self.report.collected += 1;
                    self.report.moved += moved.len() as u64;
                    self.report.doomed += doomed.len() as u64;
                    return Ok(unpinned);
                }
                // A child pinned it, it was adopted or landed, or its
                // successor or a parent went: skip it.
                Ok(TxnOutcome::PreconditionFailed(_)) => {
                    self.report.skipped += 1;
                    return Ok(Vec::new());
                }
                // A write added a row to an object it was dooming: read
                // again.
                Err(StoreError::Invalid(_)) if attempt < DOOM_ATTEMPTS => {}
                Err(e) => return Err(e),
            }
        }
        Ok(Vec::new())
    }

    // -----------------------------------------------------------------------
    // 2. Uploads and claims.

    async fn expire_rows(&mut self) -> StoreResult<()> {
        let batch = self.c.batch();
        let mut after = None;
        loop {
            let now = self.c.now();
            let page = self
                .meta()
                .expired_rows(self.repo, now, after.clone(), batch)
                .await?;
            let Some(last) = page.last().cloned() else {
                return Ok(());
            };
            after = Some(last);
            let mut by_holder: BTreeMap<String, Vec<ObjectId>> = BTreeMap::new();
            for row in page {
                by_holder.entry(row.holder).or_default().push(row.object);
            }
            for (holder, objects) in by_holder {
                if !self.spend() {
                    return Ok(());
                }
                self.expire(&holder, &objects).await?;
            }
            self.pause().await;
        }
    }

    /// Removes one holder's expired rows, dooming the objects they leave
    /// with no row.
    async fn expire(&mut self, holder: &str, objects: &[ObjectId]) -> StoreResult<()> {
        let repo = self.repo;
        for attempt in 1..=DOOM_ATTEMPTS {
            let mut doomed = Vec::new();
            for o in objects {
                if self.meta().holders(repo, *o).await? <= 1 {
                    doomed.push(*o);
                }
            }
            let since_ms = self.c.now();
            let mut writes: Vec<Write> = objects
                .iter()
                .map(|o| Write::RemoveHolder {
                    object: *o,
                    holder: holder.to_owned(),
                })
                .collect();
            writes.extend(doomed.iter().map(|o| Write::Doom {
                object: *o,
                since_ms,
            }));
            writes.push(journal::rows_expired(holder, objects.len(), doomed.len()));
            let txn = MetaTxn {
                preconditions: Vec::new(),
                writes,
            };
            match commit(self.meta(), repo, txn).await {
                Ok(()) => {
                    self.report.expired += objects.len() as u64;
                    self.report.doomed += doomed.len() as u64;
                    return Ok(());
                }
                Err(StoreError::Invalid(_)) if attempt < DOOM_ATTEMPTS => {}
                Err(e) => return Err(e),
            }
        }
        Ok(())
    }

    // -----------------------------------------------------------------------
    // 3. Delete.

    async fn delete(&mut self) -> StoreResult<()> {
        let batch = self.c.batch();
        let grace = ms(self.c.config.delete_grace);
        let mut after = None;
        loop {
            let until = self.c.now().saturating_sub(grace);
            let page = self
                .meta()
                .old_doomed(self.repo, until, after, batch)
                .await?;
            let Some((last, mark)) = page.last() else {
                return Ok(());
            };
            after = Some((mark.since_ms, *last));
            for (object, mark) in page {
                if !self.spend() {
                    return Ok(());
                }
                if mark.deleting {
                    self.report.bytes += self.c.finish_delete(self.repo, object).await?;
                    self.report.recovered += 1;
                    continue;
                }
                let txn = MetaTxn {
                    preconditions: vec![
                        Precondition::ObjectDoomed(object),
                        Precondition::ObjectUnheld(object),
                    ],
                    writes: vec![Write::MarkDeleting(object)],
                };
                match self.meta().commit(self.repo, txn).await? {
                    TxnOutcome::Committed(_) => {
                        self.report.bytes += self.c.finish_delete(self.repo, object).await?;
                        self.report.deleted += 1;
                    }
                    // Revived by a claim or a row since the scan.
                    TxnOutcome::PreconditionFailed(_) => self.report.skipped += 1,
                }
            }
            self.pause().await;
        }
    }
}
