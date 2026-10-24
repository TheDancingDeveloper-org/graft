//! The working copy (`docs/design.md` §9): `.graft/` state, sparse prefixes,
//! stat-based status with no staging area, and update by `Diff`.
//!
//! ```text
//! <wc>/.graft/
//!   config   endpoint, repo, workspace          (key = value lines)
//!   state    line, base, head, last synced LSN, (key = value lines)
//!            and an unfinished move, if any
//!   sparse   prefixes, one per line             (see crate::sparse)
//!   index    path → kind, blob id, stat         (one entry per line)
//!   conflict the conflicts a sync wrote, until they are committed
//!   cache/   local object frames (graft-store-fs, LRU mode)
//!   .gitignore  `*`, so a surrounding git checkout ignores `.graft/`
//! ```
//!
//! Every file is replaced by an atomic rename. The index records each file's
//! stat (size, mtime, ctime, inode, mode) as written; status re-hashes a file
//! only when its stat differs, or when it is *racily clean*: its mtime is not
//! strictly older than the index file's own mtime, so an edit in the same
//! timestamp tick as the index write could hide behind an equal stat. Files
//! outside the slice are never read or written.

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::fs::{self, File, Metadata};
use std::io::{self, ErrorKind, Write};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use graft_model::{
    BlobId, Change, ChangesetId, EntryKind, ObjectId, ObjectKind, RepoPath, Tree, TreeId,
};
use graft_proto::api::{ObjectApi, RepoApi};
use graft_proto::types::{Diff, Page, ResolveRef};
use graft_proto::{ApiError, CallContext};
use graft_store::ObjectStore;
use graft_store_fs::{FsMode, FsObjectStore};

use crate::fetch::{
    FetchOptions, FetchStats, fetch_objects, fetch_slice, load_blob, load_tree, over_budget,
    resolve_root,
};
use crate::sparse::{escape, unescape};
use crate::{Error, Result, Sparse};

mod commit;
mod conflict;
mod rewrite;
mod session;
mod upstream;

pub use commit::{CommitLimits, CommitOptions, Committed, UploadStats};
pub use conflict::{ConflictFile, ConflictMode, Conflicts, sidecar_path};
pub use rewrite::{Reverted, Rewritten};
pub use session::{Begun, LandOptions, LandOutcome, Moved, Recovered, SyncOutcome};
pub use upstream::{MAX_OWN_PATHS, Outcome, Overlap, Upstream, UpstreamChange, predict};

/// The working copy's state directory.
pub const GRAFT_DIR: &str = ".graft";

/// The default bound of the local object cache.
pub const DEFAULT_CACHE_BYTES: u64 = 4 * 1024 * 1024 * 1024;

/// Other tools' version-control metadata at the root of a working copy: a
/// git checkout's `.git` (a directory, or the pointer file of a linked git
/// worktree), and the same for Mercurial, Jujutsu and Subversion. graft
/// never lists, reads or commits them, so a working copy can share its
/// directory with another tool's checkout.
pub const FOREIGN_METADATA: [&str; 4] = [".git", ".hg", ".jj", ".svn"];

/// Whether `name` is one of [`FOREIGN_METADATA`] on some filesystem: matched
/// case-insensitively, and for git also the NTFS and HFS+ variants
/// ([`graft_model::path::is_git_metadata_name`]).
pub fn is_foreign_metadata(name: &str) -> bool {
    FOREIGN_METADATA
        .iter()
        .any(|m| m.eq_ignore_ascii_case(name))
        || graft_model::path::is_git_metadata_name(name)
}

/// The ignore file written into `.graft/` and what it holds: a surrounding
/// git checkout then ignores the whole directory with no rule of its own.
const SELF_IGNORE: (&str, &str) = (".gitignore", "# graft working-copy state\n*\n");

const CONFIG_FILE: &str = "config";
const STATE_FILE: &str = "state";
const SPARSE_FILE: &str = "sparse";
const INDEX_FILE: &str = "index";
const CACHE_DIR: &str = "cache";
const INDEX_HEADER: &str = "graft-index 1";

/// Suffix of the temp file a checkout writes before renaming into place.
const TMP_SUFFIX: &str = ".graft-tmp";

/// Suffix of the sidecar a sync writes beside a conflicting file it cannot
/// mark (a binary, a symlink, or a file one side deleted). Like the temp
/// suffix, it is the working copy's own: status and commit never see it.
pub const CONFLICT_SUFFIX: &str = ".graft-conflict";

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
    /// A move of the working copy and its workspace that started and has
    /// not finished: where both are going. Until it finishes the server's
    /// workspace may be on either side of it.
    pub moving: Option<Moving>,
}

/// Where an unfinished move takes the working copy and its workspace.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Moving {
    /// The new base.
    pub base: ChangesetId,
    /// The new head.
    pub head: Option<ChangesetId>,
    /// The server is rebasing or rewriting the stack and picks the head
    /// (`head` is unused): a workspace on `base` with any head is where the
    /// move goes, and one still on the old base and head never moved.
    pub by_server: bool,
}

impl Moving {
    /// A move to `base` and `head`.
    pub fn to(base: ChangesetId, head: Option<ChangesetId>) -> Self {
        Self {
            base,
            head,
            by_server: false,
        }
    }
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

/// What a read that only names the repository and the base needs of a
/// working copy: its configuration and state, without the index (whose
/// size follows the slice, not the request). See [`WorkingCopy::open_info`].
#[derive(Clone, Debug)]
pub struct WcInfo {
    root: PathBuf,
    config: WcConfig,
    state: WcState,
}

impl WcInfo {
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
        moving: None,
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
        write_atomic(&dir.join(SELF_IGNORE.0), SELF_IGNORE.1.as_bytes())?;
        write_atomic(&dir.join(CONFIG_FILE), wc.config_text().as_bytes())?;
        wc.save_state()?;
        write_atomic(&dir.join(SPARSE_FILE), wc.sparse.to_text().as_bytes())?;
        wc.save_index()?;
        Ok(wc)
    }

    /// Opens the working copy containing `path` (searching upwards).
    pub fn open(path: &Path) -> Result<Self> {
        Self::load(&find_root(path)?)
    }

    /// The configuration and state of the working copy containing `path`
    /// (searching upwards), without reading its index or slice: for reads
    /// that only need the repository, the endpoint and the base.
    pub fn open_info(path: &Path) -> Result<WcInfo> {
        let root = find_root(path)?;
        let dir = root.join(GRAFT_DIR);
        let config = parse_config(&fs::read_to_string(dir.join(CONFIG_FILE))?)?;
        let state = parse_state(&fs::read_to_string(dir.join(STATE_FILE))?)?;
        Ok(WcInfo {
            root,
            config,
            state,
        })
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
    /// `max_bytes`. Frames are verified by the fetch code on read, and one
    /// that fails is deleted and fetched again, so puts are not synced: a
    /// frame torn by a crash costs a refetch, where two fsyncs per object
    /// made a clone about 20 times slower.
    pub fn open_cache(&self, max_bytes: u64) -> Result<FsObjectStore> {
        let store = FsObjectStore::open(
            self.root.join(GRAFT_DIR).join(CACHE_DIR),
            FsMode::Lru { max_bytes },
        )?;
        Ok(store.verify_reads(false).sync_writes(false))
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
        let budget = opts.budget;
        let mut entries = BTreeMap::new();
        // Each distinct tree is loaded and verified once, however many paths
        // name it; the budget bounds the paths.
        let mut trees: HashMap<TreeId, Arc<Tree>> = HashMap::new();
        let (mut examined, mut written) = (0u64, 0u64);
        let mut dirs = vec![(root, None::<RepoPath>)];
        while let Some((id, dir)) = dirs.pop() {
            let tree = match trees.get(&id) {
                Some(t) => t.clone(),
                None => {
                    let t = match load_tree(cache, id).await? {
                        Some(t) => t,
                        None => {
                            // Evicted since the fetch: ask again.
                            let s = fetch_objects(
                                api,
                                cache,
                                cx,
                                &repo,
                                &[(id.0, ObjectKind::Tree)],
                                opts,
                            )
                            .await?;
                            stats.add(&s);
                            load_tree(cache, id).await?.ok_or(missing(id.0))?
                        }
                    };
                    let t = Arc::new(t);
                    trees.insert(id, t.clone());
                    t
                }
            };
            examined += tree.entries.len() as u64;
            if examined > budget.max_entries {
                return Err(over_budget("max_entries", budget.max_entries));
            }
            let depth = dir.as_ref().map_or(0, |d| d.components().count());
            let mut files = Vec::new();
            for e in &tree.entries {
                let path = match &dir {
                    None => RepoPath::new(&e.name),
                    Some(d) => d.join(&e.name),
                }
                .map_err(|_| ApiError::CorruptObject { id: id.0 })?;
                if e.kind.is_tree() {
                    if self.sparse.descends(&path) {
                        if depth >= budget.max_depth {
                            return Err(over_budget("max_depth", budget.max_depth as u64));
                        }
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
            let mut lost = Vec::new();
            for (path, kind, id) in files {
                let Some(data) = load_blob(cache, id.0).await? else {
                    lost.push((path, kind, id));
                    continue;
                };
                written += data.len() as u64;
                if written > budget.max_bytes {
                    return Err(over_budget("max_bytes", budget.max_bytes));
                }
                let stat = write_entry(&self.root, &path, kind, &data)?;
                entries.insert(path, IndexEntry { kind, id, stat });
            }
            if !lost.is_empty() {
                let ids: Vec<ObjectId> = lost.iter().map(|(_, _, id)| id.0).collect();
                stats.add(&refetch_blobs(api, cache, cx, &repo, &ids, opts).await?);
                for (path, kind, id) in lost {
                    let data = load_blob(cache, id.0).await?.ok_or(missing(id.0))?;
                    written += data.len() as u64;
                    if written > budget.max_bytes {
                        return Err(over_budget("max_bytes", budget.max_bytes));
                    }
                    let stat = write_entry(&self.root, &path, kind, &data)?;
                    entries.insert(path, IndexEntry { kind, id, stat });
                }
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
        let mut dirs = RealDirs::default();
        for (path, entry) in &self.index.entries {
            if let Some(change) = self.check(path, entry, &mut dirs)? {
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

    /// One indexed file's status, or `None` when it is clean. A file whose
    /// parent is no longer a real directory is deleted: nothing is read
    /// through a symlink.
    fn check(
        &self,
        path: &RepoPath,
        entry: &IndexEntry,
        dirs: &mut RealDirs,
    ) -> io::Result<Option<FileStatus>> {
        if !dirs.parent_is_real(&self.root, path)? {
            return Ok(Some(FileStatus::Deleted(path.clone())));
        }
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
        let mut dirs = RealDirs::default();
        for prefix in &self.sparse.prefixes {
            // Never list through a symlinked parent.
            if !dirs.parent_is_real(&self.root, prefix)? {
                continue;
            }
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

    /// The snapshot the files and the index hold: the head draft, else the
    /// base.
    pub fn snapshot(&self) -> ChangesetId {
        self.state.head.unwrap_or(self.state.base)
    }

    /// Moves the base to `to` with no stack (see [`WorkingCopy::move_to`]).
    pub async fn update<A: ObjectApi + RepoApi, S: ObjectStore>(
        &mut self,
        api: &A,
        cache: &S,
        cx: &CallContext,
        to: ChangesetId,
        opts: &FetchOptions,
    ) -> Result<UpdateStats> {
        self.move_to(api, cache, cx, to, None, opts).await
    }

    /// Moves the working copy to `base` and `head`: diffs the current
    /// snapshot against the new one within the slice, fetches only the blobs
    /// the diff names (and the cache lacks), and rewrites only the changed
    /// files. Refuses, changing nothing, when a changed path has local
    /// changes.
    pub async fn move_to<A: ObjectApi + RepoApi, S: ObjectStore>(
        &mut self,
        api: &A,
        cache: &S,
        cx: &CallContext,
        base: ChangesetId,
        head: Option<ChangesetId>,
        opts: &FetchOptions,
    ) -> Result<UpdateStats> {
        let plan = self.plan_move(api, cx, head.unwrap_or(base), opts).await?;
        self.apply_move(api, cache, cx, plan, base, head, opts)
            .await
    }

    /// The first half of a move: the diff from the current snapshot to `to`
    /// within the slice, checked against the working files. Nothing is
    /// written, so a caller can make a server change between planning and
    /// applying.
    pub(crate) async fn plan_move<A: RepoApi>(
        &self,
        api: &A,
        cx: &CallContext,
        to: ChangesetId,
        opts: &FetchOptions,
    ) -> Result<Vec<Change>> {
        let from = self.snapshot();
        if from == to {
            return Ok(Vec::new());
        }
        let repo = self.config.repo.clone();
        let mut changes = Vec::new();
        let mut token = None;
        let mut pages = 0u64;
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
            pages += 1;
            if changes.len() as u64 > opts.budget.max_entries || pages > opts.budget.max_entries {
                return Err(over_budget("max_entries", opts.budget.max_entries));
            }
            match page.next {
                Some(next) => token = Some(next),
                None => break,
            }
        }
        for c in &changes {
            check_reserved(c.path())?;
        }
        self.check_routes(&changes)?;

        let dirty = self.dirty(&changes)?;
        if !dirty.is_empty() {
            return Err(Error::Dirty(dirty));
        }
        Ok(changes)
    }

    /// The second half of a move: fetches and writes `changes` (from
    /// [`WorkingCopy::plan_move`]) and records `base` and `head`.
    #[allow(clippy::too_many_arguments)]
    pub(crate) async fn apply_move<A: ObjectApi, S: ObjectStore>(
        &mut self,
        api: &A,
        cache: &S,
        cx: &CallContext,
        changes: Vec<Change>,
        base: ChangesetId,
        head: Option<ChangesetId>,
        opts: &FetchOptions,
    ) -> Result<UpdateStats> {
        let repo = self.config.repo.clone();
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
        let mut fetch = fetch_objects(api, cache, cx, &repo, &wanted, opts).await?;

        let mut written = 0u64;
        // Deletes first, so a directory replaced by a file (or the reverse)
        // is out of the way before the puts.
        for c in &changes {
            if let Change::Delete { path } = c {
                remove_entry(&self.root, path)?;
                self.index.entries.remove(path);
            }
        }
        let mut lost = Vec::new();
        for c in &changes {
            if let Change::Put { path, kind, id } = c {
                if *kind == EntryKind::Submodule {
                    continue;
                }
                let Some(data) = load_blob(cache, *id).await? else {
                    lost.push((path, kind, *id));
                    continue;
                };
                written += data.len() as u64;
                if written > opts.budget.max_bytes {
                    return Err(over_budget("max_bytes", opts.budget.max_bytes));
                }
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
        if !lost.is_empty() {
            let ids: Vec<ObjectId> = lost.iter().map(|(_, _, id)| *id).collect();
            fetch.add(&refetch_blobs(api, cache, cx, &repo, &ids, opts).await?);
            for (path, kind, id) in lost {
                let data = load_blob(cache, id).await?.ok_or(missing(id))?;
                written += data.len() as u64;
                if written > opts.budget.max_bytes {
                    return Err(over_budget("max_bytes", opts.budget.max_bytes));
                }
                let stat = write_entry(&self.root, path, *kind, &data)?;
                self.index.entries.insert(
                    path.clone(),
                    IndexEntry {
                        kind: *kind,
                        id: BlobId(id),
                        stat,
                    },
                );
            }
        }
        if !changes.is_empty() {
            self.save_index()?;
        }
        self.state.base = base;
        self.state.head = head;
        self.save_state()?;
        Ok(UpdateStats {
            fetch,
            changed: changes.len(),
        })
    }

    /// Refuses, before anything is written, a change whose path passes
    /// through something that is not a directory: an indexed file or symlink
    /// of the base, or a symlink or file on disk. The one exception is a
    /// parent this same diff deletes (a file replaced by a directory), which
    /// the deletes remove first. `write_entry` and `remove_entry` check each
    /// component again as they go.
    fn check_routes(&self, changes: &[Change]) -> Result<()> {
        // A diff that puts a symlink at P and also touches a path under P
        // is not a tree; refusing it here keeps an update from stopping
        // half applied.
        let symlinks: HashSet<&RepoPath> = changes
            .iter()
            .filter_map(|c| match c {
                Change::Put {
                    path,
                    kind: EntryKind::Symlink,
                    ..
                } => Some(path),
                _ => None,
            })
            .collect();
        for c in changes {
            let mut parent = c.path().parent();
            while let Some(p) = parent {
                if symlinks.contains(&p) {
                    return Err(Error::UnsafePath(c.path().clone()));
                }
                parent = p.parent();
            }
        }
        let deleted: HashSet<&RepoPath> = changes
            .iter()
            .filter_map(|c| match c {
                Change::Delete { path } => Some(path),
                _ => None,
            })
            .collect();
        for c in changes {
            let path = c.path();
            let mut parent = path.parent();
            while let Some(p) = parent {
                let replaced = deleted.contains(&p) && self.index.entries.contains_key(&p);
                if !replaced {
                    if self.index.entries.contains_key(&p) {
                        return Err(Error::UnsafePath(path.clone()));
                    }
                    match fs::symlink_metadata(self.root.join(p.as_str())) {
                        Ok(m) if !m.file_type().is_dir() => {
                            return Err(Error::UnsafePath(path.clone()));
                        }
                        Ok(_) => {}
                        Err(e) if e.kind() == ErrorKind::NotFound => {}
                        Err(e) => return Err(e.into()),
                    }
                }
                parent = p.parent();
            }
        }
        Ok(())
    }

    /// The changed paths whose working files differ from the base: modified
    /// or deleted indexed files, and unindexed files a put would overwrite.
    fn dirty(&self, changes: &[Change]) -> io::Result<Vec<RepoPath>> {
        let mut out = Vec::new();
        let mut dirs = RealDirs::default();
        for c in changes {
            let path = c.path();
            match self.index.entries.get(path) {
                Some(entry) => match self.check(path, entry, &mut dirs)? {
                    None => {}
                    Some(FileStatus::Deleted(_)) if matches!(c, Change::Delete { .. }) => {}
                    Some(_) => out.push(path.clone()),
                },
                None => {
                    // A parent that is not a real directory is one this
                    // diff replaces (`check_routes` refused the rest), so
                    // nothing of ours is in the way.
                    if dirs.parent_is_real(&self.root, path)?
                        && fs::symlink_metadata(self.root.join(path.as_str())).is_ok()
                    {
                        out.push(path.clone());
                    }
                }
            }
        }
        Ok(out)
    }

    /// Records the workspace this working copy writes through.
    pub fn set_workspace(&mut self, name: Option<String>) -> Result<()> {
        if let Some(n) = &name {
            check_workspace_name(n)?;
        }
        self.config.workspace = name;
        write_atomic(
            &self.root.join(GRAFT_DIR).join(CONFIG_FILE),
            self.config_text().as_bytes(),
        )?;
        Ok(())
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

    pub(crate) fn save_state(&self) -> io::Result<()> {
        let mut s = format!("line = {}\nbase = {}\n", self.state.line, self.state.base);
        if let Some(h) = self.state.head {
            s.push_str(&format!("head = {h}\n"));
        }
        s.push_str(&format!("lsn = {}\n", self.state.lsn));
        if let Some(m) = self.state.moving {
            s.push_str(&format!("moving.base = {}\n", m.base));
            match (m.by_server, m.head) {
                (true, _) => s.push_str("moving.head = server\n"),
                (false, Some(h)) => s.push_str(&format!("moving.head = {h}\n")),
                (false, None) => {}
            }
        }
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

/// Refuses a workspace name that could not have come from a server: the
/// engine's names are lowercase ASCII segments of at most 128 bytes, so
/// anything else (a newline that would forge a config line, for one) is a
/// broken or hostile reply.
pub(crate) fn check_workspace_name(name: &str) -> Result<()> {
    let ok = !name.is_empty()
        && name.len() <= 128
        && name.bytes().all(|b| {
            b.is_ascii_lowercase() || b.is_ascii_digit() || matches!(b, b'.' | b'_' | b'-' | b'/')
        });
    if ok {
        Ok(())
    } else {
        Err(Error::Api(ApiError::InvalidPath {
            path: name.chars().take(128).collect(),
            reason: "workspace-name".to_owned(),
        }))
    }
}

fn missing(id: ObjectId) -> Error {
    Error::Api(ApiError::MissingObjects { ids: vec![id] })
}

/// Asks the server again for blobs the cache lost or holds torn (a crash
/// mid-write). [`load_blob`] has already deleted their frames, so the fetch
/// requests them, in batches of `opts.batch`, once. The caller loads each
/// again and fails if it is still bad; nothing here loops.
async fn refetch_blobs<A: ObjectApi, S: ObjectStore>(
    api: &A,
    cache: &S,
    cx: &CallContext,
    repo: &str,
    lost: &[ObjectId],
    opts: &FetchOptions,
) -> Result<FetchStats> {
    let ids: Vec<(ObjectId, ObjectKind)> = lost.iter().map(|&id| (id, ObjectKind::Blob)).collect();
    fetch_objects(api, cache, cx, repo, &ids, opts).await
}

/// Whether a top-level name could reach `.graft` on some filesystem: equal
/// to it after Unicode compatibility normalization (NFKC, which covers the
/// NFC and NFD forms), full case folding, and dropping the default-ignorable
/// code points that HFS+ and APFS ignore in names. Case-insensitive (NTFS,
/// default APFS and HFS+) and normalizing filesystems would otherwise let
/// `.GRAFT/config` or a look-alike overwrite the working copy's own state.
pub fn is_reserved_name(name: &str) -> bool {
    use unicode_normalization::UnicodeNormalization;
    let ignorable = |c: &char| {
        matches!(*c as u32,
            0x00AD | 0x034F | 0x061C | 0x115F | 0x1160 | 0x17B4 | 0x17B5 | 0x180B..=0x180F
            | 0x200B..=0x200F | 0x202A..=0x202E | 0x2060..=0x206F | 0x3164 | 0xFE00..=0xFE0F
            | 0xFEFF | 0xFFA0 | 0xFFF0..=0xFFF8 | 0x1BCA0..=0x1BCA3 | 0x1D173..=0x1D17A
            | 0xE0000..=0xE0FFF)
    };
    if name.eq_ignore_ascii_case(GRAFT_DIR) {
        return true;
    }
    if name.is_ascii() {
        return false;
    }
    let folded: String = name
        .nfkc()
        .filter(|c| !ignorable(c))
        .collect::<String>()
        .to_lowercase();
    let folded: String = folded.nfkc().collect();
    folded == GRAFT_DIR
}

/// Refuses a path that would write into `.graft/`, in any spelling a
/// filesystem might treat as the same name ([`is_reserved_name`]).
fn check_reserved(path: &RepoPath) -> Result<()> {
    if path.components().next().is_some_and(is_reserved_name) {
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

/// Directories under the root already checked to be real (not symlinks),
/// so a status pass stats each directory once rather than once per file.
/// The last one found real is kept apart too: paths come in sorted order,
/// so most share the previous path's parent and need no lookup at all.
#[derive(Default)]
struct RealDirs {
    real: HashSet<RepoPath>,
    last: Option<RepoPath>,
}

impl RealDirs {
    /// Whether every parent of `path` under `root` exists and is a real
    /// directory.
    fn parent_is_real(&mut self, root: &Path, path: &RepoPath) -> io::Result<bool> {
        let Some((parent_str, _)) = path.as_str().rsplit_once('/') else {
            return Ok(true);
        };
        if self.last.as_ref().is_some_and(|l| l.as_str() == parent_str) {
            return Ok(true);
        }
        let Some(parent) = path.parent() else {
            return Ok(true);
        };
        if self.real.contains(&parent) {
            self.last = Some(parent);
            return Ok(true);
        }
        if !self.parent_is_real(root, &parent)? {
            return Ok(false);
        }
        match fs::symlink_metadata(root.join(parent.as_str())) {
            Ok(m) if m.file_type().is_dir() => {
                self.real.insert(parent.clone());
                self.last = Some(parent);
                Ok(true)
            }
            Ok(_) => Ok(false),
            Err(e) if e.kind() == ErrorKind::NotFound => Ok(false),
            Err(e) => Err(e),
        }
    }
}

/// The directory that holds `path` under `root`, checked one component at
/// a time with `symlink_metadata`: every existing parent must be a real
/// directory, never a symlink or a file, so nothing is written or removed
/// outside the root. With `create`, missing parents are created one level
/// at a time; without it, a missing parent gives `None`.
fn parent_dir(root: &Path, path: &RepoPath, create: bool) -> Result<Option<PathBuf>> {
    let components: Vec<&str> = path.components().collect();
    let mut dir = root.to_owned();
    for c in &components[..components.len() - 1] {
        dir.push(c);
        match fs::symlink_metadata(&dir) {
            Ok(m) if m.file_type().is_dir() => {}
            Ok(_) => return Err(Error::UnsafePath(path.clone())),
            Err(e) if e.kind() == ErrorKind::NotFound => {
                if !create {
                    return Ok(None);
                }
                match fs::create_dir(&dir) {
                    Ok(()) => {}
                    // Raced with another creator: check what is there now.
                    Err(e) if e.kind() == ErrorKind::AlreadyExists => {
                        if !fs::symlink_metadata(&dir)?.file_type().is_dir() {
                            return Err(Error::UnsafePath(path.clone()));
                        }
                    }
                    Err(e) => return Err(e.into()),
                }
            }
            Err(e) => return Err(e.into()),
        }
    }
    Ok(Some(dir))
}

/// Writes one file (or symlink) and returns its stat. Refuses a path whose
/// parent is not a real directory ([`parent_dir`]). The leaf itself is
/// replaced, never followed: a rename or unlink acts on a symlink, not its
/// target. Refuses a path through git's metadata directory.
fn write_entry(root: &Path, path: &RepoPath, kind: EntryKind, data: &[u8]) -> Result<Stat> {
    // Never write into another tool's metadata, whatever the server sent.
    if path
        .components()
        .any(graft_model::path::is_git_metadata_name)
    {
        return Err(Error::UnsafePath(path.clone()));
    }
    let parent = parent_dir(root, path, true)?.expect("created");
    let target = parent.join(path.name());
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
            // A stale temp file (or a symlink planted under its name) goes
            // first; `create_new` then refuses anything put back.
            match fs::remove_file(&tmp) {
                Err(e) if e.kind() != ErrorKind::NotFound => return Err(e.into()),
                _ => {}
            }
            {
                let mut f = File::options().write(true).create_new(true).open(&tmp)?;
                f.write_all(data)?;
                f.set_permissions(fs::Permissions::from_mode(mode))?;
            }
            fs::rename(&tmp, &target)?;
        }
    }
    Ok(Stat::of(&fs::symlink_metadata(&target)?))
}

/// Removes one file and any directories it leaves empty. Refuses a path
/// whose parent is not a real directory; a missing parent means there is
/// nothing to remove.
fn remove_entry(root: &Path, path: &RepoPath) -> Result<()> {
    let Some(parent) = parent_dir(root, path, false)? else {
        return Ok(());
    };
    match fs::remove_file(parent.join(path.name())) {
        Ok(()) => {}
        Err(e) if e.kind() == ErrorKind::NotFound => {}
        Err(e) => return Err(e.into()),
    }
    let mut dir = Some(parent.as_path());
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
    if name.ends_with(TMP_SUFFIX) || name.ends_with(CONFLICT_SUFFIX) {
        return None;
    }
    match prefix {
        None if is_foreign_metadata(name) => None,
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
        moving: match id("moving.base")? {
            Some(base) if kv.get("moving.head") == Some(&"server") => Some(Moving {
                base,
                head: None,
                by_server: true,
            }),
            Some(base) => Some(Moving::to(base, id("moving.head")?)),
            None => None,
        },
    })
}

/// The nearest directory at or above `path` holding a `.graft` directory.
fn find_root(path: &Path) -> Result<PathBuf> {
    path.ancestors()
        .find(|dir| dir.join(GRAFT_DIR).is_dir())
        .map(Path::to_owned)
        .ok_or_else(|| Error::State("not inside a working copy".to_owned()))
}

fn parse_index(text: &str) -> Result<Index> {
    let bad = || Error::State("malformed index".to_owned());
    let mut lines = text.lines();
    if lines.next() != Some(INDEX_HEADER) {
        return Err(Error::State("unknown index version".to_owned()));
    }
    // The index is written sorted by path, so the entries are gathered in
    // order and the map is built in bulk, without a search per entry.
    let mut sorted: Vec<(RepoPath, IndexEntry)> = Vec::with_capacity(text.len() / 128);
    for line in lines {
        let mut f = line.splitn(8, ' ');
        let mut field = || f.next().ok_or_else(bad);
        let kind = kind_from_name(field()?).ok_or_else(bad)?;
        let id = BlobId(ObjectId::from_hex(field()?).map_err(|_| bad())?);
        let size = field()?.parse::<u64>().map_err(|_| bad())?;
        let mtime_ns = field()?.parse::<i64>().map_err(|_| bad())?;
        let ctime_ns = field()?.parse::<i64>().map_err(|_| bad())?;
        let ino = field()?.parse::<u64>().map_err(|_| bad())?;
        let mode = field()?.parse().map_err(|_| bad())?;
        let raw = field()?;
        let path = if raw.contains('%') {
            unescape(raw).and_then(|p| RepoPath::new(&p).ok())
        } else {
            RepoPath::new(raw).ok()
        }
        .ok_or_else(bad)?;
        let entry = IndexEntry {
            kind,
            id,
            stat: Stat {
                size,
                mtime_ns,
                ctime_ns,
                ino,
                mode,
            },
        };
        sorted.push((path, entry));
    }
    let in_order = sorted.windows(2).all(|w| w[0].0 < w[1].0);
    let entries = if in_order {
        BTreeMap::from_iter(sorted)
    } else {
        // Not written by this code: keep the last entry for a path, as
        // inserting one by one did.
        let mut map = BTreeMap::new();
        for (path, entry) in sorted {
            map.insert(path, entry);
        }
        map
    };
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
    fn reserved_names_cover_case_and_unicode_forms() {
        for name in [
            ".graft",
            ".GRAFT",
            ".Graft",
            ".gRaFt",
            "\u{FF0E}\u{FF47}\u{FF52}\u{FF41}\u{FF46}\u{FF54}", // fullwidth
            ".gr\u{200D}aft",                                   // zero-width joiner
            ".GR\u{00AD}AFT",                                   // soft hyphen
        ] {
            assert!(is_reserved_name(name), "{name:?}");
        }
        for name in [".graf", ".grafts", "graft", ".gräft", "src"] {
            assert!(!is_reserved_name(name), "{name:?}");
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
    fn writes_refuse_git_metadata_in_any_form() {
        let root = tempfile::tempdir().unwrap();
        for path in [
            ".git/config",
            ".GIT/hooks/x",
            "a/git~1/config",
            ".git\u{200c}/x",
        ] {
            let path = RepoPath::new(path).unwrap();
            assert!(matches!(
                write_entry(root.path(), &path, EntryKind::File, b"evil"),
                Err(Error::UnsafePath(_))
            ));
        }
        assert_eq!(std::fs::read_dir(root.path()).unwrap().count(), 0);
        for name in [".git", ".Git", ".HG", ".Jj", ".SVN", "git~1"] {
            assert!(is_foreign_metadata(name), "{name}");
        }
        assert!(!is_foreign_metadata(".github"));
    }

    #[test]
    fn writes_and_removes_refuse_a_parent_that_is_not_a_directory() {
        let root = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        std::fs::write(outside.path().join("x"), b"precious").unwrap();
        std::os::unix::fs::symlink(outside.path(), root.path().join("l")).unwrap();
        std::fs::write(root.path().join("file"), b"f").unwrap();
        for path in ["l/x", "l/new", "l/deep/new", "file/x"] {
            let path = RepoPath::new(path).unwrap();
            assert!(matches!(
                write_entry(root.path(), &path, EntryKind::File, b"evil"),
                Err(Error::UnsafePath(_))
            ));
            assert!(matches!(
                remove_entry(root.path(), &path),
                Err(Error::UnsafePath(_))
            ));
        }
        let names: Vec<_> = std::fs::read_dir(outside.path())
            .unwrap()
            .map(|e| e.unwrap().file_name())
            .collect();
        assert_eq!(names, vec![std::ffi::OsString::from("x")]);
        assert_eq!(
            std::fs::read(outside.path().join("x")).unwrap(),
            b"precious"
        );

        // A leaf symlink is replaced, not followed.
        let leaf = RepoPath::new("leaf").unwrap();
        std::os::unix::fs::symlink(outside.path().join("x"), root.path().join("leaf")).unwrap();
        write_entry(root.path(), &leaf, EntryKind::File, b"mine").unwrap();
        assert_eq!(
            std::fs::read(outside.path().join("x")).unwrap(),
            b"precious"
        );
        assert_eq!(std::fs::read(root.path().join("leaf")).unwrap(), b"mine");
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
            moving: Some(Moving::to(
                ChangesetId::hash(b"tip"),
                Some(ChangesetId::hash(b"next")),
            )),
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
        assert_eq!(opened.index(), wc.index());
        let info = WorkingCopy::open_info(&dir.path().join("a b")).unwrap();
        assert_eq!(
            (info.root(), info.config(), info.state()),
            (dir.path(), &config, &state)
        );
        wc.state.moving = Some(Moving {
            base,
            head: None,
            by_server: true,
        });
        wc.save_state().unwrap();
        let opened = WorkingCopy::open(dir.path()).unwrap();
        assert_eq!(opened.state(), wc.state());
        assert_eq!(opened.sparse(), &sparse);
        assert_eq!(opened.index().entries, wc.index.entries);
        assert!(opened.index().written_ns > 0);
        assert!(matches!(
            WorkingCopy::init(dir.path(), config, state, sparse),
            Err(Error::State(_))
        ));
    }
}
