//! SQLite `MetaStore` (`docs/design.md` §6.5, work item G1.6).
//!
//! WAL mode, `synchronous=FULL`, `foreign_keys=ON`. One writer connection on
//! a dedicated thread behind a bounded queue (see [`writer`] for how it
//! groups commits), plus a pool of reader threads, each with its own
//! connection. A full queue is backpressure: the call fails at once with
//! `StoreError::Busy(retry_after_ms)` rather than waiting. The schema is
//! versioned and migrated on open ([`migrations`]).

use std::path::PathBuf;
use std::sync::mpsc::{self, SyncSender, TrySendError};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::Duration;

use graft_model::{ChangesetId, ObjectId, RepoPath};
use graft_store::{
    ChangesetRecord, Lsn, MetaStore, MetaTxn, RecordState, RefRecord, RepoId, StoreError,
    StoreResult, TxnOutcome, WorkspaceRecord,
};
use rusqlite::{Connection, OpenFlags};

mod codec;
pub mod migrations;
mod oneshot;
mod reader;
mod writer;

use codec::sql_err;
use reader::ReadJob;
use writer::{WriteJob, WriteOp, Written};

/// The retry hint, in milliseconds, for a full queue or a busy database.
pub const DEFAULT_RETRY_MS: u64 = 10;

/// How long a connection waits on a SQLite lock before reporting busy.
const LOCK_TIMEOUT: Duration = Duration::from_secs(5);

/// Store configuration.
#[derive(Clone, Debug)]
pub struct SqliteConfig {
    /// Database file.
    pub path: PathBuf,
    /// Writer queue bound; a full queue yields `StoreError::Busy`.
    pub writer_queue: usize,
    /// Reader connections.
    pub readers: usize,
    /// Reader queue bound; a full queue yields `StoreError::Busy`.
    pub reader_queue: usize,
    /// The retry hint returned with `StoreError::Busy`, in milliseconds.
    pub retry_after_ms: u64,
}

impl SqliteConfig {
    /// The defaults for a database at `path`.
    pub fn new(path: impl Into<PathBuf>) -> Self {
        Self {
            path: path.into(),
            writer_queue: 1024,
            readers: 4,
            reader_queue: 1024,
            retry_after_ms: DEFAULT_RETRY_MS,
        }
    }
}

/// A SQLite metadata store.
#[derive(Debug)]
pub struct SqliteMetaStore {
    config: SqliteConfig,
    writer: Option<SyncSender<WriteJob>>,
    readers: Option<SyncSender<ReadJob>>,
    threads: Vec<JoinHandle<()>>,
}

/// Opens a connection with the store's pragmas.
fn connect(config: &SqliteConfig, flags: OpenFlags) -> StoreResult<Connection> {
    let conn = Connection::open_with_flags(&config.path, flags).map_err(sql_err)?;
    conn.busy_timeout(LOCK_TIMEOUT).map_err(sql_err)?;
    let mode: String = conn
        .pragma_update_and_check(None, "journal_mode", "WAL", |r| r.get(0))
        .map_err(sql_err)?;
    if !mode.eq_ignore_ascii_case("wal") {
        return Err(StoreError::Backend(format!(
            "sqlite refused WAL mode ({mode})"
        )));
    }
    conn.pragma_update(None, "synchronous", "FULL")
        .map_err(sql_err)?;
    conn.pragma_update(None, "foreign_keys", "ON")
        .map_err(sql_err)?;
    Ok(conn)
}

impl SqliteMetaStore {
    /// Opens (or creates) the database, runs migrations, and starts the
    /// writer and reader threads.
    pub fn open(config: SqliteConfig) -> StoreResult<Self> {
        let flags = OpenFlags::SQLITE_OPEN_READ_WRITE
            | OpenFlags::SQLITE_OPEN_CREATE
            | OpenFlags::SQLITE_OPEN_NO_MUTEX;
        let mut write_conn = connect(&config, flags)?;
        migrations::migrate(&mut write_conn)?;

        let mut read_conns = Vec::with_capacity(config.readers.max(1));
        for _ in 0..config.readers.max(1) {
            let conn = connect(&config, flags)?;
            conn.pragma_update(None, "query_only", "ON")
                .map_err(sql_err)?;
            read_conns.push(conn);
        }

        let mut threads = Vec::new();
        let (write_tx, write_rx) = mpsc::sync_channel(config.writer_queue.max(1));
        threads.push(spawn("graft-sqlite-writer", move || {
            writer::run(write_conn, write_rx)
        })?);
        let (read_tx, read_rx) = mpsc::sync_channel(config.reader_queue.max(1));
        let read_rx = Arc::new(Mutex::new(read_rx));
        for (n, conn) in read_conns.into_iter().enumerate() {
            let jobs = read_rx.clone();
            threads.push(spawn(&format!("graft-sqlite-reader-{n}"), move || {
                reader::run(conn, jobs)
            })?);
        }

        Ok(Self {
            config,
            writer: Some(write_tx),
            readers: Some(read_tx),
            threads,
        })
    }

    /// The configuration in use.
    pub fn config(&self) -> &SqliteConfig {
        &self.config
    }

    fn stopped() -> StoreError {
        StoreError::Backend("sqlite store is stopped".to_owned())
    }

    /// Queues a write and waits for its commit.
    async fn write(&self, op: WriteOp) -> StoreResult<Written> {
        let (reply, answer) = oneshot::channel();
        let queue = self.writer.as_ref().ok_or_else(Self::stopped)?;
        match queue.try_send(WriteJob { op, reply }) {
            Ok(()) => {}
            Err(TrySendError::Full(_)) => return Err(StoreError::Busy(self.config.retry_after_ms)),
            Err(TrySendError::Disconnected(_)) => return Err(Self::stopped()),
        }
        answer.await.unwrap_or_else(|| Err(Self::stopped()))
    }

    /// Queues a read on the pool and waits for it.
    async fn read<T, F>(&self, query: F) -> StoreResult<T>
    where
        T: Send + 'static,
        F: FnOnce(&Connection) -> StoreResult<T> + Send + 'static,
    {
        let (reply, answer) = oneshot::channel();
        let job: ReadJob = Box::new(move |conn| reply.send(query(conn)));
        let queue = self.readers.as_ref().ok_or_else(Self::stopped)?;
        match queue.try_send(job) {
            Ok(()) => {}
            Err(TrySendError::Full(_)) => return Err(StoreError::Busy(self.config.retry_after_ms)),
            Err(TrySendError::Disconnected(_)) => return Err(Self::stopped()),
        }
        answer.await.unwrap_or_else(|| Err(Self::stopped()))
    }
}

fn spawn(name: &str, f: impl FnOnce() + Send + 'static) -> StoreResult<JoinHandle<()>> {
    thread::Builder::new()
        .name(name.to_owned())
        .spawn(f)
        .map_err(|e| StoreError::Backend(format!("spawn {name}: {e}")))
}

impl Drop for SqliteMetaStore {
    /// Closes the queues; the threads finish what is queued and exit.
    fn drop(&mut self) {
        self.writer.take();
        self.readers.take();
        for handle in self.threads.drain(..) {
            let _ = handle.join();
        }
    }
}

impl MetaStore for SqliteMetaStore {
    async fn create_repo(&self, name: &str) -> StoreResult<RepoId> {
        match self.write(WriteOp::CreateRepo(name.to_owned())).await? {
            Written::Repo(id) => Ok(id),
            Written::Txn(_) => Err(StoreError::Backend("unexpected writer reply".to_owned())),
        }
    }

    async fn repo(&self, name: &str) -> StoreResult<Option<RepoId>> {
        let name = name.to_owned();
        self.read(move |c| reader::repo(c, &name)).await
    }

    async fn commit(&self, repo: RepoId, txn: MetaTxn) -> StoreResult<TxnOutcome> {
        match self.write(WriteOp::Commit(repo, txn)).await? {
            Written::Txn(outcome) => Ok(outcome),
            Written::Repo(_) => Err(StoreError::Backend("unexpected writer reply".to_owned())),
        }
    }

    async fn changeset(
        &self,
        repo: RepoId,
        id: ChangesetId,
    ) -> StoreResult<Option<(Vec<u8>, ChangesetRecord)>> {
        self.read(move |c| reader::changeset(c, repo, id)).await
    }

    async fn get_ref(&self, repo: RepoId, name: &str) -> StoreResult<Option<RefRecord>> {
        let name = name.to_owned();
        self.read(move |c| reader::get_ref(c, repo, &name)).await
    }

    async fn list_refs(
        &self,
        repo: RepoId,
        prefix: &str,
        limit: u32,
    ) -> StoreResult<Vec<RefRecord>> {
        let prefix = prefix.to_owned();
        self.read(move |c| reader::list_refs(c, repo, &prefix, limit))
            .await
    }

    async fn workspace(&self, repo: RepoId, name: &str) -> StoreResult<Option<WorkspaceRecord>> {
        let name = name.to_owned();
        self.read(move |c| reader::workspace(c, repo, &name)).await
    }

    async fn expired_workspaces(
        &self,
        repo: RepoId,
        before_ms: i64,
        limit: u32,
    ) -> StoreResult<Vec<WorkspaceRecord>> {
        self.read(move |c| reader::expired_workspaces(c, repo, before_ms, limit))
            .await
    }

    async fn list_workspaces(
        &self,
        repo: RepoId,
        owner: &str,
        after: &str,
        limit: u32,
    ) -> StoreResult<Vec<WorkspaceRecord>> {
        let (owner, after) = (owner.to_owned(), after.to_owned());
        self.read(move |c| reader::list_workspaces(c, repo, &owner, &after, limit))
            .await
    }

    async fn count_records(&self, repo: RepoId, state: RecordState) -> StoreResult<u64> {
        self.read(move |c| reader::count_records(c, repo, state))
            .await
    }

    async fn count_workspaces(&self, repo: RepoId, before_ms: Option<i64>) -> StoreResult<u64> {
        self.read(move |c| reader::count_workspaces(c, repo, before_ms))
            .await
    }

    async fn derived(
        &self,
        repo: RepoId,
        kind: &str,
        version: u32,
        key: &[u8],
    ) -> StoreResult<Option<Vec<u8>>> {
        let (kind, key) = (kind.to_owned(), key.to_vec());
        self.read(move |c| reader::derived(c, repo, &kind, version, &key))
            .await
    }

    async fn line_pos(
        &self,
        repo: RepoId,
        line: &str,
        id: ChangesetId,
    ) -> StoreResult<Option<u64>> {
        let line = line.to_owned();
        self.read(move |c| reader::line_pos(c, repo, &line, id))
            .await
    }

    async fn scan_line_path(
        &self,
        repo: RepoId,
        line: &str,
        path: &RepoPath,
        before: u64,
        limit: u32,
    ) -> StoreResult<Vec<(u64, ChangesetId)>> {
        let (line, path) = (line.to_owned(), path.clone());
        self.read(move |c| reader::scan_line_path(c, repo, &line, &path, before, limit))
            .await
    }

    async fn holders(&self, repo: RepoId, object: ObjectId) -> StoreResult<u32> {
        self.read(move |c| reader::holders(c, repo, object)).await
    }

    async fn land(
        &self,
        repo: RepoId,
        draft: ChangesetId,
        line: &str,
    ) -> StoreResult<Option<ChangesetId>> {
        let line = line.to_owned();
        self.read(move |c| reader::land(c, repo, draft, &line))
            .await
    }

    async fn journal(
        &self,
        repo: RepoId,
        from: Lsn,
        limit: u32,
    ) -> StoreResult<Vec<(Lsn, String, Vec<u8>)>> {
        self.read(move |c| reader::journal(c, repo, from, limit))
            .await
    }
}
