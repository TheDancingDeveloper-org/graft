//! The G2.9 dry run: what would be collected, found by walking ancestry
//! from the live drafts. It works on any repository, with or without
//! protection records, and is the oracle `--verify` checks the protection
//! plan against.
//!
//! The report reads only what the store indexes: the changesets of one
//! record state, the workspaces by name, the holder rows of one object, and
//! the changesets an expired candidate's successors and ancestors name. It
//! never scans the journal or every changeset.

use std::collections::HashSet;

use graft_model::{ChangesetId, ObjectId};
use graft_store::{MetaStore, ObjectStore, RecordState, RepoId, StoreResult};

use crate::{GcConfig, Report, Shape, ms};

/// What the ancestry walk found, as sets.
#[derive(Clone, Debug, Default)]
pub(crate) struct Ancestry {
    /// Candidates nothing live protects.
    pub(crate) collectable: HashSet<ChangesetId>,
    /// Objects every holder of which is collectable, with a stored frame.
    pub(crate) doomed: HashSet<ObjectId>,
    /// Their frame bytes.
    pub(crate) bytes: u64,
}

impl Ancestry {
    pub(crate) fn report(&self) -> Report {
        Report {
            changesets: self.collectable.len() as u64,
            doomed: self.doomed.len() as u64,
            deleted: 0,
            bytes: self.bytes,
        }
    }
}

/// The dry-run report over borrowed stores, so the engine can run it without
/// giving them up.
///
/// Candidates are drafts whose workspace lapsed more than the retention ago,
/// and superseded or abandoned changesets kept longer than it. A draft whose
/// workspace is missing is kept, since nothing says it was closed. A
/// candidate is protected when a live draft descends from it (walking through
/// non-landed changesets) or when its successor chain reaches a changeset
/// that is not a candidate. An object is doomed when it has holder rows, all
/// of them collectable, and the frame is stored.
pub async fn dry_run_report<M: MetaStore, O: ObjectStore, S: Shape>(
    meta: &M,
    objects: &O,
    config: &GcConfig,
    repo: RepoId,
    now_ms: i64,
    shape: &S,
) -> StoreResult<Report> {
    Ok(ancestry(meta, objects, config, repo, now_ms, shape)
        .await?
        .report())
}

pub(crate) async fn ancestry<M: MetaStore, O: ObjectStore, S: Shape>(
    meta: &M,
    objects: &O,
    config: &GcConfig,
    repo: RepoId,
    now_ms: i64,
    shape: &S,
) -> StoreResult<Ancestry> {
    let retention = ms(config.draft_retention);
    let batch = config.batch.max(1);

    let mut candidates = HashSet::new();
    let mut successors = Vec::new();
    let mut live_drafts = Vec::new();
    let mut after = None;
    loop {
        let page = meta
            .list_records(repo, RecordState::Draft, after, batch)
            .await?;
        let Some(last) = page.last().map(|(id, _)| *id) else {
            break;
        };
        after = Some(last);
        for (id, record) in page {
            let lapsed = match record.holder.as_deref().and_then(|h| h.strip_prefix("ws:")) {
                Some(name) => meta
                    .workspace(repo, name)
                    .await?
                    .is_some_and(|ws| ws.lease_until_ms.saturating_add(retention) <= now_ms),
                None => false,
            };
            if lapsed {
                candidates.insert(id);
            } else {
                live_drafts.push(id);
            }
        }
    }
    for state in [RecordState::Superseded, RecordState::Abandoned] {
        let mut after = None;
        loop {
            let page = meta.list_records(repo, state, after, batch).await?;
            let Some(last) = page.last().map(|(id, _)| *id) else {
                break;
            };
            after = Some(last);
            for (id, record) in page {
                if record.received_at_ms.saturating_add(retention) <= now_ms {
                    candidates.insert(id);
                    if state == RecordState::Superseded {
                        successors.push((id, record.superseded_by));
                    }
                }
            }
        }
    }

    // Ancestors of live drafts, stopping at landed changesets.
    let mut protected = descendants_reach(meta, repo, shape, live_drafts).await?;
    for (id, successor) in successors {
        if reaches_live(meta, repo, &candidates, successor).await? {
            protected.insert(id);
        }
    }

    let collectable: HashSet<ChangesetId> = candidates
        .iter()
        .copied()
        .filter(|id| !protected.contains(id))
        .collect();

    // Only objects a collectable changeset introduced can be doomed.
    let mut introduced = HashSet::new();
    for id in &collectable {
        let Some((content, _)) = meta.changeset(repo, *id).await? else {
            continue;
        };
        introduced.extend(shape.introduced(repo, *id, &content).await?);
    }

    let mut out = Ancestry {
        collectable,
        ..Ancestry::default()
    };
    for object in introduced {
        let rows = holder_names(meta, repo, object, batch).await?;
        if rows.is_empty() || !rows.iter().all(|r| is_gone(r, &out.collectable, now_ms)) {
            continue;
        }
        if let Some(frame) = objects.get(repo, object).await? {
            out.doomed.insert(object);
            out.bytes += frame.len() as u64;
        }
    }
    Ok(out)
}

/// Every changeset `from` reaches through parents, stopping at landed ones
/// (which are in the set, though nothing collects them): the set the live
/// changesets in `from` protect.
pub(crate) async fn descendants_reach<M: MetaStore, S: Shape>(
    meta: &M,
    repo: RepoId,
    shape: &S,
    from: Vec<ChangesetId>,
) -> StoreResult<HashSet<ChangesetId>> {
    let mut reached = HashSet::new();
    let mut stack = from;
    while let Some(id) = stack.pop() {
        if !reached.insert(id) {
            continue;
        }
        let Some((content, record)) = meta.changeset(repo, id).await? else {
            continue;
        };
        if record.state == RecordState::Landed {
            continue;
        }
        stack.extend(shape.parents(&content)?);
    }
    Ok(reached)
}

/// Whether following `successor` through superseded changesets reaches one
/// outside `candidates`, or a broken link.
async fn reaches_live<M: MetaStore>(
    meta: &M,
    repo: RepoId,
    candidates: &HashSet<ChangesetId>,
    mut successor: Option<ChangesetId>,
) -> StoreResult<bool> {
    let mut seen = HashSet::new();
    while let Some(id) = successor {
        if !candidates.contains(&id) || !seen.insert(id) {
            return Ok(true);
        }
        successor = match meta.changeset(repo, id).await? {
            Some((_, record)) => record.superseded_by,
            None => return Ok(true),
        };
    }
    Ok(false)
}

/// Every holder name of an object, paged.
pub(crate) async fn holder_names<M: MetaStore>(
    meta: &M,
    repo: RepoId,
    object: ObjectId,
    batch: u32,
) -> StoreResult<Vec<String>> {
    let mut names = Vec::new();
    let mut after = None;
    loop {
        let page = meta.holder_rows(repo, object, after.clone(), batch).await?;
        let Some(last) = page.last().cloned() else {
            return Ok(names);
        };
        after = Some(last);
        names.extend(page);
    }
}

/// Whether a holder row names something the pass would collect: a
/// collectable changeset, or an upload whose lease has lapsed. Anything
/// else (a live workspace, a land attempt, a landed changeset) keeps the
/// object.
fn is_gone(holder: &str, collectable: &HashSet<ChangesetId>, now_ms: i64) -> bool {
    if let Ok(id) = holder.parse::<ChangesetId>() {
        return collectable.contains(&id);
    }
    holder
        .strip_prefix("upload:")
        .and_then(|rest| rest.split_once(':'))
        .and_then(|(expires, _)| expires.parse::<i64>().ok())
        .is_some_and(|expires| expires <= now_ms)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn uploads_are_gone_once_their_lease_lapses() {
        let none = HashSet::new();
        assert!(is_gone("upload:100:alice", &none, 100));
        assert!(!is_gone("upload:101:alice", &none, 100));
        assert!(!is_gone("ws:w", &none, 100));
        assert!(!is_gone("land:1", &none, 100));
    }

    #[test]
    fn changesets_are_gone_only_when_collectable() {
        let id = ChangesetId(ObjectId([7; 32]));
        let mut set = HashSet::new();
        assert!(!is_gone(&id.to_string(), &set, 0));
        set.insert(id);
        assert!(is_gone(&id.to_string(), &set, 0));
    }
}
