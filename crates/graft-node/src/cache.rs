//! The disk cache: a filesystem frame store in LRU mode (`graft-store-fs`)
//! under `<cache_dir>/objects`, and the repository index beside it.
//!
//! It is only a cache. Every read is verified against its id by the store;
//! a frame that fails is deleted and reported as a miss, so it is fetched
//! again. Only frames the node has verified are written. A crash mid-write
//! leaves at most temp files, which the store removes when it opens, and an
//! index that does not parse wipes the whole cache.
//!
//! Store calls do blocking file I/O (and the fsyncs of a write), so they run
//! on the blocking pool, never on a runtime worker.

use std::path::Path;
use std::sync::Arc;

use graft_model::{ObjectId, ObjectKind};
use graft_proto::frame::Frame;
use graft_store::{ObjectStore as _, StoreError};
use graft_store_fs::{FsMode, FsObjectStore};

use crate::metrics::NodeMetrics;
use crate::repos::{AddError, Repos};

/// The frame store's directory under the cache directory.
const OBJECTS: &str = "objects";

/// The repository index under the cache directory.
const REPOS: &str = "repos";

/// The node's object cache.
#[derive(Debug)]
pub(crate) struct Cache {
    store: FsObjectStore,
    repos: Repos,
    min_free: u64,
    metrics: NodeMetrics,
}

/// Why a fetched frame was refused.
#[derive(Debug, PartialEq, Eq)]
pub(crate) struct Corrupt;

impl Cache {
    /// Opens the cache at `dir`, wiping it if its index is unreadable.
    pub(crate) fn open(
        dir: &Path,
        max_bytes: u64,
        min_free: u64,
        max_repos: u32,
        metrics: NodeMetrics,
    ) -> Result<Self, String> {
        std::fs::create_dir_all(dir).map_err(|e| format!("create {}: {e}", dir.display()))?;
        let index = dir.join(REPOS);
        let objects = dir.join(OBJECTS);
        let repos = match Repos::load(&index, max_repos) {
            Ok(repos) => repos,
            Err(why) => {
                tracing::warn!(%why, "cache index unreadable; wiping the cache");
                wipe(&objects, &index)?;
                Repos::load(&index, max_repos)?
            }
        };
        let store = FsObjectStore::open(&objects, FsMode::Lru { max_bytes })
            .map_err(|e| format!("open {}: {e:?}", objects.display()))?;
        Ok(Self {
            store,
            repos,
            min_free,
            metrics,
        })
    }

    /// The cached frame bytes of `id` in `repo`, verified. A corrupt entry is
    /// deleted and reads as a miss. Blocking.
    pub(crate) fn get(&self, repo: &str, id: ObjectId) -> Option<Vec<u8>> {
        let rid = self.repos.get(repo)?;
        match now(self.store.get(rid, id)) {
            Ok(found) => found,
            Err(StoreError::Corrupt(_)) => {
                self.metrics.corrupt.with_label_values(&["cache"]).inc();
                tracing::warn!(repo, id = %id.to_hex(), "corrupt cache entry; discarding");
                if let Err(e) = now(self.store.delete(rid, id)) {
                    tracing::warn!(error = ?e, "discard of a corrupt cache entry failed");
                }
                None
            }
            Err(e) => {
                tracing::warn!(error = ?e, "cache read failed");
                None
            }
        }
    }

    /// Whether each of `ids` has an entry, without reading it ([`Cache::get`]
    /// reads and verifies). Blocking.
    pub(crate) fn has(&self, repo: &str, ids: &[ObjectId]) -> Vec<bool> {
        let Some(rid) = self.repos.get(repo) else {
            return vec![false; ids.len()];
        };
        match now(self.store.has(rid, ids)) {
            Ok(present) => {
                let present: std::collections::HashSet<ObjectId> = present.into_iter().collect();
                ids.iter().map(|id| present.contains(id)).collect()
            }
            Err(e) => {
                tracing::warn!(error = ?e, "cache lookup failed");
                vec![false; ids.len()]
            }
        }
    }

    /// Caches the frame of `id`, which the caller has verified, when there
    /// is room. Caching is best effort: a full disk or a write error leaves
    /// the answer correct, just uncached. Blocking.
    pub(crate) fn store(&self, repo: &str, id: ObjectId, bytes: &[u8]) {
        match now(self.store.stats()) {
            Ok(stats) if stats.free_bytes < self.min_free => {
                self.skip("disk");
                return;
            }
            Ok(_) => {}
            Err(e) => {
                tracing::warn!(error = ?e, "cache stats failed");
                self.skip("error");
                return;
            }
        }
        let rid = match self.repos.get_or_add(repo) {
            Ok(rid) => rid,
            Err(AddError::Io(e)) => {
                tracing::warn!(error = %e, "cache index write failed");
                self.skip("error");
                return;
            }
            Err(AddError::Full | AddError::BadName) => {
                self.skip("repos");
                return;
            }
        };
        if let Err(e) = now(self.store.put(rid, id, bytes.to_vec())) {
            tracing::warn!(error = ?e, "cache write failed");
            self.skip("error");
        }
    }

    fn skip(&self, reason: &str) {
        self.metrics
            .cache_skipped
            .with_label_values(&[reason])
            .inc();
    }
}

/// Checks that `bytes` is a blob or tree frame that decodes to `id`.
pub(crate) fn verify(id: ObjectId, bytes: &[u8]) -> Result<(), Corrupt> {
    let frame = Frame::from_bytes(bytes).map_err(|_| Corrupt)?;
    if frame.kind == ObjectKind::Changeset {
        return Err(Corrupt);
    }
    frame.decode_verified(id).map(|_| ()).map_err(|_| Corrupt)
}

/// Runs a store call to completion. Callers are on the blocking pool (see
/// [`blocking`]), where waiting on the runtime is allowed.
fn now<T>(fut: impl std::future::Future<Output = T>) -> T {
    tokio::runtime::Handle::current().block_on(fut)
}

fn wipe(objects: &Path, index: &Path) -> Result<(), String> {
    for path in [objects, index] {
        let result = if path.is_dir() {
            std::fs::remove_dir_all(path)
        } else {
            std::fs::remove_file(path)
        };
        match result {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(format!("wipe {}: {e}", path.display())),
        }
    }
    Ok(())
}

/// Runs `f` against the cache on the blocking pool.
pub(crate) async fn blocking<T, F>(cache: &Arc<Cache>, f: F) -> Option<T>
where
    T: Send + 'static,
    F: FnOnce(&Cache) -> T + Send + 'static,
{
    let cache = cache.clone();
    tokio::task::spawn_blocking(move || f(&cache)).await.ok()
}
