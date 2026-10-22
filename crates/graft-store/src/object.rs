//! The `ObjectStore` port.

use std::future::Future;

use graft_model::ObjectId;

use crate::{RepoId, StoreResult};

/// Capacity figures for metrics and admission control.
#[derive(Clone, Copy, Debug, Default)]
pub struct StoreStats {
    /// Stored objects.
    pub objects: u64,
    /// Stored bytes (encoded).
    pub bytes: u64,
    /// Free bytes on the backing volume.
    pub free_bytes: u64,
}

/// Content-addressed frame storage. Frames are opaque bytes here; callers
/// (engine, node) verify them against their ids.
pub trait ObjectStore: Send + Sync {
    /// Put-if-absent; durable on return; idempotent.
    fn put(
        &self,
        repo: RepoId,
        id: ObjectId,
        frame: Vec<u8>,
    ) -> impl Future<Output = StoreResult<()>> + Send;
    /// The frame, or `None`.
    fn get(
        &self,
        repo: RepoId,
        id: ObjectId,
    ) -> impl Future<Output = StoreResult<Option<Vec<u8>>>> + Send;
    /// The subset of `ids` that is present.
    fn has(
        &self,
        repo: RepoId,
        ids: &[ObjectId],
    ) -> impl Future<Output = StoreResult<Vec<ObjectId>>> + Send;
    /// Removes an object (GC only); idempotent.
    fn delete(&self, repo: RepoId, id: ObjectId) -> impl Future<Output = StoreResult<()>> + Send;
    /// Capacity figures.
    fn stats(&self) -> impl Future<Output = StoreResult<StoreStats>> + Send;
}

impl<T: ObjectStore> ObjectStore for std::sync::Arc<T> {
    fn put(
        &self,
        repo: RepoId,
        id: ObjectId,
        frame: Vec<u8>,
    ) -> impl Future<Output = StoreResult<()>> + Send {
        (**self).put(repo, id, frame)
    }
    fn get(
        &self,
        repo: RepoId,
        id: ObjectId,
    ) -> impl Future<Output = StoreResult<Option<Vec<u8>>>> + Send {
        (**self).get(repo, id)
    }
    fn has(
        &self,
        repo: RepoId,
        ids: &[ObjectId],
    ) -> impl Future<Output = StoreResult<Vec<ObjectId>>> + Send {
        (**self).has(repo, ids)
    }
    fn delete(&self, repo: RepoId, id: ObjectId) -> impl Future<Output = StoreResult<()>> + Send {
        (**self).delete(repo, id)
    }
    fn stats(&self) -> impl Future<Output = StoreResult<StoreStats>> + Send {
        (**self).stats()
    }
}
