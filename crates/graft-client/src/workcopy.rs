//! The working copy (`docs/design.md` §9): `.graft/` state, sparse prefixes,
//! stat-based status with no staging area, and update by `Diff`.
//!
//! ```text
//! <wc>/.graft/
//!   config   endpoint, repo, workspace          (key = value lines)
//!   state    line, base, head, last synced LSN  (key = value lines)
//!   sparse   prefixes, one per line             (see crate::sparse)
//!   index    path → kind, blob id, stat         (one entry per line)
//!   cache/   local object frames (graft-store-fs, LRU mode)
//! ```
//!
//! Every file is replaced by an atomic rename. The index records each file's
//! stat (size, mtime, ctime, inode, mode) as written; status re-hashes a file
//! only when its stat differs, or when it is *racily clean*: its mtime is not
//! strictly older than the index file's own mtime, so an edit in the same
//! timestamp tick as the index write could hide behind an equal stat. Files
//! outside the slice are never read or written.

use std::collections::{BTreeMap, BTreeSet, HashSet};
use std::fs::{self, File, Metadata};
use std::io::{self, ErrorKind, Write};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};

use graft_model::{BlobId, Change, ChangesetId, EntryKind, ObjectId, ObjectKind, RepoPath, TreeId};
use graft_proto::api::{ObjectApi, RepoApi};
use graft_proto::types::{Diff, Page, ResolveRef};
use graft_proto::{ApiError, CallContext};
use graft_store::ObjectStore;
use graft_store_fs::{FsMode, FsObjectStore};

use crate::fetch::{
    FetchOptions, FetchStats, fetch_objects, fetch_slice, load_blob, load_tree, resolve_root,
};
use crate::sparse::{escape, unescape};
use crate::{Error, Result, Sparse};

/// The working copy's state directory.
pub const GRAFT_DIR: &str = ".graft";

/// The default bound of the local object cache.
pub const DEFAULT_CACHE_BYTES: u64 = 4 * 1024 * 1024 * 1024;

const CONFIG_FILE: &str = "config";
const STATE_FILE: &str = "state";
const SPARSE_FILE: &str = "sparse";
const INDEX_FILE: &str = "index";
const CACHE_DIR: &str = "cache";
const INDEX_HEADER: &str = "graft-index 1";

/// Suffix of the temp file a checkout writes before renaming into place.
const TMP_SUFFIX: &str = ".graft-tmp";

/// Diff page size used by update.
const DIFF_PAGE: u32 = 1000;

/// The working copy's configuration (`.graft/config`).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct WcConfig {
    /// Server endpoint, when configured (see [`crate::discover`]).
    pub endpoint: Option<String>,
    /// Repository.
    pub repo: String,
    /// Workspace name, once one is open (phase 2).
    pub workspace: Option<String>,
}

/// The persisted working-copy state (`.graft/state`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WcState {
    /// Line tracked.
    pub line: String,
    /// Base changeset: the snapshot the files were checked out from.
    pub base: ChangesetId,
    /// Head draft (phase 2).
    pub head: Option<ChangesetId>,
    /// Last synced journal position.
    pub lsn: u64,
}

/// The stat fields status compares.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Stat {
    /// Size in bytes.
    pub size: u64,
    /// Modification time, ns since the epoch.
    pub mtime_ns: i64,
    /// Status-change time, ns since the epoch.
    pub ctime_ns: i64,
    /// Inode.
    pub ino: u64,
    /// Mode bits.
    pub mode: u32,
}

impl Stat {
    /// The fields of `meta` (from `symlink_metadata`).
    pub fn of(meta: &Metadata) -> Self {
        Self {
            size: meta.len(),
            mtime_ns: meta.mtime().saturating_mul(1_000_000_000) + meta.mtime_nsec(),
            ctime_ns: meta.ctime().saturating_mul(1_000_000_000) + meta.ctime_nsec(),
            ino: meta.ino(),
            mode: meta.mode(),
        }
    }
}

/// Whether status must re-hash a file whose index entry recorded `recorded`
/// and whose stat is now `now`, given the index was written at
/// `index_written_ns`: the stat changed, or the file is racily clean.
pub fn needs_rehash(recorded: &Stat, now: &Stat, index_written_ns: i64) -> bool {
    recorded != now || now.mtime_ns >= index_written_ns
}

/// One file in the index.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct IndexEntry {
    /// Kind (file, exec or symlink).
    pub kind: EntryKind,
    /// The base content's blob id.
    pub id: BlobId,
    /// The stat when the file was last known to hold that content.
    pub stat: Stat,
}

/// The index (`.graft/index`): every file of the slice in the base.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Index {
    /// Entries by path.
    pub entries: BTreeMap<RepoPath, IndexEntry>,
    /// The index file's mtime when it was loaded or last written, ns.
    pub written_ns: i64,
}

/// One file's status against the base.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum FileStatus {
    /// New file in the slice.
    Added(RepoPath),
    /// Content or mode changed.
    Modified(RepoPath),
    /// Removed from the working copy.
    Deleted(RepoPath),
}

impl FileStatus {
    /// The path.
    pub fn path(&self) -> &RepoPath {
        match self {
            FileStatus::Added(p) | FileStatus::Modified(p) | FileStatus::Deleted(p) => p,
        }
    }
}

/// What an update did.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct UpdateStats {
    /// The blob fetch.
    pub fetch: FetchStats,
    /// Files written or removed.
    pub changed: usize,
}

/// A working copy on disk.
#[derive(Debug)]
pub struct WorkingCopy {
    root: PathBuf,
    config: WcConfig,
    state: WcState,
    sparse: Sparse,
    index: Index,
}

/// Clones `line` (at `at`, or its tip) of `config.repo` into `dest`, which
/// must not already be a working copy: writes `.graft/`, fetches the slice
/// into the local cache, and checks it out.
#[allow(clippy::too_many_arguments)]
pub async fn clone<A: ObjectApi + RepoApi>(
    api: &A,
    cx: &CallContext,
    dest: &Path,
    config: WcConfig,
    line: &str,
    at: Option<ChangesetId>,
    sparse: Sparse,
    opts: &FetchOptions,
) -> Result<(WorkingCopy, FetchStats)> {
    let base = match at {
        Some(id) => id,
        None => {
            api.resolve_ref(
                cx,
                ResolveRef {
                    repo: config.repo.clone(),
                    name: line.to_owned(),
                },
            )
            .await?
            .target
        }
    };
    let state = WcState {
        line: line.to_owned(),
        base,
        head: None,
        lsn: 0,
    };
    let mut wc = WorkingCopy::init(dest, config, state, sparse)?;
    let cache = wc.open_cache(DEFAULT_CACHE_BYTES)?;
    let stats = wc.checkout(api, &cache, cx, opts).await?;
    Ok((wc, stats))
}

impl WorkingCopy {
    /// Creates `.graft/` under `root` with an empty index. Fails if `root`
    /// is already a working copy.
    pub fn init(root: &Path, config: WcConfig, state: WcState, sparse: Sparse) -> Result<Self> {
        let dir = root.join(GRAFT_DIR);
        fs::create_dir_all(root)?;
        fs::create_dir(&dir).map_err(|e| match e.kind() {
            ErrorKind::AlreadyExists => Error::State("already a working copy".to_owned()),
            _ => Error::Io(e),
        })?;
        let mut wc = Self {
            root: root.to_owned(),
            config,
            state,
            sparse,
            index: Index::default(),
        };
        write_atomic(&dir.join(CONFIG_FILE), wc.config_text().as_bytes())?;
        wc.save_state()?;
        write_atomic(&dir.join(SPARSE_FILE), wc.sparse.to_text().as_bytes())?;
        wc.save_index()?;
        Ok(wc)
    }

    /// Opens the working copy containing `path` (searching upwards).
    pub fn open(path: &Path) -> Result<Self> {
        let mut at = Some(path);
        while let Some(dir) = at {
            if dir.join(GRAFT_DIR).is_dir() {
                return Self::load(dir);
            }
            at = dir.parent();
        }
        Err(Error::State("not inside a working copy".to_owned()))
    }

    fn load(root: &Path) -> Result<Self> {
        let dir = root.join(GRAFT_DIR);
        let config = parse_config(&fs::read_to_string(dir.join(CONFIG_FILE))?)?;
        let state = parse_state(&fs::read_to_string(dir.join(STATE_FILE))?)?;
        let sparse = Sparse::parse(&fs::read_to_string(dir.join(SPARSE_FILE))?)
            .map_err(|e| Error::State(format!("sparse: {e}")))?;
        let index_path = dir.join(INDEX_FILE);
        let mut index = parse_index(&fs::read_to_string(&index_path)?)?;
        index.written_ns = Stat::of(&fs::metadata(&index_path)?).mtime_ns;
        Ok(Self {
            root: root.to_owned(),
            config,
            state,
            sparse,
            index,
        })
    }

    /// The working copy root.
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// `.graft/config`.
    pub fn config(&self) -> &WcConfig {
        &self.config
    }

    /// `.graft/state`.
    pub fn state(&self) -> &WcState {
        &self.state
    }

    /// `.graft/sparse`.
    pub fn sparse(&self) -> &Sparse {
        &self.sparse
    }

    /// `.graft/index`.
    pub fn index(&self) -> &Index {
        &self.index
    }

    /// Opens the local object cache (`.graft/cache`), bounded to
    /// `max_bytes`. Frames are verified by the fetch code on read.
    pub fn open_cache(&self, max_bytes: u64) -> Result<FsObjectStore> {
        let store = FsObjectStore::open(
            self.root.join(GRAFT_DIR).join(CACHE_DIR),
            FsMode::Lru { max_bytes },
        )?;
        Ok(store.verify_reads(false))
    }

    /// Fetches the base's slice into `cache` and writes it out, replacing
    /// the index.
    pub async fn checkout<A: ObjectApi + RepoApi, S: ObjectStore>(
        &mut self,
        api: &A,
        cache: &S,
        cx: &CallContext,
        opts: &FetchOptions,
    ) -> Result<FetchStats> {
        let repo = self.config.repo.clone();
        let root = resolve_root(api, cx, &repo, self.state.base).await?;
        let mut stats = fetch_slice(api, cache, cx, &repo, root, &self.sparse, opts).await?;
        let mut entries = BTreeMap::new();
        let mut dirs = vec![(root, None::<RepoPath>)];
        while let Some((id, dir)) = dirs.pop() {
            let tree = match load_tree(cache, id).await? {
                Some(t) => t,
                None => {
                    // Evicted since the fetch: ask again.
                    let s = fetch_objects(api, cache, cx, &repo, &[(id.0, ObjectKind::Tree)], opts)
                        .await?;
                    stats.add(&s);
                    load_tree(cache, id).await?.ok_or(missing(id.0))?
                }
            };
            let mut files = Vec::new();
            for e in &tree.entries {
                let path = match &dir {
                    None => RepoPath::new(&e.name),
                    Some(d) => d.join(&e.name),
                }
                .map_err(|_| ApiError::CorruptObject { id: id.0 })?;
                if e.kind.is_tree() {
                    if self.sparse.descends(&path) {
                        dirs.push((TreeId(e.id), Some(path)));
                    }
                } else if e.kind != EntryKind::Submodule && self.sparse.includes(&path) {
                    check_reserved(&path)?;
                    files.push((path, e.kind, BlobId(e.id)));
                }
            }
            let ids: Vec<(ObjectId, ObjectKind)> =
                files.iter().map(|f| (f.2.0, ObjectKind::Blob)).collect();
            let s = fetch_objects(api, cache, cx, &repo, &ids, opts).await?;
            stats.add(&s);
            for (path, kind, id) in files {
                let data = load_blob(cache, id.0).await?.ok_or(missing(id.0))?;
                let stat = write_entry(&self.root, &path, kind, &data)?;
                entries.insert(path, IndexEntry { kind, id, stat });
            }
        }
        self.index.entries = entries;
        self.save_index()?;
        Ok(stats)
    }

    /// Status against the base, re-hashing only on a stat mismatch or a
    /// racily clean entry. Sorted by path.
    pub fn status(&self) -> io::Result<Vec<FileStatus>> {
        let mut out = Vec::new();
        for (path, entry) in &self.index.entries {
            if let Some(change) = self.check(path, entry)? {
                out.push(change);
            }
        }
        for path in self.files_in_slice()? {
            if !self.index.entries.contains_key(&path) {
                out.push(FileStatus::Added(path));
            }
        }
        out.sort_by(|a, b| a.path().cmp(b.path()));
        Ok(out)
    }

    /// One indexed file's status, or `None` when it is clean.
    fn check(&self, path: &RepoPath, entry: &IndexEntry) -> io::Result<Option<FileStatus>> {
        let file = self.root.join(path.as_str());
        let meta = match fs::symlink_metadata(&file) {
            Ok(m) if !m.is_dir() => m,
            Ok(_) => return Ok(Some(FileStatus::Deleted(path.clone()))),
            Err(e) if e.kind() == ErrorKind::NotFound => {
                return Ok(Some(FileStatus::Deleted(path.clone())));
            }
            Err(e) => return Err(e),
        };
        if !needs_rehash(&entry.stat, &Stat::of(&meta), self.index.written_ns) {
            return Ok(None);
        }
        let (kind, id) = hash_file(&file, &meta)?;
        Ok((kind != entry.kind || id != entry.id).then(|| FileStatus::Modified(path.clone())))
    }

    /// Every file on disk in the slice (never descending elsewhere).
    fn files_in_slice(&self) -> io::Result<BTreeSet<RepoPath>> {
        let mut out = BTreeSet::new();
        if self.sparse.is_full() {
            walk(&self.root, None, &mut out)?;
            return Ok(out);
        }
        for prefix in &self.sparse.prefixes {
            match fs::symlink_metadata(self.root.join(prefix.as_str())) {
                Ok(m) if m.is_dir() => {
                    walk(&self.root.join(prefix.as_str()), Some(prefix), &mut out)?
                }
                Ok(_) => {
                    out.insert(prefix.clone());
                }
                Err(e) if e.kind() == ErrorKind::NotFound => {}
                Err(e) => return Err(e),
            }
        }
        if self.sparse.root_files {
            for entry in fs::read_dir(&self.root)? {
                let entry = entry?;
                if entry.file_type()?.is_dir() {
                    continue;
                }
                if let Some(path) = entry_path(None, &entry) {
                    out.insert(path);
                }
            }
        }
        Ok(out)
    }

    /// Moves the base to `to`: diffs the base against `to` within the slice,
    /// fetches only the blobs the diff names (and the cache lacks), and
    /// rewrites only the changed files. Refuses, changing nothing, when a
    /// changed path has local changes.
    pub async fn update<A: ObjectApi + RepoApi, S: ObjectStore>(
        &mut self,
        api: &A,
        cache: &S,
        cx: &CallContext,
        to: ChangesetId,
        opts: &FetchOptions,
    ) -> Result<UpdateStats> {
        let from = self.state.base;
        if from == to {
            return Ok(UpdateStats::default());
        }
        let repo = self.config.repo.clone();
        let mut changes = Vec::new();
        let mut token = None;
        loop {
            let page = api
                .diff(
                    cx,
                    Diff {
                        repo: repo.clone(),
                        from,
                        to,
                        paths: self.sparse.diff_paths().unwrap_or_default(),
                        patch: None,
                        page: Page {
                            limit: DIFF_PAGE,
                            token,
                        },
                    },
                )
                .await?;
            changes.extend(
                page.files
                    .into_iter()
                    .map(|f| f.change)
                    .filter(|c| self.sparse.includes(c.path())),
            );
            match page.next {
                Some(next) => token = Some(next),
                None => break,
            }
        }
        for c in &changes {
            check_reserved(c.path())?;
        }

        let dirty = self.dirty(&changes)?;
        if !dirty.is_empty() {
            return Err(Error::Dirty(dirty));
        }

        let mut seen = HashSet::new();
        let wanted: Vec<(ObjectId, ObjectKind)> = changes
            .iter()
            .filter_map(|c| match c {
                Change::Put { kind, id, .. } if *kind != EntryKind::Submodule => Some(*id),
                _ => None,
            })
            .filter(|id| seen.insert(*id))
            .map(|id| (id, ObjectKind::Blob))
            .collect();
        let fetch = fetch_objects(api, cache, cx, &repo, &wanted, opts).await?;

        // Deletes first, so a directory replaced by a file (or the reverse)
        // is out of the way before the puts.
        for c in &changes {
            if let Change::Delete { path } = c {
                remove_entry(&self.root, path)?;
                self.index.entries.remove(path);
            }
        }
        for c in &changes {
            if let Change::Put { path, kind, id } = c {
                if *kind == EntryKind::Submodule {
                    continue;
                }
                let data = load_blob(cache, *id).await?.ok_or(missing(*id))?;
                let stat = write_entry(&self.root, path, *kind, &data)?;
                self.index.entries.insert(
                    path.clone(),
                    IndexEntry {
                        kind: *kind,
                        id: BlobId(*id),
                        stat,
                    },
                );
            }
        }
        self.save_index()?;
        self.state.base = to;
        self.save_state()?;
        Ok(UpdateStats {
            fetch,
            changed: changes.len(),
        })
    }

    /// The changed paths whose working files differ from the base: modified
    /// or deleted indexed files, and unindexed files a put would overwrite.
    fn dirty(&self, changes: &[Change]) -> io::Result<Vec<RepoPath>> {
        let mut out = Vec::new();
        for c in changes {
            let path = c.path();
            match self.index.entries.get(path) {
                Some(entry) => match self.check(path, entry)? {
                    None => {}
                    Some(FileStatus::Deleted(_)) if matches!(c, Change::Delete { .. }) => {}
                    Some(_) => out.push(path.clone()),
                },
                None => {
                    if fs::symlink_metadata(self.root.join(path.as_str())).is_ok() {
                        out.push(path.clone());
                    }
                }
            }
        }
        Ok(out)
    }

    fn config_text(&self) -> String {
        let mut s = format!("repo = {}\n", self.config.repo);
        if let Some(e) = &self.config.endpoint {
            s.push_str(&format!("endpoint = {e}\n"));
        }
        if let Some(w) = &self.config.workspace {
            s.push_str(&format!("workspace = {w}\n"));
        }
        s
    }

    fn save_state(&self) -> io::Result<()> {
        let mut s = format!("line = {}\nbase = {}\n", self.state.line, self.state.base);
        if let Some(h) = self.state.head {
            s.push_str(&format!("head = {h}\n"));
        }
        s.push_str(&format!("lsn = {}\n", self.state.lsn));
        write_atomic(&self.root.join(GRAFT_DIR).join(STATE_FILE), s.as_bytes())
    }

    /// Writes the index and records its new mtime as the racy-clean bound.
    fn save_index(&mut self) -> io::Result<()> {
        let mut s = String::from(INDEX_HEADER);
        s.push('\n');
        for (path, e) in &self.index.entries {
            let st = &e.stat;
            s.push_str(&format!(
                "{} {} {} {} {} {} {} {}\n",
                kind_name(e.kind),
                e.id.0.to_hex(),
                st.size,
                st.mtime_ns,
                st.ctime_ns,
                st.ino,
                st.mode,
                escape(path.as_str())
            ));
        }
        let path = self.root.join(GRAFT_DIR).join(INDEX_FILE);
        write_atomic(&path, s.as_bytes())?;
        self.index.written_ns = Stat::of(&fs::metadata(&path)?).mtime_ns;
        Ok(())
    }
}

fn missing(id: ObjectId) -> Error {
    Error::Api(ApiError::MissingObjects { ids: vec![id] })
}

/// Refuses a path that would write into `.graft/`.
fn check_reserved(path: &RepoPath) -> Result<()> {
    if path.components().next() == Some(GRAFT_DIR) {
        return Err(Error::Api(ApiError::InvalidPath {
            path: path.as_str().to_owned(),
            reason: "reserved for the working copy state".to_owned(),
        }));
    }
    Ok(())
}

fn kind_name(k: EntryKind) -> &'static str {
    match k {
        EntryKind::File => "file",
        EntryKind::Exec => "exec",
        EntryKind::Symlink => "symlink",
        EntryKind::Tree => "tree",
        EntryKind::Submodule => "submodule",
    }
}

fn kind_from_name(s: &str) -> Option<EntryKind> {
    Some(match s {
        "file" => EntryKind::File,
        "exec" => EntryKind::Exec,
        "symlink" => EntryKind::Symlink,
        _ => return None,
    })
}

/// The kind and blob id of a working file.
fn hash_file(file: &Path, meta: &Metadata) -> io::Result<(EntryKind, BlobId)> {
    if meta.file_type().is_symlink() {
        let target = fs::read_link(file)?;
        return Ok((
            EntryKind::Symlink,
            BlobId::hash(target.as_os_str().as_bytes()),
        ));
    }
    let data = fs::read(file)?;
    let kind = if meta.mode() & 0o100 != 0 {
        EntryKind::Exec
    } else {
        EntryKind::File
    };
    Ok((kind, BlobId::hash(&data)))
}

/// Writes one file (or symlink) and returns its stat.
fn write_entry(root: &Path, path: &RepoPath, kind: EntryKind, data: &[u8]) -> io::Result<Stat> {
    let target = root.join(path.as_str());
    let parent = target.parent().expect("a repository path has a parent");
    fs::create_dir_all(parent)?;
    match kind {
        EntryKind::Symlink => {
            match fs::symlink_metadata(&target) {
                Ok(m) if !m.is_dir() => fs::remove_file(&target)?,
                _ => {}
            }
            std::os::unix::fs::symlink(std::ffi::OsStr::from_bytes(data), &target)?;
        }
        _ => {
            let tmp = parent.join(format!(".{}{TMP_SUFFIX}", path.name()));
            let mode = if kind == EntryKind::Exec {
                0o755
            } else {
                0o644
            };
            {
                let mut f = File::create(&tmp)?;
                f.write_all(data)?;
                f.set_permissions(fs::Permissions::from_mode(mode))?;
            }
            fs::rename(&tmp, &target)?;
        }
    }
    Ok(Stat::of(&fs::symlink_metadata(&target)?))
}

/// Removes one file and any directories it leaves empty.
fn remove_entry(root: &Path, path: &RepoPath) -> io::Result<()> {
    let target = root.join(path.as_str());
    match fs::remove_file(&target) {
        Ok(()) => {}
        Err(e) if e.kind() == ErrorKind::NotFound => {}
        Err(e) => return Err(e),
    }
    let mut dir = target.parent();
    while let Some(d) = dir {
        if d == root || fs::remove_dir(d).is_err() {
            break;
        }
        dir = d.parent();
    }
    Ok(())
}

/// Collects every file under `dir` (whose repository path is `prefix`),
/// skipping `.graft` at the root, checkout temp files and names that are
/// not valid repository paths.
fn walk(dir: &Path, prefix: Option<&RepoPath>, out: &mut BTreeSet<RepoPath>) -> io::Result<()> {
    for entry in fs::read_dir(dir)? {
        let entry = entry?;
        let Some(path) = entry_path(prefix, &entry) else {
            continue;
        };
        if entry.file_type()?.is_dir() {
            if prefix.is_none() && path.as_str() == GRAFT_DIR {
                continue;
            }
            walk(&entry.path(), Some(&path), out)?;
        } else {
            out.insert(path);
        }
    }
    Ok(())
}

fn entry_path(prefix: Option<&RepoPath>, entry: &fs::DirEntry) -> Option<RepoPath> {
    let name = entry.file_name();
    let name = name.to_str()?;
    if name.ends_with(TMP_SUFFIX) {
        return None;
    }
    match prefix {
        None => RepoPath::new(name).ok(),
        Some(p) => p.join(name).ok(),
    }
}

fn write_atomic(path: &Path, data: &[u8]) -> io::Result<()> {
    let tmp = path.with_extension("tmp");
    {
        let mut f = File::create(&tmp)?;
        f.write_all(data)?;
        f.sync_all()?;
    }
    fs::rename(&tmp, path)?;
    if let Some(dir) = path.parent() {
        File::open(dir)?.sync_all()?;
    }
    Ok(())
}

/// `key = value` lines; blank lines and `#` comments ignored.
fn pairs(text: &str) -> Result<BTreeMap<&str, &str>> {
    let mut out = BTreeMap::new();
    for line in text.lines() {
        if line.trim().is_empty() || line.starts_with('#') {
            continue;
        }
        let (k, v) = line
            .split_once('=')
            .ok_or_else(|| Error::State("expected key = value".to_owned()))?;
        out.insert(k.trim(), v.trim());
    }
    Ok(out)
}

fn parse_config(text: &str) -> Result<WcConfig> {
    let kv = pairs(text)?;
    Ok(WcConfig {
        repo: kv
            .get("repo")
            .ok_or_else(|| Error::State("config has no repo".to_owned()))?
            .to_string(),
        endpoint: kv.get("endpoint").map(|s| s.to_string()),
        workspace: kv.get("workspace").map(|s| s.to_string()),
    })
}

fn parse_state(text: &str) -> Result<WcState> {
    let kv = pairs(text)?;
    let id = |key: &str| -> Result<Option<ChangesetId>> {
        kv.get(key)
            .map(|s| ChangesetId::parse(s).map_err(|_| Error::State(format!("bad {key} id"))))
            .transpose()
    };
    Ok(WcState {
        line: kv
            .get("line")
            .ok_or_else(|| Error::State("state has no line".to_owned()))?
            .to_string(),
        base: id("base")?.ok_or_else(|| Error::State("state has no base".to_owned()))?,
        head: id("head")?,
        lsn: kv
            .get("lsn")
            .map(|s| s.parse().map_err(|_| Error::State("bad lsn".to_owned())))
            .transpose()?
            .unwrap_or(0),
    })
}

fn parse_index(text: &str) -> Result<Index> {
    let bad = || Error::State("malformed index".to_owned());
    let mut lines = text.lines();
    if lines.next() != Some(INDEX_HEADER) {
        return Err(Error::State("unknown index version".to_owned()));
    }
    let mut entries = BTreeMap::new();
    for line in lines {
        let f: Vec<&str> = line.splitn(8, ' ').collect();
        if f.len() != 8 {
            return Err(bad());
        }
        let num = |s: &str| s.parse::<i64>().map_err(|_| bad());
        let unum = |s: &str| s.parse::<u64>().map_err(|_| bad());
        let path = unescape(f[7])
            .and_then(|p| RepoPath::new(&p).ok())
            .ok_or_else(bad)?;
        entries.insert(
            path,
            IndexEntry {
                kind: kind_from_name(f[0]).ok_or_else(bad)?,
                id: BlobId(ObjectId::from_hex(f[1]).map_err(|_| bad())?),
                stat: Stat {
                    size: unum(f[2])?,
                    mtime_ns: num(f[3])?,
                    ctime_ns: num(f[4])?,
                    ino: unum(f[5])?,
                    mode: f[6].parse().map_err(|_| bad())?,
                },
            },
        );
    }
    Ok(Index {
        entries,
        written_ns: 0,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn stat(mtime_ns: i64) -> Stat {
        Stat {
            size: 10,
            mtime_ns,
            ctime_ns: mtime_ns,
            ino: 7,
            mode: 0o100644,
        }
    }

    #[test]
    fn rehash_on_stat_change_or_racy_clean() {
        let index_written = 1_000;
        // Equal stat, strictly older than the index: trusted.
        assert!(!needs_rehash(&stat(999), &stat(999), index_written));
        // Equal stat, same tick as the index write: racily clean.
        assert!(needs_rehash(&stat(1_000), &stat(1_000), index_written));
        assert!(needs_rehash(&stat(1_001), &stat(1_001), index_written));
        // Any field differing.
        let mut other = stat(999);
        other.ino = 8;
        assert!(needs_rehash(&stat(999), &other, index_written));
    }

    #[test]
    fn state_files_round_trip() {
        let dir = tempfile::tempdir().unwrap();
        let base = ChangesetId::hash(b"base");
        let config = WcConfig {
            endpoint: Some("http://x:1".into()),
            repo: "r".into(),
            workspace: None,
        };
        let state = WcState {
            line: "main".into(),
            base,
            head: Some(ChangesetId::hash(b"head")),
            lsn: 42,
        };
        let sparse = Sparse::new([RepoPath::new("a b/c").unwrap()], true);
        let mut wc =
            WorkingCopy::init(dir.path(), config.clone(), state.clone(), sparse.clone()).unwrap();
        wc.index.entries.insert(
            RepoPath::new("a b/c/odd\nname").unwrap(),
            IndexEntry {
                kind: EntryKind::Exec,
                id: BlobId::hash(b"x"),
                stat: stat(5),
            },
        );
        wc.save_index().unwrap();

        let opened = WorkingCopy::open(&dir.path().join("a b")).unwrap();
        assert_eq!(opened.config(), &config);
        assert_eq!(opened.state(), &state);
        assert_eq!(opened.sparse(), &sparse);
        assert_eq!(opened.index().entries, wc.index.entries);
        assert!(opened.index().written_ns > 0);
        assert!(matches!(
            WorkingCopy::init(dir.path(), config, state, sparse),
            Err(Error::State(_))
        ));
    }
}
