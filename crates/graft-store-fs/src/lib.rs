//! Filesystem `ObjectStore` (`docs/design.md` §6.4, work item G1.5).
//!
//! Layout `<root>/<repo>/objects/<2 hex>/<62 hex>`; writes are temp file,
//! fsync, no-replace rename, directory fsync. Used by the server
//! (authoritative mode) and by the node daemon and client (LRU mode).

use std::path::PathBuf;

use graft_model::ObjectId;
use graft_store::{ObjectStore, RepoId, StoreResult, StoreStats};

/// How the store treats capacity.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FsMode {
    /// Never evicts (server).
    Authoritative,
    /// Evicts least-recently-used frames above `max_bytes` (node, client).
    Lru {
        /// Byte bound.
        max_bytes: u64,
    },
}

/// A filesystem object store.
#[derive(Debug)]
pub struct FsObjectStore {
    root: PathBuf,
    mode: FsMode,
}

impl FsObjectStore {
    /// Opens (creating if needed) a store at `root`, removing stray temp files.
    pub fn open(root: impl Into<PathBuf>, mode: FsMode) -> StoreResult<Self> {
        Ok(Self {
            root: root.into(),
            mode,
        })
    }

    /// The store root.
    pub fn root(&self) -> &std::path::Path {
        &self.root
    }

    /// The capacity mode.
    pub fn mode(&self) -> FsMode {
        self.mode
    }
}

impl ObjectStore for FsObjectStore {
    async fn put(&self, _repo: RepoId, _id: ObjectId, _frame: Vec<u8>) -> StoreResult<()> {
        todo!("G1.5")
    }
    async fn get(&self, _repo: RepoId, _id: ObjectId) -> StoreResult<Option<Vec<u8>>> {
        todo!("G1.5")
    }
    async fn has(&self, _repo: RepoId, _ids: &[ObjectId]) -> StoreResult<Vec<ObjectId>> {
        todo!("G1.5")
    }
    async fn delete(&self, _repo: RepoId, _id: ObjectId) -> StoreResult<()> {
        todo!("G1.5")
    }
    async fn stats(&self) -> StoreResult<StoreStats> {
        todo!("G1.5")
    }
}
