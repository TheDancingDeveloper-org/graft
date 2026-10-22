//! Draft expiry and incremental object collection (`docs/design.md` §8.6).
//! G2.9 adds the dry-run report; the sweep (G3.2) is not built, and `run`
//! refuses it.
//!
//! The report reads only what the store indexes: the changesets of one
//! record state, the workspaces by name, the holder rows of one object, and
//! the changesets an expired candidate's successors and ancestors name. It
//! never scans the journal or every changeset.

use std::collections::HashSet;
use std::future::Future;
use std::time::Duration;

use graft_model::{ChangesetId, ObjectId};
use graft_store::{MetaStore, ObjectStore, RecordState, RepoId, StoreError, StoreResult};

/// Collection timings.
#[derive(Clone, Debug)]
pub struct GcConfig {
    /// How long abandoned and superseded drafts are kept.
    pub draft_retention: Duration,
    /// How long a losing land candidate is kept.
    pub land_attempt_lease: Duration,
    /// How long an unclaimed upload is kept.
    pub upload_lease: Duration,
    /// Doomed-to-deleted grace; longer than any request deadline.
    pub delete_grace: Duration,
    /// Rows per batch.
    pub batch: u32,
}

impl Default for GcConfig {
    fn default() -> Self {
        Self {
            draft_retention: Duration::from_secs(7 * 24 * 3600),
            land_attempt_lease: Duration::from_secs(600),
            upload_lease: Duration::from_secs(3600),
            delete_grace: Duration::from_secs(900),
            batch: 1000,
        }
    }
}

/// What a pass found or did.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Report {
    /// Changesets collected (or collectable).
    pub changesets: u64,
    /// Objects doomed (or doomable). Only objects with a stored frame count.
    pub doomed: u64,
    /// Objects deleted. Always zero in a dry run.
    pub deleted: u64,
    /// Bytes reclaimed (or reclaimable): the frames of the doomed objects.
    pub bytes: u64,
}

/// What the collector needs from a changeset's stored content, which only
/// the engine can decode.
pub trait Shape: Send + Sync {
    /// The parents of a stored changeset, read from its content.
    fn parents(&self, content: &[u8]) -> StoreResult<Vec<ChangesetId>>;

    /// The objects a changeset introduced: the ones it holds rows for. The
    /// answer may be a superset, because an object is doomed only when every
    /// one of its stored holder rows is collectable. It must not omit an
    /// object the changeset holds a row for, or that object is never
    /// reported.
    fn introduced(
        &self,
        repo: RepoId,
        id: ChangesetId,
        content: &[u8],
    ) -> impl Future<Output = StoreResult<Vec<ObjectId>>> + Send;
}

/// The collector for one repository.
#[derive(Debug)]
pub struct Collector<M, O> {
    meta: M,
    objects: O,
    config: GcConfig,
}

impl<M: MetaStore, O: ObjectStore> Collector<M, O> {
    /// Creates a collector.
    pub fn new(meta: M, objects: O, config: GcConfig) -> Self {
        Self {
            meta,
            objects,
            config,
        }
    }

    /// The configuration.
    pub fn config(&self) -> &GcConfig {
        &self.config
    }

    /// The stores.
    pub fn stores(&self) -> (&M, &O) {
        (&self.meta, &self.objects)
    }

    /// Runs one pass at `now_ms`. With `dry_run`, reports only. A sweep is
    /// refused until G3.2 builds it, so no path here deletes.
    pub async fn run<S: Shape>(
        &self,
        repo: RepoId,
        now_ms: i64,
        dry_run: bool,
        shape: &S,
    ) -> StoreResult<Report> {
        if !dry_run {
            return Err(StoreError::Invalid(
                "the GC sweep is not built yet (G3.2)".to_owned(),
            ));
        }
        dry_run_report(&self.meta, &self.objects, &self.config, repo, now_ms, shape).await
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
    let retention = i64::try_from(config.draft_retention.as_millis()).unwrap_or(i64::MAX);
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
    let mut protected = HashSet::new();
    let mut stack = live_drafts;
    while let Some(id) = stack.pop() {
        if !protected.insert(id) {
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

    let mut report = Report {
        changesets: collectable.len() as u64,
        ..Report::default()
    };
    for object in introduced {
        let rows = holder_names(meta, repo, object, batch).await?;
        if rows.is_empty() || !rows.iter().all(|r| is_gone(r, &collectable, now_ms)) {
            continue;
        }
        if let Some(frame) = objects.get(repo, object).await? {
            report.doomed += 1;
            report.bytes += frame.len() as u64;
        }
    }
    Ok(report)
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
async fn holder_names<M: MetaStore>(
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

    #[test]
    fn the_default_timings_match_the_design() {
        let c = GcConfig::default();
        assert_eq!(c.draft_retention, Duration::from_secs(7 * 24 * 3600));
        assert_eq!(c.land_attempt_lease, Duration::from_secs(600));
        assert_eq!(c.upload_lease, Duration::from_secs(3600));
        assert_eq!(c.delete_grace, Duration::from_secs(900));
    }
}
