//! Named-object fetch (`docs/design.md` §7.3): breadth-first, pipelined,
//! resumable, verified.
//!
//! There is no negotiation. The client walks the snapshot from its root tree
//! through the sparse slice and asks for the objects its cache lacks, in
//! batches of ids. Up to [`FetchOptions::window`] batches are in flight at
//! once, and each answered tree batch is expanded as soon as it arrives, so
//! the next level's requests go out while the current level is still
//! arriving. Trees go before blobs. Every frame is verified against its id
//! before it is cached. Objects are immutable, so a fetch that fails part
//! way resumes by running again: what reached the cache is read locally and
//! only the rest is requested.

use std::collections::{HashMap, HashSet, VecDeque};

use futures_util::StreamExt;
use futures_util::stream::FuturesUnordered;
use graft_model::{ChangesetId, EntryKind, ObjectId, ObjectKind, RepoPath, Tree, TreeId};
use graft_proto::api::{ObjectApi, RepoApi};
use graft_proto::types::{GetChangesets, GetObjects, Objects};
use graft_proto::{ApiError, CallContext, Frame};
use graft_store::{ObjectStore, RepoId};

use crate::{Error, Result, Sparse};

/// The repository id objects are cached under: a cache holds one
/// repository.
pub const CACHE_REPO: RepoId = RepoId(0);

/// The most ids one `GetObjects` call may name (`docs/design.md` §7.2).
pub const MAX_BATCH: usize = 1024;

/// Consecutive answers with no progress (everything deferred) before the
/// fetch gives up with `Unavailable`.
const MAX_STALLS: u32 = 8;

/// Limits on the work one fetch, checkout or update does for a server, so a
/// hostile or broken server can't make the client do unbounded work. When a
/// limit is reached the operation stops with
/// `ApiError::LimitExceeded { limit, max }`, `limit` naming the field.
///
/// - `max_entries`: tree entries examined (fetch, checkout) or changes
///   applied (update). A snapshot whose trees repeat a child under several
///   names has 2^depth paths while it has only `depth` distinct trees. The
///   fetch reads each distinct tree once, but checkout writes every path, so
///   this bound is what stops it.
/// - `max_bytes`: decoded object bytes received (fetch) or file bytes
///   written (checkout, update).
/// - `max_depth`: directory depth below the root.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Budget {
    /// Tree entries examined, or changes applied.
    pub max_entries: u64,
    /// Object bytes received, or file bytes written.
    pub max_bytes: u64,
    /// Directory depth.
    pub max_depth: usize,
}

impl Default for Budget {
    /// 10 million entries, 64 GiB, and depth 256.
    fn default() -> Self {
        Self {
            max_entries: 10_000_000,
            max_bytes: 64 * 1024 * 1024 * 1024,
            max_depth: 256,
        }
    }
}

/// The error for a spent budget field.
pub(crate) fn over_budget(limit: &str, max: u64) -> Error {
    Error::Api(ApiError::LimitExceeded {
        limit: format!("client.{limit}"),
        max,
    })
}

/// How a fetch batches and pipelines its requests, and its budget.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FetchOptions {
    /// Ids per request (clamped to `1..=MAX_BATCH`).
    pub batch: usize,
    /// Requests in flight at once (at least one).
    pub window: usize,
    /// Work limits.
    pub budget: Budget,
}

impl Default for FetchOptions {
    fn default() -> Self {
        Self {
            batch: 256,
            window: 4,
            budget: Budget::default(),
        }
    }
}

/// What a fetch did.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct FetchStats {
    /// Trees received.
    pub trees: u64,
    /// Blobs received.
    pub blobs: u64,
    /// Bytes received (encoded frame bodies).
    pub bytes: u64,
    /// Requests sent.
    pub requests: u64,
    /// Objects found in the cache, so not requested.
    pub cached: u64,
}

impl FetchStats {
    /// Adds `other`'s counts to these.
    pub fn add(&mut self, other: &FetchStats) {
        self.trees += other.trees;
        self.blobs += other.blobs;
        self.bytes += other.bytes;
        self.requests += other.requests;
        self.cached += other.cached;
    }
}

/// The id of the empty tree, which no store holds: an empty snapshot.
pub fn empty_tree() -> TreeId {
    Tree::default().id()
}

/// Reads a changeset's root tree id, checking the changeset hashes to `at`.
pub async fn resolve_root<R: RepoApi>(
    api: &R,
    cx: &CallContext,
    repo: &str,
    at: ChangesetId,
) -> Result<TreeId> {
    let got = api
        .get_changesets(
            cx,
            GetChangesets {
                repo: repo.to_owned(),
                ids: vec![at],
            },
        )
        .await?;
    if got.collected.contains(&at) {
        return Err(ApiError::Collected { id: at }.into());
    }
    got.found
        .into_iter()
        .find(|v| v.changeset.id() == at)
        .map(|v| v.changeset.root)
        .ok_or_else(|| {
            ApiError::NotFound {
                what: format!("changeset {at}"),
            }
            .into()
        })
}

/// Fetches every object of the snapshot rooted at `root` that lies in
/// `sparse` (plus the trees leading to its prefixes) and that `cache`
/// lacks.
pub async fn fetch_slice<A: ObjectApi, S: ObjectStore>(
    api: &A,
    cache: &S,
    cx: &CallContext,
    repo: &str,
    root: TreeId,
    sparse: &Sparse,
    opts: &FetchOptions,
) -> Result<FetchStats> {
    let mut walk = Walk::new(cache, Some(sparse), opts.budget);
    if root != empty_tree() {
        walk.want(vec![Want::tree(root.0, None)]).await?;
    }
    run(api, cx, repo, opts, &mut walk).await?;
    Ok(walk.stats)
}

/// Fetches the listed objects that `cache` lacks, without walking into
/// trees.
pub async fn fetch_objects<A: ObjectApi, S: ObjectStore>(
    api: &A,
    cache: &S,
    cx: &CallContext,
    repo: &str,
    ids: &[(ObjectId, ObjectKind)],
    opts: &FetchOptions,
) -> Result<FetchStats> {
    let mut walk = Walk::new(cache, None, opts.budget);
    walk.want(
        ids.iter()
            .map(|&(id, kind)| Want {
                id,
                kind,
                dir: None,
            })
            .collect(),
    )
    .await?;
    run(api, cx, repo, opts, &mut walk).await?;
    Ok(walk.stats)
}

/// Reads and verifies a cached tree. A cached frame that fails verification
/// is removed and reported as absent, so the next fetch refills it.
pub async fn load_tree<S: ObjectStore>(cache: &S, id: TreeId) -> Result<Option<Tree>> {
    if id == empty_tree() {
        return Ok(Some(Tree::default()));
    }
    let Some(payload) = load(cache, id.0, ObjectKind::Tree).await? else {
        return Ok(None);
    };
    match Tree::decode(&payload) {
        Ok(t) => Ok(Some(t)),
        Err(_) => {
            cache.delete(CACHE_REPO, id.0).await?;
            Ok(None)
        }
    }
}

/// Reads and verifies a cached blob, as [`load_tree`] does a tree.
pub async fn load_blob<S: ObjectStore>(cache: &S, id: ObjectId) -> Result<Option<Vec<u8>>> {
    load(cache, id, ObjectKind::Blob).await
}

async fn load<S: ObjectStore>(
    cache: &S,
    id: ObjectId,
    kind: ObjectKind,
) -> Result<Option<Vec<u8>>> {
    let Some(bytes) = cache.get(CACHE_REPO, id).await? else {
        return Ok(None);
    };
    let payload = Frame::from_bytes(&bytes)
        .ok()
        .filter(|f| f.kind == kind)
        .and_then(|f| f.decode_verified(id).ok());
    if payload.is_none() {
        cache.delete(CACHE_REPO, id).await?;
    }
    Ok(payload)
}

/// One wanted object; for a tree, the directory it is (`None` for the root).
#[derive(Clone, Debug)]
struct Want {
    id: ObjectId,
    kind: ObjectKind,
    dir: Option<RepoPath>,
}

impl Want {
    fn tree(id: ObjectId, dir: Option<RepoPath>) -> Self {
        Self {
            id,
            kind: ObjectKind::Tree,
            dir,
        }
    }

    fn blob(id: ObjectId) -> Self {
        Self {
            id,
            kind: ObjectKind::Blob,
            dir: None,
        }
    }
}

/// The fetch's state: what to request, what to expand, what was seen.
struct Walk<'a, S> {
    cache: &'a S,
    /// The slice to walk; `None` fetches only the named objects.
    sparse: Option<&'a Sparse>,
    seen: HashSet<ObjectId>,
    trees: VecDeque<Want>,
    blobs: VecDeque<Want>,
    /// Trees in hand whose entries are not yet examined.
    local: VecDeque<(TreeId, Tree, Option<RepoPath>)>,
    stats: FetchStats,
    budget: Budget,
    /// Tree entries examined so far.
    entries: u64,
    /// Decoded bytes received so far.
    received: u64,
}

impl<'a, S: ObjectStore> Walk<'a, S> {
    fn new(cache: &'a S, sparse: Option<&'a Sparse>, budget: Budget) -> Self {
        Self {
            cache,
            sparse,
            seen: HashSet::new(),
            trees: VecDeque::new(),
            blobs: VecDeque::new(),
            local: VecDeque::new(),
            stats: FetchStats::default(),
            budget,
            entries: 0,
            received: 0,
        }
    }

    /// Queues objects not seen before: cached ones are expanded locally (or
    /// skipped, for blobs), the rest are queued for a request.
    async fn want(&mut self, wants: Vec<Want>) -> Result<()> {
        let fresh: Vec<Want> = wants
            .into_iter()
            .filter(|w| self.seen.insert(w.id))
            .collect();
        if fresh.is_empty() {
            return Ok(());
        }
        let ids: Vec<ObjectId> = fresh.iter().map(|w| w.id).collect();
        let present: HashSet<ObjectId> = self
            .cache
            .has(CACHE_REPO, &ids)
            .await?
            .into_iter()
            .collect();
        for w in fresh {
            let walking = self.sparse.is_some() && w.kind == ObjectKind::Tree;
            if present.contains(&w.id) {
                if !walking {
                    self.stats.cached += 1;
                    continue;
                }
                // It may have been evicted, or be corrupt; then fetch it.
                if let Some(tree) = load_tree(self.cache, TreeId(w.id)).await? {
                    self.stats.cached += 1;
                    self.local.push_back((TreeId(w.id), tree, w.dir));
                    continue;
                }
            }
            self.queue(w);
        }
        Ok(())
    }

    fn queue(&mut self, w: Want) {
        match w.kind {
            ObjectKind::Tree => self.trees.push_back(w),
            _ => self.blobs.push_back(w),
        }
    }

    /// The wants under `tree` that lie in the slice, charged to the budget.
    fn expand(&mut self, id: TreeId, tree: &Tree, dir: Option<&RepoPath>) -> Result<Vec<Want>> {
        let Some(sparse) = self.sparse else {
            return Ok(Vec::new());
        };
        self.entries += tree.entries.len() as u64;
        if self.entries > self.budget.max_entries {
            return Err(over_budget("max_entries", self.budget.max_entries));
        }
        let depth = dir.map_or(0, |d| d.components().count());
        if depth >= self.budget.max_depth && tree.entries.iter().any(|e| e.kind.is_tree()) {
            return Err(over_budget("max_depth", self.budget.max_depth as u64));
        }
        let mut out = Vec::new();
        for e in &tree.entries {
            let path = match dir {
                None => RepoPath::new(&e.name),
                Some(d) => d.join(&e.name),
            }
            .map_err(|_| ApiError::CorruptObject { id: id.0 })?;
            if e.kind.is_tree() {
                if sparse.descends(&path) {
                    out.push(Want::tree(e.id, Some(path)));
                }
            } else if e.kind != EntryKind::Submodule && sparse.includes(&path) {
                out.push(Want::blob(e.id));
            }
        }
        Ok(out)
    }

    /// Expands every tree in hand.
    async fn expand_local(&mut self) -> Result<()> {
        while let Some((id, tree, dir)) = self.local.pop_front() {
            let wants = self.expand(id, &tree, dir.as_ref())?;
            self.want(wants).await?;
        }
        Ok(())
    }

    /// The next batch to request, if one is due: trees first; blobs once a
    /// full batch is queued, or when no tree is pending that could add more.
    fn next_batch(&mut self, batch: usize, trees_in_flight: usize) -> Option<Vec<Want>> {
        let take = |q: &mut VecDeque<Want>| q.drain(..batch.min(q.len())).collect();
        if !self.trees.is_empty() {
            return Some(take(&mut self.trees));
        }
        if self.blobs.len() >= batch || (!self.blobs.is_empty() && trees_in_flight == 0) {
            return Some(take(&mut self.blobs));
        }
        None
    }

    /// Verifies, caches and queues the answer to one batch. Returns whether
    /// anything arrived.
    async fn receive(&mut self, batch: Vec<Want>, objects: Objects) -> Result<bool> {
        let mut asked: HashMap<ObjectId, Want> = batch.into_iter().map(|w| (w.id, w)).collect();
        let mut progress = false;
        for (id, frame) in objects.found {
            let Some(w) = asked.remove(&id) else {
                continue;
            };
            if frame.kind != w.kind {
                return Err(ApiError::CorruptObject { id }.into());
            }
            let payload = frame
                .decode_verified(id)
                .map_err(|_| ApiError::CorruptObject { id })?;
            self.received += payload.len() as u64;
            if self.received > self.budget.max_bytes {
                return Err(over_budget("max_bytes", self.budget.max_bytes));
            }
            let tree = match w.kind {
                ObjectKind::Tree => {
                    Some(Tree::decode(&payload).map_err(|_| ApiError::CorruptObject { id })?)
                }
                _ => None,
            };
            self.cache.put(CACHE_REPO, id, frame.to_bytes()).await?;
            self.stats.bytes += frame.body.len() as u64;
            progress = true;
            match tree {
                Some(tree) => {
                    self.stats.trees += 1;
                    if self.sparse.is_some() {
                        self.local.push_back((TreeId(id), tree, w.dir));
                    }
                }
                None => self.stats.blobs += 1,
            }
        }
        let missing: Vec<ObjectId> = objects
            .missing
            .into_iter()
            .filter(|id| asked.contains_key(id))
            .collect();
        if !missing.is_empty() {
            return Err(ApiError::MissingObjects { ids: missing }.into());
        }
        // Deferred, or not answered at all: ask again.
        for (_, w) in asked {
            self.queue(w);
        }
        Ok(progress)
    }
}

/// The request loop: keeps up to `window` batches in flight and expands
/// each answer as it arrives.
async fn run<A: ObjectApi, S: ObjectStore>(
    api: &A,
    cx: &CallContext,
    repo: &str,
    opts: &FetchOptions,
    walk: &mut Walk<'_, S>,
) -> Result<()> {
    let batch = opts.batch.clamp(1, MAX_BATCH);
    let window = opts.window.max(1);
    let issue = |wants: Vec<Want>| {
        let req = GetObjects {
            repo: repo.to_owned(),
            ids: wants.iter().map(|w| w.id).collect(),
        };
        async move { (wants, api.get_objects(cx, req).await) }
    };
    let mut inflight = FuturesUnordered::new();
    let mut trees_in_flight = 0;
    let mut stalls = 0;
    loop {
        walk.expand_local().await?;
        while inflight.len() < window {
            let Some(wants) = walk.next_batch(batch, trees_in_flight) else {
                break;
            };
            if wants[0].kind == ObjectKind::Tree {
                trees_in_flight += 1;
            }
            inflight.push(issue(wants));
        }
        let Some((wants, answer)) = inflight.next().await else {
            break;
        };
        if wants[0].kind == ObjectKind::Tree {
            trees_in_flight -= 1;
        }
        walk.stats.requests += 1;
        if walk.receive(wants, answer?).await? {
            stalls = 0;
        } else {
            stalls += 1;
            if stalls >= MAX_STALLS {
                return Err(Error::Api(ApiError::Unavailable {
                    retry_after_ms: 1000,
                }));
            }
        }
    }
    Ok(())
}
