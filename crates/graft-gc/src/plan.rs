//! The dry run over protection records, and `--verify`.
//!
//! The plan is what a sweep at `now` would do, read from the same indexes
//! the sweep reads and with the same rules: the drafts the lapse step would
//! abandon, the records that would be collected (expired with no pins, and
//! the parents that reach no pins as their children go), the rows the
//! successor rule would move, and the objects left with no row. It writes
//! nothing.
//!
//! `--verify` checks the plan against the G2.9 ancestry walk. The two use
//! different retention bases (the walk counts from `received_at`, the plan
//! from `expires_at`), so they disagree on young records, and the walk's
//! successor rule is not the plan's. What must hold whatever the ages is
//! the safety the walk exists to check: nothing the plan collects is an
//! ancestor of a changeset it keeps, and nothing it dooms is an object a
//! kept changeset introduced.

use std::collections::{BTreeSet, HashMap, HashSet};

use graft_model::{ChangesetId, ObjectId};
use graft_store::{MetaStore, ObjectStore, RecordState, RepoId};

use crate::ancestry::{ancestry, descendants_reach, holder_names};
use crate::{Collector, GcError, Report, Shape, distinct, held_by, held_objects, ms};

/// What a sweep at a given time would do.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Plan {
    /// Drafts of lapsed workspaces the lapse step would abandon.
    pub abandoned: BTreeSet<ChangesetId>,
    /// Records that would be collected.
    pub changesets: BTreeSet<ChangesetId>,
    /// Objects that would be doomed: every row they have would go.
    pub doomed: BTreeSet<ObjectId>,
    /// The stored frame bytes of `doomed`.
    pub bytes: u64,
    /// Objects already doomed for longer than the grace period, which the
    /// delete step would take now.
    pub deletable: u64,
}

impl Plan {
    /// The dry-run report.
    pub fn report(&self) -> Report {
        Report {
            changesets: self.changesets.len() as u64,
            doomed: self.doomed.len() as u64,
            deleted: 0,
            bytes: self.bytes,
        }
    }
}

/// What `--verify` found.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Verify {
    /// The plan checked.
    pub plan: Plan,
    /// The G2.9 ancestry walk's report, for comparison.
    pub ancestry: Report,
    /// Records both call collectable.
    pub both: u64,
    /// Records the plan collects that a changeset it keeps descends from.
    pub unsafe_changesets: Vec<ChangesetId>,
    /// Objects the plan dooms that a changeset it keeps introduced.
    pub unsafe_objects: Vec<ObjectId>,
}

impl Verify {
    /// Whether the plan passed: it collects and dooms nothing the walk
    /// protects.
    pub fn matches(&self) -> bool {
        self.unsafe_changesets.is_empty() && self.unsafe_objects.is_empty()
    }
}

impl<M: MetaStore, O: ObjectStore> Collector<M, O> {
    /// The plan of a sweep at `now_ms`, from the protection records.
    pub async fn plan<S: Shape>(
        &self,
        repo: RepoId,
        now_ms: i64,
        shape: &S,
    ) -> Result<Plan, GcError> {
        let meta = &self.meta;
        let batch = self.batch();
        let mut plan = Plan::default();

        // Lapse: the drafts that would be abandoned, and their expiry.
        let retention = ms(self.config.draft_retention);
        let mut lapse_expiry: HashMap<ChangesetId, i64> = HashMap::new();
        let mut after = None;
        loop {
            let page = meta
                .lapsed_workspaces(repo, now_ms, after.clone(), batch)
                .await?;
            let Some(last) = page.last() else {
                break;
            };
            after = Some((last.lease_until_ms, last.name.clone()));
            for ws in page {
                let expires = ws.lease_until_ms.saturating_add(retention);
                let mut at = ws.head;
                let mut walked = 0;
                while let Some(id) = at {
                    if id == ws.base || walked >= self.config.max_stack {
                        break;
                    }
                    walked += 1;
                    let Some((content, record)) = meta.changeset(repo, id).await? else {
                        break;
                    };
                    if matches!(record.state, RecordState::Landed | RecordState::Collected) {
                        break;
                    }
                    if held_by(&record, &ws.name) {
                        plan.abandoned.insert(id);
                        lapse_expiry.insert(id, expires);
                    }
                    at = shape.parents(&content)?.first().copied();
                }
            }
        }

        // Collect: expired records with no pins, then the parents they
        // unpin, to the fixpoint.
        let mut work: Vec<ChangesetId> = Vec::new();
        let mut after = None;
        loop {
            let page = meta.expired_records(repo, now_ms, after, batch).await?;
            let Some(last) = page.last() else {
                break;
            };
            after = Some(*last);
            work.extend(page.into_iter().map(|(_, id)| id));
        }
        for (id, expires) in &lapse_expiry {
            if *expires <= now_ms
                && meta
                    .protection(repo, *id)
                    .await?
                    .is_some_and(|p| p.pins == 0)
            {
                work.push(*id);
            }
        }
        let mut pins_left: HashMap<ChangesetId, u32> = HashMap::new();
        let mut successor_of: Vec<(ChangesetId, Vec<u8>, ChangesetId)> = Vec::new();
        while let Some(id) = work.pop() {
            if plan.changesets.contains(&id) {
                continue;
            }
            let Some((content, record)) = meta.changeset(repo, id).await? else {
                continue;
            };
            if matches!(record.state, RecordState::Landed | RecordState::Collected) {
                continue;
            }
            plan.changesets.insert(id);
            if record.state == RecordState::Superseded
                && let Some(s) = record.superseded_by
            {
                successor_of.push((id, content.clone(), s));
            }
            for p in distinct(id, shape.parents(&content)?) {
                let Some((_, parent)) = meta.changeset(repo, p).await? else {
                    continue;
                };
                if matches!(parent.state, RecordState::Landed | RecordState::Collected) {
                    continue;
                }
                let Some(protection) = meta.protection(repo, p).await? else {
                    continue;
                };
                let left = pins_left.entry(p).or_insert(protection.pins);
                *left = left.saturating_sub(1);
                let expires = lapse_expiry.get(&p).copied().or(protection.expires_at_ms);
                if *left == 0 && expires.is_some_and(|e| e <= now_ms) {
                    work.push(p);
                }
            }
        }

        // Rows: those of collected records, less what moves to a successor
        // that stays, and the expired upload and claim rows.
        let mut gone: HashSet<(ObjectId, String)> = HashSet::new();
        let mut kept: HashSet<ObjectId> = HashSet::new();
        let mut candidates: BTreeSet<ObjectId> = BTreeSet::new();
        let mut rows_of: HashMap<ChangesetId, Vec<ObjectId>> = HashMap::new();
        for id in &plan.changesets {
            let rows = held_objects(meta, repo, &id.to_string(), batch).await?;
            for o in &rows {
                gone.insert((*o, id.to_string()));
                candidates.insert(*o);
            }
            rows_of.insert(*id, rows);
        }
        for (id, content, s) in successor_of {
            let rows = &rows_of[&id];
            if rows.is_empty() || plan.changesets.contains(&s) {
                continue;
            }
            let Some((to, record)) = meta.changeset(repo, s).await? else {
                continue;
            };
            if record.state == RecordState::Collected {
                continue;
            }
            kept.extend(shape.retained(repo, &content, &to, rows).await?);
        }
        let mut after = None;
        loop {
            let page = meta.expired_rows(repo, now_ms, after, batch).await?;
            let Some(last) = page.last().cloned() else {
                break;
            };
            after = Some(last);
            for row in page {
                candidates.insert(row.object);
                gone.insert((row.object, row.holder));
            }
        }
        for o in candidates {
            if kept.contains(&o) {
                continue;
            }
            let holders = holder_names(meta, repo, o, batch).await?;
            if holders.iter().all(|h| gone.contains(&(o, h.clone()))) {
                if let Some(frame) = self.objects.get(repo, o).await? {
                    plan.bytes += frame.len() as u64;
                }
                plan.doomed.insert(o);
            }
        }

        // Delete: what is already past the grace period.
        let until = now_ms.saturating_sub(ms(self.config.delete_grace));
        let mut after = None;
        loop {
            let page = meta.old_doomed(repo, until, after, batch).await?;
            let Some((last, mark)) = page.last() else {
                break;
            };
            after = Some((mark.since_ms, *last));
            plan.deletable += page.len() as u64;
        }
        Ok(plan)
    }

    /// `--verify`: the plan at `now_ms`, checked against the ancestry walk.
    /// The check reads every non-landed record and what each introduced,
    /// so it costs that population, as the G2.9 walk does.
    pub async fn verify<S: Shape>(
        &self,
        repo: RepoId,
        now_ms: i64,
        shape: &S,
    ) -> Result<Verify, GcError> {
        let meta = &self.meta;
        let batch = self.batch();
        let plan = self.plan(repo, now_ms, shape).await?;
        let walk = ancestry(meta, &self.objects, &self.config, repo, now_ms, shape).await?;

        let mut kept = Vec::new();
        for state in [
            RecordState::Draft,
            RecordState::Superseded,
            RecordState::Abandoned,
        ] {
            let mut after = None;
            loop {
                let page = meta.list_records(repo, state, after, batch).await?;
                let Some((last, _)) = page.last() else {
                    break;
                };
                after = Some(*last);
                kept.extend(
                    page.into_iter()
                        .map(|(id, _)| id)
                        .filter(|id| !plan.changesets.contains(id)),
                );
            }
        }

        // Every changeset a kept one reaches through parents, the kept
        // ones' own parents included.
        let mut parents = Vec::new();
        for id in &kept {
            if let Some((content, _)) = meta.changeset(repo, *id).await? {
                parents.extend(distinct(*id, shape.parents(&content)?));
            }
        }
        let reached = descendants_reach(meta, repo, shape, parents).await?;
        let unsafe_changesets = plan
            .changesets
            .iter()
            .copied()
            .filter(|id| reached.contains(id))
            .collect();

        let mut unsafe_objects = BTreeSet::new();
        for id in &kept {
            let Some((content, _)) = meta.changeset(repo, *id).await? else {
                continue;
            };
            for o in shape.introduced(repo, *id, &content).await? {
                if plan.doomed.contains(&o) {
                    unsafe_objects.insert(o);
                }
            }
        }

        Ok(Verify {
            both: plan
                .changesets
                .iter()
                .filter(|id| walk.collectable.contains(id))
                .count() as u64,
            ancestry: walk.report(),
            unsafe_changesets,
            unsafe_objects: unsafe_objects.into_iter().collect(),
            plan,
        })
    }
}
