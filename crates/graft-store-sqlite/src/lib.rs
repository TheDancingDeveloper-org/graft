//! SQLite `MetaStore` (`docs/design.md` §6.5, work item G1.6).
//!
//! WAL mode, `synchronous=FULL`, one writer thread behind a bounded queue,
//! a reader pool, versioned migrations.

use std::path::PathBuf;

use graft_model::{ChangesetId, ObjectId, RepoPath};
use graft_store::{
    ChangesetRecord, Lsn, MetaStore, MetaTxn, RefRecord, RepoId, StoreResult, TxnOutcome,
    WorkspaceRecord,
};

pub mod migrations;

/// Store configuration.
#[derive(Clone, Debug)]
pub struct SqliteConfig {
    /// Database file.
    pub path: PathBuf,
    /// Writer queue bound; a full queue yields `StoreError::Busy`.
    pub writer_queue: usize,
    /// Reader connections.
    pub readers: usize,
}

/// A SQLite metadata store.
#[derive(Debug)]
pub struct SqliteMetaStore {
    config: SqliteConfig,
}

impl SqliteMetaStore {
    /// Opens the database, running migrations.
    pub fn open(config: SqliteConfig) -> StoreResult<Self> {
        Ok(Self { config })
    }

    /// The configuration in use.
    pub fn config(&self) -> &SqliteConfig {
        &self.config
    }
}

impl MetaStore for SqliteMetaStore {
    async fn create_repo(&self, _name: &str) -> StoreResult<RepoId> {
        todo!("G1.6")
    }
    async fn repo(&self, _name: &str) -> StoreResult<Option<RepoId>> {
        todo!("G1.6")
    }
    async fn commit(&self, _repo: RepoId, _txn: MetaTxn) -> StoreResult<TxnOutcome> {
        todo!("G1.6")
    }
    async fn changeset(
        &self,
        _repo: RepoId,
        _id: ChangesetId,
    ) -> StoreResult<Option<(Vec<u8>, ChangesetRecord)>> {
        todo!("G1.6")
    }
    async fn get_ref(&self, _repo: RepoId, _name: &str) -> StoreResult<Option<RefRecord>> {
        todo!("G1.6")
    }
    async fn list_refs(
        &self,
        _repo: RepoId,
        _prefix: &str,
        _limit: u32,
    ) -> StoreResult<Vec<RefRecord>> {
        todo!("G1.6")
    }
    async fn workspace(&self, _repo: RepoId, _name: &str) -> StoreResult<Option<WorkspaceRecord>> {
        todo!("G1.6")
    }
    async fn expired_workspaces(
        &self,
        _repo: RepoId,
        _before_ms: i64,
        _limit: u32,
    ) -> StoreResult<Vec<WorkspaceRecord>> {
        todo!("G1.6")
    }
    async fn derived(
        &self,
        _repo: RepoId,
        _kind: &str,
        _version: u32,
        _key: &[u8],
    ) -> StoreResult<Option<Vec<u8>>> {
        todo!("G1.6")
    }
    async fn line_pos(
        &self,
        _repo: RepoId,
        _line: &str,
        _id: ChangesetId,
    ) -> StoreResult<Option<u64>> {
        todo!("G1.6")
    }
    async fn scan_line_path(
        &self,
        _repo: RepoId,
        _line: &str,
        _path: &RepoPath,
        _before: u64,
        _limit: u32,
    ) -> StoreResult<Vec<(u64, ChangesetId)>> {
        todo!("G1.6")
    }
    async fn holders(&self, _repo: RepoId, _object: ObjectId) -> StoreResult<u32> {
        todo!("G1.6")
    }
    async fn land(
        &self,
        _repo: RepoId,
        _draft: ChangesetId,
        _line: &str,
    ) -> StoreResult<Option<ChangesetId>> {
        todo!("G1.6")
    }
    async fn journal(
        &self,
        _repo: RepoId,
        _from: Lsn,
        _limit: u32,
    ) -> StoreResult<Vec<(Lsn, String, Vec<u8>)>> {
        todo!("G1.6")
    }
}
