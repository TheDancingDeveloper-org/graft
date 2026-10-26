//! SQLite `MetaStore` (`docs/design.md` §6.5, work item G1.6).
//!
//! WAL mode, `synchronous=FULL`, `foreign_keys=ON`. One writer connection on
//! a dedicated thread behind a bounded queue (see [`writer`] for how it
//! groups commits), plus a pool of reader threads, each with its own
//! connection. A full queue is backpressure: the call fails at once with
//! `StoreError::Busy(retry_after_ms)` rather than waiting. The schema is
//! versioned and migrated on open ([`migrations`]).

use std::fmt::Debug;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{self, SyncSender, TrySendError};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use graft_model::{ChangesetId, ObjectId, RepoPath};
use graft_store::{
    AttestationRow, ChangesetRecord, DoomMark, LandCheckRow, LandingRow, LineMergeRow, Lsn,
    MetaStore, MetaTxn, Protection, RecordState, RefRecord, RepoId, StoreError, StoreResult,
    TimedRow, TxnOutcome, WorkspaceRecord,
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

/// Receives the store's own measurements. The server installs one that
/// records them in its metric registry, so this crate needs no telemetry.
pub trait StoreObserver: Debug + Send + Sync + 'static {
    /// The writer queue now holds `depth` jobs. Calls are made one at a
    /// time, in the order the depth changed, so the last call always
    /// carries the current depth.
    fn writer_queue_depth(&self, depth: usize);
    /// Operation `op` answered after `elapsed`, measured from the call (so
    /// queueing time is included).
    fn operation(&self, op: &'static str, elapsed: Duration);
}

/// The writer queue's state, shared by the store and its writer thread.
#[derive(Debug)]
pub(crate) struct WriterQueue {
    /// Batches taken off the queue so far (see `writer_batches_taken`).
    sealed: AtomicU64,
    /// Jobs queued and not yet taken. The observer is told the new depth
    /// while this lock is held, so reports arrive in the order of the
    /// changes and the last one is always the current depth.
    pending: Mutex<usize>,
    observer: Option<Arc<dyn StoreObserver>>,
}

impl WriterQueue {
    fn new(observer: Option<Arc<dyn StoreObserver>>) -> Self {
        Self {
            sealed: AtomicU64::new(0),
            pending: Mutex::new(0),
            observer,
        }
    }

    /// Counts a job before it is sent, so the writer can never take it
    /// before it is counted.
    fn enqueued(&self) {
        self.change(|depth| *depth += 1);
    }

    /// Undoes [`WriterQueue::enqueued`] for a job that was not sent.
    fn unsent(&self) {
        self.change(|depth| *depth -= 1);
    }

    /// The writer took `n` jobs off the queue.
    fn taken(&self, n: usize) {
        self.sealed.fetch_add(1, Ordering::Release);
        self.change(|depth| *depth -= n);
    }

    /// Applies `f` to the depth and reports the result under the same lock.
    fn change(&self, f: impl FnOnce(&mut usize)) {
        let mut depth = self.pending.lock().unwrap_or_else(|e| e.into_inner());
        f(&mut depth);
        if let Some(observer) = &self.observer {
            observer.writer_queue_depth(*depth);
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
    queue: Arc<WriterQueue>,
    observer: Option<Arc<dyn StoreObserver>>,
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
        Self::open_observed(config, None)
    }

    /// As [`SqliteMetaStore::open`], reporting queue depth and operation
    /// timings to `observer`.
    pub fn open_observed(
        config: SqliteConfig,
        observer: Option<Arc<dyn StoreObserver>>,
    ) -> StoreResult<Self> {
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
        let queue = Arc::new(WriterQueue::new(observer.clone()));
        let writer_queue = queue.clone();
        threads.push(spawn("graft-sqlite-writer", move || {
            writer::run(write_conn, write_rx, writer_queue)
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
            queue,
            observer,
        })
    }

    /// The configuration in use.
    pub fn config(&self) -> &SqliteConfig {
        &self.config
    }

    /// How many write batches the writer has taken off its queue so far. A
    /// hook for tests: once the count moves, the first job is in the writer's
    /// hands and the queue behind it is empty.
    #[doc(hidden)]
    pub fn writer_batches_taken(&self) -> u64 {
        self.queue.sealed.load(Ordering::Acquire)
    }

    fn stopped() -> StoreError {
        StoreError::Backend("sqlite store is stopped".to_owned())
    }

    /// Reports one finished operation to the observer, if any.
    fn observe(&self, op: &'static str, started: Instant) {
        if let Some(observer) = &self.observer {
            observer.operation(op, started.elapsed());
        }
    }

    /// Queues a write and waits for its commit. `op` names it for the
    /// observer.
    async fn write(&self, op: &'static str, job: WriteOp) -> StoreResult<Written> {
        let started = Instant::now();
        let result = self.send_write(job).await;
        self.observe(op, started);
        result
    }

    async fn send_write(&self, op: WriteOp) -> StoreResult<Written> {
        let (reply, answer) = oneshot::channel();
        let queue = self.writer.as_ref().ok_or_else(Self::stopped)?;
        self.queue.enqueued();
        match queue.try_send(WriteJob { op, reply }) {
            Ok(()) => {}
            Err(TrySendError::Full(_)) => {
                self.queue.unsent();
                return Err(StoreError::Busy(self.config.retry_after_ms));
            }
            Err(TrySendError::Disconnected(_)) => {
                self.queue.unsent();
                return Err(Self::stopped());
            }
        }
        answer.await.unwrap_or_else(|| Err(Self::stopped()))
    }

    /// Queues a read on the pool and waits for it. `op` names it for the
    /// observer.
    async fn read<T, F>(&self, op: &'static str, query: F) -> StoreResult<T>
    where
        T: Send + 'static,
        F: FnOnce(&Connection) -> StoreResult<T> + Send + 'static,
    {
        let started = Instant::now();
        let result = self.send_read(query).await;
        self.observe(op, started);
        result
    }

    async fn send_read<T, F>(&self, query: F) -> StoreResult<T>
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
        match self
            .write("create_repo", WriteOp::CreateRepo(name.to_owned()))
            .await?
        {
            Written::Repo(id) => Ok(id),
            _ => Err(StoreError::Backend("unexpected writer reply".to_owned())),
        }
    }

    async fn repo(&self, name: &str) -> StoreResult<Option<RepoId>> {
        let name = name.to_owned();
        self.read("repo", move |c| reader::repo(c, &name)).await
    }

    async fn commit(&self, repo: RepoId, txn: MetaTxn) -> StoreResult<TxnOutcome> {
        match self.write("commit", WriteOp::Commit(repo, txn)).await? {
            Written::Txn(outcome) => Ok(outcome),
            _ => Err(StoreError::Backend("unexpected writer reply".to_owned())),
        }
    }

    /// One writer job: the run shares the batch's one `COMMIT` (one fsync
    /// under `synchronous=FULL`) instead of paying one per transaction.
    async fn commit_run(&self, repo: RepoId, txns: Vec<MetaTxn>) -> StoreResult<Vec<TxnOutcome>> {
        if txns.is_empty() {
            return Ok(Vec::new());
        }
        match self
            .write("commit_run", WriteOp::CommitRun(repo, txns))
            .await?
        {
            Written::Run(outcomes) => Ok(outcomes),
            Written::RunErr(e) => Err(e),
            _ => Err(StoreError::Backend("unexpected writer reply".to_owned())),
        }
    }

    async fn changeset(
        &self,
        repo: RepoId,
        id: ChangesetId,
    ) -> StoreResult<Option<(Vec<u8>, ChangesetRecord)>> {
        self.read("changeset", move |c| reader::changeset(c, repo, id))
            .await
    }

    async fn get_ref(&self, repo: RepoId, name: &str) -> StoreResult<Option<RefRecord>> {
        let name = name.to_owned();
        self.read("get_ref", move |c| reader::get_ref(c, repo, &name))
            .await
    }

    async fn list_refs(
        &self,
        repo: RepoId,
        prefix: &str,
        limit: u32,
    ) -> StoreResult<Vec<RefRecord>> {
        let prefix = prefix.to_owned();
        self.read("list_refs", move |c| {
            reader::list_refs(c, repo, &prefix, limit)
        })
        .await
    }

    async fn workspace(&self, repo: RepoId, name: &str) -> StoreResult<Option<WorkspaceRecord>> {
        let name = name.to_owned();
        self.read("workspace", move |c| reader::workspace(c, repo, &name))
            .await
    }

    async fn expired_workspaces(
        &self,
        repo: RepoId,
        before_ms: i64,
        limit: u32,
    ) -> StoreResult<Vec<WorkspaceRecord>> {
        self.read("expired_workspaces", move |c| {
            reader::expired_workspaces(c, repo, before_ms, limit)
        })
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
        self.read("list_workspaces", move |c| {
            reader::list_workspaces(c, repo, &owner, &after, limit)
        })
        .await
    }

    async fn count_records(&self, repo: RepoId, state: RecordState) -> StoreResult<u64> {
        self.read("count_records", move |c| {
            reader::count_records(c, repo, state)
        })
        .await
    }

    async fn count_workspaces(&self, repo: RepoId, before_ms: Option<i64>) -> StoreResult<u64> {
        self.read("count_workspaces", move |c| {
            reader::count_workspaces(c, repo, before_ms)
        })
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
        self.read("derived", move |c| {
            reader::derived(c, repo, &kind, version, &key)
        })
        .await
    }

    async fn line_pos(
        &self,
        repo: RepoId,
        line: &str,
        id: ChangesetId,
    ) -> StoreResult<Option<u64>> {
        let line = line.to_owned();
        self.read("line_pos", move |c| reader::line_pos(c, repo, &line, id))
            .await
    }

    async fn line_holders(
        &self,
        repo: RepoId,
        id: ChangesetId,
        limit: u32,
    ) -> StoreResult<Vec<(String, u64)>> {
        self.read("line_holders", move |c| {
            reader::line_holders(c, repo, id, limit)
        })
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
        self.read("scan_line_path", move |c| {
            reader::scan_line_path(c, repo, &line, &path, before, limit)
        })
        .await
    }

    async fn line_fork(&self, repo: RepoId, line: &str) -> StoreResult<Option<(String, u64)>> {
        let line = line.to_owned();
        self.read("line_fork", move |c| reader::line_fork(c, repo, &line))
            .await
    }

    async fn line_merges(
        &self,
        repo: RepoId,
        dst: &str,
        src: Option<&str>,
        at_or_below: u64,
        limit: u32,
    ) -> StoreResult<Vec<LineMergeRow>> {
        let (dst, src) = (dst.to_owned(), src.map(str::to_owned));
        self.read("line_merges", move |c| {
            reader::line_merges(c, repo, &dst, src.as_deref(), at_or_below, limit)
        })
        .await
    }

    async fn holders(&self, repo: RepoId, object: ObjectId) -> StoreResult<u32> {
        self.read("holders", move |c| reader::holders(c, repo, object))
            .await
    }

    async fn list_records(
        &self,
        repo: RepoId,
        state: RecordState,
        after: Option<ChangesetId>,
        limit: u32,
    ) -> StoreResult<Vec<(ChangesetId, ChangesetRecord)>> {
        self.read("list_records", move |c| {
            reader::list_records(c, repo, state, after, limit)
        })
        .await
    }

    async fn list_all_workspaces(
        &self,
        repo: RepoId,
        after: &str,
        limit: u32,
    ) -> StoreResult<Vec<WorkspaceRecord>> {
        let after = after.to_owned();
        self.read("list_all_workspaces", move |c| {
            reader::list_all_workspaces(c, repo, &after, limit)
        })
        .await
    }

    async fn holder_rows(
        &self,
        repo: RepoId,
        object: ObjectId,
        after: Option<String>,
        limit: u32,
    ) -> StoreResult<Vec<String>> {
        self.read("holder_rows", move |c| {
            reader::holder_rows(c, repo, object, after, limit)
        })
        .await
    }

    async fn protection(&self, repo: RepoId, id: ChangesetId) -> StoreResult<Option<Protection>> {
        self.read("protection", move |c| reader::protection(c, repo, id))
            .await
    }

    async fn expired_records(
        &self,
        repo: RepoId,
        now_ms: i64,
        after: Option<(i64, ChangesetId)>,
        limit: u32,
    ) -> StoreResult<Vec<(i64, ChangesetId)>> {
        self.read("expired_records", move |c| {
            reader::expired_records(c, repo, now_ms, after, limit)
        })
        .await
    }

    async fn held_objects(
        &self,
        repo: RepoId,
        holder: &str,
        after: Option<ObjectId>,
        limit: u32,
    ) -> StoreResult<Vec<ObjectId>> {
        let holder = holder.to_owned();
        self.read("held_objects", move |c| {
            reader::held_objects(c, repo, &holder, after, limit)
        })
        .await
    }

    async fn expired_rows(
        &self,
        repo: RepoId,
        now_ms: i64,
        after: Option<TimedRow>,
        limit: u32,
    ) -> StoreResult<Vec<TimedRow>> {
        self.read("expired_rows", move |c| {
            reader::expired_rows(c, repo, now_ms, after, limit)
        })
        .await
    }

    async fn doom_marks(
        &self,
        repo: RepoId,
        ids: &[ObjectId],
    ) -> StoreResult<Vec<(ObjectId, DoomMark)>> {
        let ids = ids.to_vec();
        self.read("doom_marks", move |c| reader::doom_marks(c, repo, &ids))
            .await
    }

    async fn old_doomed(
        &self,
        repo: RepoId,
        until_ms: i64,
        after: Option<(i64, ObjectId)>,
        limit: u32,
    ) -> StoreResult<Vec<(ObjectId, DoomMark)>> {
        self.read("old_doomed", move |c| {
            reader::old_doomed(c, repo, until_ms, after, limit)
        })
        .await
    }

    async fn lapsed_workspaces(
        &self,
        repo: RepoId,
        before_ms: i64,
        after: Option<(i64, String)>,
        limit: u32,
    ) -> StoreResult<Vec<WorkspaceRecord>> {
        self.read("lapsed_workspaces", move |c| {
            reader::lapsed_workspaces(c, repo, before_ms, after, limit)
        })
        .await
    }

    async fn scan_draft_paths(
        &self,
        repo: RepoId,
        line: &str,
        prefix: &RepoPath,
        after: Option<(RepoPath, String)>,
        limit: u32,
    ) -> StoreResult<Vec<(RepoPath, String)>> {
        let (line, prefix) = (line.to_owned(), prefix.clone());
        self.read("scan_draft_paths", move |c| {
            reader::scan_draft_paths(c, repo, &line, &prefix, after, limit)
        })
        .await
    }

    async fn workspace_draft_paths(
        &self,
        repo: RepoId,
        workspace: &str,
        after: Option<RepoPath>,
        limit: u32,
    ) -> StoreResult<Vec<RepoPath>> {
        let workspace = workspace.to_owned();
        self.read("workspace_draft_paths", move |c| {
            reader::workspace_draft_paths(c, repo, &workspace, after, limit)
        })
        .await
    }

    async fn gc_protection(&self, repo: RepoId) -> StoreResult<bool> {
        self.read("gc_protection", move |c| reader::gc_protection(c, repo))
            .await
    }

    async fn attestations_by_changeset(
        &self,
        repo: RepoId,
        changeset: ChangesetId,
        after: Option<(Lsn, u32)>,
        limit: u32,
    ) -> StoreResult<Vec<AttestationRow>> {
        self.read("attestations_by_changeset", move |c| {
            reader::attestations_by_changeset(c, repo, changeset, after, limit)
        })
        .await
    }

    async fn attestations_by_line(
        &self,
        repo: RepoId,
        line: &str,
        from_pos: u64,
        to_pos: u64,
        after: Option<(u64, Lsn, u32)>,
        limit: u32,
    ) -> StoreResult<Vec<AttestationRow>> {
        let line = line.to_owned();
        self.read("attestations_by_line", move |c| {
            reader::attestations_by_line(c, repo, &line, from_pos, to_pos, after, limit)
        })
        .await
    }

    async fn land_checks(
        &self,
        repo: RepoId,
        line: &str,
        from_pos: u64,
        limit: u32,
    ) -> StoreResult<Vec<LandCheckRow>> {
        let line = line.to_owned();
        self.read("land_checks", move |c| {
            reader::land_checks(c, repo, &line, from_pos, limit)
        })
        .await
    }

    async fn review_queue(
        &self,
        repo: RepoId,
        line: &str,
        since: Lsn,
        limit: u32,
    ) -> StoreResult<Vec<LandingRow>> {
        let line = line.to_owned();
        self.read("review_queue", move |c| {
            reader::review_queue(c, repo, &line, since, limit)
        })
        .await
    }

    async fn land(
        &self,
        repo: RepoId,
        draft: ChangesetId,
        line: &str,
    ) -> StoreResult<Option<ChangesetId>> {
        let line = line.to_owned();
        self.read("land", move |c| reader::land(c, repo, draft, &line))
            .await
    }

    async fn journal(
        &self,
        repo: RepoId,
        from: Lsn,
        limit: u32,
    ) -> StoreResult<Vec<(Lsn, String, Vec<u8>)>> {
        self.read("journal", move |c| reader::journal(c, repo, from, limit))
            .await
    }
}
