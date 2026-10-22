//! Filesystem `ObjectStore` (`docs/design.md` §6.4, work item G1.5).
//!
//! Layout `<root>/<repo>/objects/<2 hex>/<62 hex>`; each file holds one
//! object exactly as its gzip wire frame (§6.3, decision D5). Writes are
//! temp file in the target directory, fsync, no-replace rename, directory
//! fsync, so a crash leaves at most stray `*.tmp` files, which
//! [`FsObjectStore::open`] removes. Used by the server (authoritative mode)
//! and by the node daemon and client (LRU mode). A cache whose reader
//! verifies every frame may turn the syncs off
//! ([`FsObjectStore::sync_writes`]).
//!
//! One process owns a root at a time: `open` takes an exclusive lock on
//! `<root>/.lock` and holds it for the store's life.
//!
//! The calls do their file I/O on the calling thread. Every call touches
//! one object (or one file per id for `has`), so none of them waits long.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::fs::{self, File, OpenOptions};
use std::io::{self, ErrorKind, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Mutex, MutexGuard, PoisonError};
use std::time::SystemTime;

use graft_model::ObjectId;
use graft_proto::frame::{Frame, HEADER_LEN, MAX_FRAME_PAYLOAD};
use graft_store::{ObjectStore, RepoId, StoreError, StoreResult, StoreStats};

/// The per-repository directory that holds the shards.
const OBJECTS_DIR: &str = "objects";

/// Suffix of a temp file; anything with it is removed at open.
const TMP_SUFFIX: &str = ".tmp";

/// The lock file in the root.
const LOCK_FILE: &str = ".lock";

/// Hex characters in an object's file name (the first two name its shard).
const NAME_HEX: usize = 62;

/// The largest file a valid frame can be: a raw body is at most
/// [`MAX_FRAME_PAYLOAD`] bytes, and gzip is only kept when it is smaller.
const MAX_FRAME_FILE: u64 = HEADER_LEN as u64 + MAX_FRAME_PAYLOAD;

/// How the store treats capacity.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FsMode {
    /// Never evicts (server).
    Authoritative,
    /// Evicts least-recently-used frames above `max_bytes` (node, client).
    /// Stored bytes never exceed the bound by more than the newest object.
    Lru {
        /// Byte bound.
        max_bytes: u64,
    },
}

type Key = (RepoId, ObjectId);

/// A filesystem object store.
#[derive(Debug)]
pub struct FsObjectStore {
    root: PathBuf,
    mode: FsMode,
    verify_reads: bool,
    sync_writes: bool,
    objects: AtomicU64,
    bytes: AtomicU64,
    /// Recency and sizes in LRU mode. Its lock also orders every change to
    /// the counters in that mode, so eviction sees an exact total.
    lru: Option<Mutex<Lru>>,
    /// Directories whose entry in their parent is known to be durable.
    durable_dirs: Mutex<HashSet<PathBuf>>,
    next_tmp: AtomicU64,
    _lock: Option<File>,
}

impl FsObjectStore {
    /// Opens (creating if needed) a store at `root`, removing stray temp
    /// files. Opening walks every shard directory once, to clean up and to
    /// count what is stored. In LRU mode it also evicts down to the bound,
    /// oldest modification time first.
    ///
    /// Reads are verified (see [`FsObjectStore::verify_reads`]).
    pub fn open(root: impl Into<PathBuf>, mode: FsMode) -> StoreResult<Self> {
        let root = root.into();
        fs::create_dir_all(&root).map_err(|e| io_error("create", &root, e))?;
        let lock = lock_root(&root)?;

        let mut objects = 0u64;
        let mut bytes = 0u64;
        let mut found = Vec::new();
        let mut durable_dirs = HashSet::new();
        scan(&root, &mut durable_dirs, |key, size, modified| {
            objects += 1;
            bytes += size;
            if matches!(mode, FsMode::Lru { .. }) {
                found.push((modified, key, size));
            }
        })?;

        let lru = match mode {
            FsMode::Authoritative => None,
            FsMode::Lru { max_bytes } => {
                found.sort();
                let mut lru = Lru::new(max_bytes);
                for (_, key, size) in found {
                    lru.insert(key, size);
                }
                Some(Mutex::new(lru))
            }
        };
        let store = Self {
            root,
            mode,
            verify_reads: true,
            sync_writes: true,
            objects: AtomicU64::new(objects),
            bytes: AtomicU64::new(bytes),
            lru,
            durable_dirs: Mutex::new(durable_dirs),
            next_tmp: AtomicU64::new(0),
            _lock: lock,
        };
        if let Some(mut lru) = store.lock_lru() {
            store.evict(&mut lru, 0)?;
        }
        Ok(store)
    }

    /// Sets whether `get` verifies each frame against its id (the default).
    /// With verification on, a frame that does not decode to its id is
    /// [`StoreError::Corrupt`]. Turn it off only where the caller verifies,
    /// or where frames are opaque test bytes (the conformance suite).
    pub fn verify_reads(mut self, on: bool) -> Self {
        self.verify_reads = on;
        self
    }

    /// Sets whether a put syncs the file and its directory before it
    /// returns (the default). With syncing off, a crash may leave an object
    /// file empty or torn under its final name, so turn it off only for a
    /// cache whose reader verifies every frame and refetches a bad one (the
    /// client's). The temp-and-rename write stays, so a reader never sees a
    /// partial file while the process runs.
    pub fn sync_writes(mut self, on: bool) -> Self {
        self.sync_writes = on;
        self
    }

    /// The store root.
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// The capacity mode.
    pub fn mode(&self) -> FsMode {
        self.mode
    }

    /// The path of an object's file.
    pub fn object_path(&self, repo: RepoId, id: ObjectId) -> PathBuf {
        let (dir, name) = self.location(repo, id);
        dir.join(name)
    }

    fn location(&self, repo: RepoId, id: ObjectId) -> (PathBuf, String) {
        let mut hex = id.to_hex();
        let name = hex.split_off(2);
        let dir = self
            .root
            .join(repo.0.to_string())
            .join(OBJECTS_DIR)
            .join(hex);
        (dir, name)
    }

    fn lock_lru(&self) -> Option<MutexGuard<'_, Lru>> {
        self.lru
            .as_ref()
            .map(|l| l.lock().unwrap_or_else(PoisonError::into_inner))
    }

    fn put_sync(&self, repo: RepoId, id: ObjectId, frame: &[u8]) -> StoreResult<()> {
        let (dir, name) = self.location(repo, id);
        let target = dir.join(&name);
        match fs::symlink_metadata(&target) {
            Ok(_) => {
                if let Some(mut lru) = self.lock_lru() {
                    lru.touch((repo, id));
                }
                // A concurrent writer may have renamed it in but not yet
                // synced the directory; this put must not return before that.
                return self.sync_dir(&dir);
            }
            Err(e) if e.kind() == ErrorKind::NotFound => {}
            Err(e) => return Err(io_error("stat", &target, e)),
        }
        self.ensure_dir(&dir)?;

        let seq = self.next_tmp.fetch_add(1, Ordering::Relaxed);
        let tmp = dir.join(format!("{name}.{}.{seq}{TMP_SUFFIX}", std::process::id()));
        if let Err(e) = write_new(&tmp, frame, self.sync_writes) {
            let _ = fs::remove_file(&tmp);
            return Err(io_error("write", &tmp, e));
        }
        {
            let mut lru = self.lock_lru();
            match rename_noreplace(&tmp, &target) {
                Ok(true) => {
                    let size = frame.len() as u64;
                    self.objects.fetch_add(1, Ordering::Relaxed);
                    self.bytes.fetch_add(size, Ordering::Relaxed);
                    if let Some(lru) = lru.as_deref_mut() {
                        lru.insert((repo, id), size);
                        self.evict(lru, 1)?;
                    }
                }
                Ok(false) => {
                    // Put-if-absent: another writer got there first.
                    let _ = fs::remove_file(&tmp);
                    if let Some(lru) = lru.as_deref_mut() {
                        lru.touch((repo, id));
                    }
                }
                Err(e) => {
                    let _ = fs::remove_file(&tmp);
                    return Err(io_error("rename", &target, e));
                }
            }
        }
        self.sync_dir(&dir)
    }

    /// [`sync_dir`], unless syncing is off.
    fn sync_dir(&self, dir: &Path) -> StoreResult<()> {
        if self.sync_writes {
            sync_dir(dir)
        } else {
            Ok(())
        }
    }

    fn get_sync(&self, repo: RepoId, id: ObjectId) -> StoreResult<Option<Vec<u8>>> {
        let path = self.object_path(repo, id);
        let bytes = match self.read(&path) {
            Ok(bytes) => bytes,
            Err(ReadFail::Missing) => {
                if let Some(mut lru) = self.lock_lru() {
                    // Removed behind our back: stop counting it.
                    if let Some(size) = lru.remove((repo, id)) {
                        self.forget(size);
                    }
                }
                return Ok(None);
            }
            Err(ReadFail::TooLarge(len)) => {
                return Err(corrupt(repo, id, &format!("{len} byte file")));
            }
            Err(ReadFail::Io(e)) => return Err(io_error("read", &path, e)),
        };
        if self.verify_reads {
            Frame::from_bytes(&bytes)
                .and_then(|frame| frame.decode_verified(id))
                .map_err(|e| corrupt(repo, id, &format!("{e:?}")))?;
        }
        if let Some(mut lru) = self.lock_lru() {
            lru.touch((repo, id));
        }
        Ok(Some(bytes))
    }

    fn read(&self, path: &Path) -> Result<Vec<u8>, ReadFail> {
        let mut file = File::open(path).map_err(ReadFail::from)?;
        let len = file.metadata().map_err(ReadFail::from)?.len();
        if self.verify_reads && len > MAX_FRAME_FILE {
            return Err(ReadFail::TooLarge(len));
        }
        let mut bytes = Vec::with_capacity(len as usize);
        file.read_to_end(&mut bytes).map_err(ReadFail::from)?;
        Ok(bytes)
    }

    fn has_sync(&self, repo: RepoId, ids: &[ObjectId]) -> StoreResult<Vec<ObjectId>> {
        let mut present = Vec::new();
        for &id in ids {
            let path = self.object_path(repo, id);
            match fs::symlink_metadata(&path) {
                Ok(_) => present.push(id),
                Err(e) if e.kind() == ErrorKind::NotFound => {}
                Err(e) => return Err(io_error("stat", &path, e)),
            }
        }
        Ok(present)
    }

    fn delete_sync(&self, repo: RepoId, id: ObjectId) -> StoreResult<()> {
        let path = self.object_path(repo, id);
        let mut lru = self.lock_lru();
        let tracked = lru.as_deref_mut().and_then(|lru| lru.remove((repo, id)));
        let size = match fs::symlink_metadata(&path) {
            Ok(meta) => meta.len(),
            Err(e) if e.kind() == ErrorKind::NotFound => {
                // Removed behind our back: stop counting it.
                if let Some(size) = tracked {
                    self.forget(size);
                }
                return Ok(());
            }
            Err(e) => return Err(io_error("stat", &path, e)),
        };
        match fs::remove_file(&path) {
            Ok(()) => {
                self.forget(size);
                Ok(())
            }
            // A concurrent delete removed it and counted it.
            Err(e) if e.kind() == ErrorKind::NotFound => Ok(()),
            Err(e) => Err(io_error("remove", &path, e)),
        }
    }

    fn stats_sync(&self) -> StoreResult<StoreStats> {
        // Read the counters under the LRU lock so the pair is consistent.
        let _lru = self.lock_lru();
        Ok(StoreStats {
            objects: self.objects.load(Ordering::Relaxed),
            bytes: self.bytes.load(Ordering::Relaxed),
            free_bytes: free_bytes(&self.root)?,
        })
    }

    fn forget(&self, size: u64) {
        self.objects.fetch_sub(1, Ordering::Relaxed);
        self.bytes.fetch_sub(size, Ordering::Relaxed);
    }

    /// Removes least-recently-used objects until the bound holds or only
    /// `keep` (the newest) objects are left. Call with the LRU lock held.
    fn evict(&self, lru: &mut Lru, keep: usize) -> StoreResult<()> {
        while self.bytes.load(Ordering::Relaxed) > lru.max_bytes && lru.len() > keep {
            let Some((key, size)) = lru.pop_oldest() else {
                break;
            };
            let path = self.object_path(key.0, key.1);
            match fs::remove_file(&path) {
                Ok(()) => self.forget(size),
                Err(e) if e.kind() == ErrorKind::NotFound => self.forget(size),
                Err(e) => {
                    // Still on disk, so still counted and still evictable.
                    lru.insert(key, size);
                    return Err(io_error("evict", &path, e));
                }
            }
        }
        Ok(())
    }

    /// Creates `dir` and its missing parents, syncing each parent so the
    /// new entries are durable before an object is renamed into `dir`.
    fn ensure_dir(&self, dir: &Path) -> StoreResult<()> {
        let mut known = self
            .durable_dirs
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        if known.contains(dir) {
            return Ok(());
        }
        let mut missing = Vec::new();
        let mut at = dir;
        while at != self.root && !known.contains(at) {
            missing.push(at);
            match at.parent() {
                Some(parent) => at = parent,
                None => break,
            }
        }
        for dir in missing.into_iter().rev() {
            match fs::create_dir(dir) {
                Ok(()) => {}
                Err(e) if e.kind() == ErrorKind::AlreadyExists => {}
                Err(e) => return Err(io_error("create", dir, e)),
            }
            // Sync even when it already existed: it may be from before a
            // crash, created but never synced.
            if let Some(parent) = dir.parent() {
                self.sync_dir(parent)?;
            }
            known.insert(dir.to_path_buf());
        }
        Ok(())
    }
}

impl ObjectStore for FsObjectStore {
    async fn put(&self, repo: RepoId, id: ObjectId, frame: Vec<u8>) -> StoreResult<()> {
        self.put_sync(repo, id, &frame)
    }
    async fn get(&self, repo: RepoId, id: ObjectId) -> StoreResult<Option<Vec<u8>>> {
        self.get_sync(repo, id)
    }
    async fn has(&self, repo: RepoId, ids: &[ObjectId]) -> StoreResult<Vec<ObjectId>> {
        self.has_sync(repo, ids)
    }
    async fn delete(&self, repo: RepoId, id: ObjectId) -> StoreResult<()> {
        self.delete_sync(repo, id)
    }
    async fn stats(&self) -> StoreResult<StoreStats> {
        self.stats_sync()
    }
}

/// Why a read found no frame.
enum ReadFail {
    Missing,
    TooLarge(u64),
    Io(io::Error),
}

impl From<io::Error> for ReadFail {
    fn from(e: io::Error) -> Self {
        if e.kind() == ErrorKind::NotFound {
            ReadFail::Missing
        } else {
            ReadFail::Io(e)
        }
    }
}

/// Recency order for LRU mode.
#[derive(Debug)]
struct Lru {
    max_bytes: u64,
    clock: u64,
    /// Each object's size and last use.
    entries: HashMap<Key, (u64, u64)>,
    /// Last use → object, oldest first.
    order: BTreeMap<u64, Key>,
}

impl Lru {
    fn new(max_bytes: u64) -> Self {
        Self {
            max_bytes,
            clock: 0,
            entries: HashMap::new(),
            order: BTreeMap::new(),
        }
    }

    fn len(&self) -> usize {
        self.entries.len()
    }

    fn insert(&mut self, key: Key, size: u64) {
        self.clock += 1;
        if let Some((_, used)) = self.entries.insert(key, (size, self.clock)) {
            self.order.remove(&used);
        }
        self.order.insert(self.clock, key);
    }

    fn touch(&mut self, key: Key) {
        if let Some((size, _)) = self.entries.get(&key) {
            let size = *size;
            self.insert(key, size);
        }
    }

    fn remove(&mut self, key: Key) -> Option<u64> {
        let (size, used) = self.entries.remove(&key)?;
        self.order.remove(&used);
        Some(size)
    }

    fn pop_oldest(&mut self) -> Option<(Key, u64)> {
        let (_, key) = self.order.pop_first()?;
        let (size, _) = self.entries.remove(&key)?;
        Some((key, size))
    }
}

/// Walks every shard directory: removes stray temp files and reports each
/// object file with its size and modification time.
fn scan(
    root: &Path,
    durable_dirs: &mut HashSet<PathBuf>,
    mut each: impl FnMut(Key, u64, SystemTime),
) -> StoreResult<()> {
    for repo_entry in read_dir(root)? {
        let Some(repo) = repo_entry
            .file_name()
            .to_str()
            .and_then(|s| s.parse::<u32>().ok().filter(|n| n.to_string() == s))
        else {
            continue;
        };
        let objects = repo_entry.path().join(OBJECTS_DIR);
        if !objects.is_dir() {
            continue;
        }
        durable_dirs.insert(repo_entry.path());
        durable_dirs.insert(objects.clone());
        for shard_entry in read_dir(&objects)? {
            let shard_name = shard_entry.file_name();
            let Some(shard) = shard_name.to_str().filter(|s| is_hex(s, 2)) else {
                continue;
            };
            let shard_path = shard_entry.path();
            if !shard_path.is_dir() {
                continue;
            }
            durable_dirs.insert(shard_path.clone());
            for file_entry in read_dir(&shard_path)? {
                let file_name = file_entry.file_name();
                let Some(name) = file_name.to_str() else {
                    continue;
                };
                let path = file_entry.path();
                if name.ends_with(TMP_SUFFIX) {
                    match fs::remove_file(&path) {
                        Ok(()) => {}
                        Err(e) if e.kind() == ErrorKind::NotFound => {}
                        Err(e) => return Err(io_error("remove", &path, e)),
                    }
                    continue;
                }
                if !is_hex(name, NAME_HEX) {
                    continue;
                }
                let Ok(id) = ObjectId::from_hex(&format!("{shard}{name}")) else {
                    continue;
                };
                let meta = fs::symlink_metadata(&path).map_err(|e| io_error("stat", &path, e))?;
                if !meta.is_file() {
                    continue;
                }
                let modified = meta.modified().unwrap_or(SystemTime::UNIX_EPOCH);
                each((RepoId(repo), id), meta.len(), modified);
            }
        }
    }
    Ok(())
}

fn read_dir(dir: &Path) -> StoreResult<Vec<fs::DirEntry>> {
    fs::read_dir(dir)
        .and_then(Iterator::collect)
        .map_err(|e| io_error("list", dir, e))
}

fn is_hex(s: &str, len: usize) -> bool {
    s.len() == len && s.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
}

fn write_new(path: &Path, bytes: &[u8], sync: bool) -> io::Result<()> {
    let mut file = OpenOptions::new().write(true).create_new(true).open(path)?;
    file.write_all(bytes)?;
    if sync { file.sync_all() } else { Ok(()) }
}

/// Renames `from` to `to` unless `to` exists. `Ok(false)` means it existed
/// and `from` is left in place.
#[cfg(any(target_os = "linux", target_os = "android"))]
fn rename_noreplace(from: &Path, to: &Path) -> io::Result<bool> {
    use rustix::fs::{CWD, RenameFlags, renameat_with};
    use rustix::io::Errno;
    match renameat_with(CWD, from, CWD, to, RenameFlags::NOREPLACE) {
        Ok(()) => Ok(true),
        Err(Errno::EXIST) => Ok(false),
        // The filesystem does not support RENAME_NOREPLACE.
        Err(Errno::INVAL | Errno::NOSYS) => link_noreplace(from, to),
        Err(e) => Err(e.into()),
    }
}

#[cfg(not(any(target_os = "linux", target_os = "android")))]
fn rename_noreplace(from: &Path, to: &Path) -> io::Result<bool> {
    link_noreplace(from, to)
}

/// No-replace rename by hard link: `link` fails if the target exists.
fn link_noreplace(from: &Path, to: &Path) -> io::Result<bool> {
    match fs::hard_link(from, to) {
        Ok(()) => {
            fs::remove_file(from)?;
            Ok(true)
        }
        Err(e) if e.kind() == ErrorKind::AlreadyExists => Ok(false),
        Err(e) => Err(e),
    }
}

#[cfg(unix)]
fn sync_dir(dir: &Path) -> StoreResult<()> {
    File::open(dir)
        .and_then(|d| d.sync_all())
        .map_err(|e| io_error("sync", dir, e))
}

#[cfg(not(unix))]
fn sync_dir(_dir: &Path) -> StoreResult<()> {
    Ok(())
}

#[cfg(unix)]
fn lock_root(root: &Path) -> StoreResult<Option<File>> {
    use rustix::fs::{FlockOperation, flock};
    use rustix::io::Errno;
    let path = root.join(LOCK_FILE);
    let file = OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(&path)
        .map_err(|e| io_error("open", &path, e))?;
    match flock(&file, FlockOperation::NonBlockingLockExclusive) {
        Ok(()) => Ok(Some(file)),
        Err(Errno::WOULDBLOCK) => Err(StoreError::Backend(format!(
            "{} is in use by another process",
            root.display()
        ))),
        Err(e) => Err(io_error("lock", &path, e.into())),
    }
}

#[cfg(not(unix))]
fn lock_root(_root: &Path) -> StoreResult<Option<File>> {
    Ok(None)
}

#[cfg(unix)]
fn free_bytes(root: &Path) -> StoreResult<u64> {
    let vfs = rustix::fs::statvfs(root).map_err(|e| io_error("statvfs", root, e.into()))?;
    Ok(vfs.f_bavail.saturating_mul(vfs.f_frsize))
}

/// Unknown off Unix; reported as unlimited so admission does not refuse.
#[cfg(not(unix))]
fn free_bytes(_root: &Path) -> StoreResult<u64> {
    Ok(u64::MAX)
}

fn corrupt(repo: RepoId, id: ObjectId, why: &str) -> StoreError {
    StoreError::Corrupt(format!("repo {} object {}: {why}", repo.0, id.to_hex()))
}

fn io_error(op: &str, path: &Path, e: io::Error) -> StoreError {
    match e.kind() {
        ErrorKind::StorageFull | ErrorKind::QuotaExceeded => StoreError::Full,
        _ => StoreError::Backend(format!("{op} {}: {e}", path.display())),
    }
}
