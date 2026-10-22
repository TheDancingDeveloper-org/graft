//! Draft expiry and incremental object collection (`docs/design.md` §8.6).
//! G2.9 adds the dry-run report; G3.2 adds the sweep.

use std::time::Duration;

use graft_store::{MetaStore, ObjectStore, RepoId, StoreResult};

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
    /// Objects doomed.
    pub doomed: u64,
    /// Objects deleted.
    pub deleted: u64,
    /// Bytes reclaimed.
    pub bytes: u64,
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

    /// Runs one pass: mark, doom, delete past grace. With `dry_run`, reports
    /// only.
    pub async fn run(&self, _repo: RepoId, _now_ms: i64, _dry_run: bool) -> StoreResult<Report> {
        todo!("G2.9 (dry run), G3.2 (sweep)")
    }
}
