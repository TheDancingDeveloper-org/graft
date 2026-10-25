//! An in-memory API for the MCP tests. It implements the `graft-proto` traits
//! over a small fixed repository. The standard fixture has no files; the
//! path-bearing tests use [`Fake::with_files`] or [`Fake::with_tree`], which
//! add a tree to every snapshot and file changes to the changesets.
//!
//! A diff between two snapshots is the net effect of the changesets after the
//! first up to and including the second, one entry per path. Patches are
//! synthesised from the fixture's file bytes (every line added), and clipped
//! to the caller's cap as the real API does. Diffs that filter by path are not
//! modelled.

#![allow(dead_code, unreachable_pub)]

pub mod http;

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::sync::Arc;

use graft_model::{
    Author, AuthorKind, BlobId, Change, Changeset, ChangesetId, EntryKind, Metadata, ObjectId,
    ObjectKind, RepoPath, Timestamp, TreeId,
};
use graft_proto::context::{CallContext, Principal, PrincipalKind};
use graft_proto::error::ApiResult;
use graft_proto::types::*;
use graft_proto::{ApiError, RepoApi, WorkspaceApi};

/// A changeset id from one repeated byte.
pub fn cid(byte: u8) -> ChangesetId {
    ChangesetId(ObjectId([byte; 32]))
}

/// The bytes of [`binary_file`]: not UTF-8 (the first byte is never valid),
/// and 40,001 of them, so reads of it need several chunks.
pub fn blob_data() -> Vec<u8> {
    std::iter::once(0xFF)
        .chain((0..40_000u32).map(|i| 0x80 + (i % 128) as u8))
        .collect()
}

/// A text file with one line per entry, each line tagged with the changeset
/// that introduced it.
pub fn text_file<S: AsRef<str>>(lines: &[(S, ChangesetId)]) -> FileSpec {
    let mut data = Vec::new();
    for (text, _) in lines {
        data.extend_from_slice(text.as_ref().as_bytes());
        data.push(b'\n');
    }
    FileSpec {
        data,
        origins: lines.iter().map(|(_, origin)| *origin).collect(),
    }
}

/// A binary file. It has no lines to blame.
pub fn binary_file(data: Vec<u8>) -> FileSpec {
    FileSpec {
        data,
        origins: Vec::new(),
    }
}

/// A `Put` of `spec` at `path`, as a changeset would record it.
pub fn put(path: &str, spec: &FileSpec) -> Change {
    Change::Put {
        path: RepoPath::new(path).expect("fixture paths are valid"),
        kind: EntryKind::File,
        id: BlobId::hash(&spec.data).0,
    }
}

/// A `Delete` of `path`.
pub fn delete(path: &str) -> Change {
    Change::Delete {
        path: RepoPath::new(path).expect("fixture paths are valid"),
    }
}

/// Clips a patch to `max` bytes at a character boundary. Returns the kept
/// text and whether anything was cut.
fn clip_patch(mut patch: String, max: u64) -> (String, bool) {
    let max = usize::try_from(max).unwrap_or(usize::MAX);
    if patch.len() <= max {
        return (patch, false);
    }
    let mut end = max;
    while !patch.is_char_boundary(end) {
        end -= 1;
    }
    patch.truncate(end);
    (patch, true)
}

pub fn principal() -> Principal {
    Principal {
        subject: "test-agent".into(),
        kind: PrincipalKind::Agent,
        session: None,
        work_item: None,
        scopes: vec!["repo:read".into()],
    }
}

/// A file in the fixture tree: its bytes and, per line, the changeset that
/// introduced the line.
pub struct FileSpec {
    data: Vec<u8>,
    origins: Vec<ChangesetId>,
}

impl FileSpec {
    /// The unified patch for the file, with every line added. Binary files
    /// have none.
    fn patch(&self) -> Option<String> {
        let text = std::str::from_utf8(&self.data).ok()?;
        let mut patch = format!("@@ -0,0 +1,{} @@\n", text.lines().count());
        for line in text.lines() {
            patch.push('+');
            patch.push_str(line);
            patch.push('\n');
        }
        Some(patch)
    }
}

/// A fake repository: a linear history `c1 <- c2 <- c3` on `main`.
pub struct Fake {
    changesets: HashMap<ChangesetId, Changeset>,
    refs: HashMap<String, ChangesetId>,
    /// Changesets in order, oldest first.
    order: Vec<ChangesetId>,
    /// The tree every snapshot shows, by path. Empty unless built with
    /// [`Fake::with_files`].
    files: BTreeMap<String, FileSpec>,
    /// Conflicting paths `GetConflicts` returns, when set.
    conflicts: Vec<ConflictView>,
    /// Paths a `sync` reports as changed, when set.
    synced_paths: Vec<RepoPath>,
    /// Draft-overlap hits `OverlappingDrafts` returns, when set.
    /// `(path, workspace, owner)`.
    overlaps: Vec<(String, String, String)>,
    /// Whether `get_workspace` exposes a workspace at c1 on main.
    upstream_workspace: bool,
}

impl Fake {
    /// The standard fixture: three changesets, one ref `main` at the last.
    pub fn standard() -> Arc<Self> {
        Self::with_authors(3, |i| format!("author {i}"), |_| "short subject".into())
    }

    /// The standard history with a tree on every snapshot, and file changes
    /// that a diff shows:
    ///
    /// - `README.md`, `src/lib.rs`, `src/util/mod.rs`, `docs/big.md` (a text
    ///   file whose patch is over the 4 KiB cap) and `bin/blob.dat` (binary);
    /// - `c2` adds `src/lib.rs`; `c3` changes `README.md`, adds `src/util/mod.rs`,
    ///   `docs/big.md` and `bin/blob.dat`, and deletes `src/old.rs`.
    ///
    /// Each line of `README.md` names the changeset that introduced it, so
    /// blame has several origins.
    pub fn with_files() -> Arc<Self> {
        let c = cid;
        let readme = text_file(&[
            ("# fixture", c(1)),
            ("", c(1)),
            ("A repository for the MCP tests.", c(2)),
            ("Each line names the changeset that added it.", c(2)),
            ("Three changesets, one branch.", c(3)),
            ("", c(3)),
        ]);
        let lib = text_file(&[("pub mod util;", c(2))]);
        let util = text_file(&[("pub fn id() -> u8 { 1 }", c(3))]);
        let big_lines: Vec<(String, ChangesetId)> = (0..200)
            .map(|i| (format!("docs line {i:03} {}", "x".repeat(60)), c(3)))
            .collect();
        let big = text_file(&big_lines);
        let blob = binary_file(blob_data());
        let changes = vec![
            (2, put("src/lib.rs", &lib)),
            (3, put("README.md", &readme)),
            (3, put("src/util/mod.rs", &util)),
            (3, put("docs/big.md", &big)),
            (3, put("bin/blob.dat", &blob)),
            (3, delete("src/old.rs")),
        ];
        let tree = vec![
            ("README.md".into(), readme),
            ("src/lib.rs".into(), lib),
            ("src/util/mod.rs".into(), util),
            ("docs/big.md".into(), big),
            ("bin/blob.dat".into(), blob),
        ];
        Self::with_tree(tree, changes)
    }

    /// The standard history with `tree` on every snapshot and `changes`
    /// applied to the changesets at the given 1-based indices.
    pub fn with_tree(tree: Vec<(String, FileSpec)>, changes: Vec<(u8, Change)>) -> Arc<Self> {
        let mut fake = Self::linear(3, |i| format!("author {i}"), |_| "short subject".into());
        for (path, spec) in tree {
            fake.files.insert(path, spec);
        }
        for (at, change) in changes {
            fake.changesets
                .get_mut(&cid(at))
                .expect("the fixture has changesets 1 to 3")
                .changes
                .push(change);
        }
        Arc::new(fake)
    }

    /// A linear chain of `count` changesets (`cid(1)` to `cid(count)`), with
    /// `main` at the last one. Each author name and subject comes from
    /// `name` and `subject`, so tests can push fields to their clip limits.
    pub fn with_authors(
        count: u8,
        name: impl Fn(u8) -> String,
        subject: impl Fn(u8) -> String,
    ) -> Arc<Self> {
        Arc::new(Self::linear(count, name, subject))
    }

    fn linear(count: u8, name: impl Fn(u8) -> String, subject: impl Fn(u8) -> String) -> Self {
        let mut fake = Fake {
            changesets: HashMap::new(),
            refs: HashMap::new(),
            order: Vec::new(),
            files: BTreeMap::new(),
            conflicts: Vec::new(),
            synced_paths: Vec::new(),
            overlaps: Vec::new(),
            upstream_workspace: false,
        };
        for i in 1..=count {
            let id = cid(i);
            let parents = if i == 1 { vec![] } else { vec![cid(i - 1)] };
            let changeset = Changeset {
                parents,
                root: TreeId(ObjectId([0x80 + i; 32])),
                changes: vec![],
                author: Author {
                    name: name(i),
                    email: format!("a{i}@example.test"),
                    kind: AuthorKind::Agent,
                },
                authored_at: Timestamp {
                    ms: 1_700_000_000_000 + i64::from(i) * 60_000,
                    offset_min: 0,
                },
                message: format!("{}\n\nbody {i}", subject(i)),
                metadata: Metadata::from([("graft.test".to_owned(), format!("run {i}"))]),
            };
            fake.changesets.insert(id, changeset);
            fake.order.push(id);
        }
        fake.refs.insert("main".into(), cid(count));
        fake
    }

    fn unavailable<T>() -> ApiResult<T> {
        Err(ApiError::Unavailable { retry_after_ms: 0 })
    }

    /// The position of a changeset in `order`, oldest first.
    fn position(&self, id: ChangesetId) -> ApiResult<usize> {
        self.order
            .iter()
            .position(|x| *x == id)
            .ok_or_else(|| ApiError::NotFound {
                what: format!("changeset {id}"),
            })
    }

    fn check_at(&self, at: ChangesetId) -> ApiResult<()> {
        if self.changesets.contains_key(&at) {
            return Ok(());
        }
        Err(ApiError::NotFound {
            what: format!("changeset {at}"),
        })
    }

    fn file(&self, path: &RepoPath) -> ApiResult<&FileSpec> {
        self.files
            .get(path.as_str())
            .ok_or_else(|| ApiError::NotFound {
                what: format!("file {path}"),
            })
    }

    /// The entry for a file in the fixture tree.
    fn file_entry(path: &str, spec: &FileSpec) -> EntryView {
        EntryView {
            path: RepoPath::new(path).expect("fixture paths are valid"),
            kind: EntryKind::File,
            id: BlobId::hash(&spec.data).0,
        }
    }

    /// The entry for a directory in the fixture tree.
    fn dir_entry(path: &str) -> EntryView {
        EntryView {
            path: RepoPath::new(path).expect("fixture paths are valid"),
            kind: EntryKind::Tree,
            id: ObjectId::hash(ObjectKind::Tree, path.as_bytes()),
        }
    }

    fn summary(&self, id: ChangesetId) -> ChangesetSummary {
        let cs = &self.changesets[&id];
        let subject = cs.message.lines().next().unwrap_or("").to_owned();
        ChangesetSummary {
            id,
            author: cs.author.clone(),
            authored_at: cs.authored_at,
            subject,
            line_pos: None,
        }
    }

    /// A fixture whose `GetConflicts` returns `n` paths, each with markers of
    /// `marker_bytes`.
    pub fn with_conflicts(n: usize, marker_bytes: usize) -> Arc<Self> {
        let mut fake = Self::linear(1, |_| "author".into(), |_| "subject".into());
        fake.conflicts = (0..n)
            .map(|i| ConflictView {
                path: RepoPath::new(&format!("src/f{i}.rs")).unwrap(),
                base: None,
                ours: None,
                theirs: None,
                text_mergeable: true,
                markers: Some(vec![b'x'; marker_bytes]),
                reason: None,
            })
            .collect();
        Arc::new(fake)
    }

    /// A fixture whose sync reports `n` changed paths.
    pub fn with_synced_paths(n: usize) -> Arc<Self> {
        let mut fake = Self::linear(1, |_| "author".into(), |_| "subject".into());
        fake.synced_paths = (0..n)
            .map(|i| RepoPath::new(&format!("src/p{i}.rs")).unwrap())
            .collect();
        Arc::new(fake)
    }

    /// A fixture whose draft-overlap read returns `n` hits. Each hit is its
    /// own workspace, with an owner of `owner_bytes`.
    pub fn with_overlaps(n: usize, owner_bytes: usize) -> Arc<Self> {
        let mut fake = Self::linear(1, |_| "author".into(), |_| "subject".into());
        let owner = "o".repeat(owner_bytes);
        fake.overlaps = (0..n)
            .map(|i| (format!("src/h{i}.rs"), format!("ws{i}"), owner.clone()))
            .collect();
        Arc::new(fake)
    }

    /// A workspace based at c1 while main has `n` long changed paths at c2.
    pub fn with_upstream_paths(n: usize, path_bytes: usize) -> Arc<Self> {
        let mut fake = Self::linear(2, |_| "author".into(), |_| "subject".into());
        let stem = "x".repeat(path_bytes.min(200));
        fake.changesets.get_mut(&cid(2)).unwrap().changes = (0..n)
            .map(|i| {
                let path = format!("{stem}/{stem}/{stem}/{i:03}.txt");
                Change::Put {
                    path: RepoPath::new(&path).unwrap(),
                    kind: EntryKind::File,
                    id: ObjectId::hash(ObjectKind::Blob, i.to_string().as_bytes()),
                }
            })
            .collect();
        fake.upstream_workspace = true;
        Arc::new(fake)
    }
}

impl RepoApi for Fake {
    async fn create_changeset(&self, _: &CallContext, _: CreateChangeset) -> ApiResult<Created> {
        Self::unavailable()
    }

    async fn get_changesets(&self, _: &CallContext, req: GetChangesets) -> ApiResult<Changesets> {
        let mut out = Changesets::default();
        for id in req.ids {
            match self.changesets.get(&id) {
                Some(cs) => out.found.push(ChangesetView {
                    changeset: cs.clone(),
                    lsn: self.order.iter().position(|x| *x == id).unwrap_or(0) as u64 + 1,
                    state: DraftState::Landed,
                    superseded_by: None,
                }),
                None => out.missing.push(id),
            }
        }
        Ok(out)
    }

    async fn resolve_ref(&self, _: &CallContext, req: ResolveRef) -> ApiResult<RefView> {
        match self.refs.get(&req.name) {
            Some(target) => Ok(RefView {
                name: req.name,
                target: *target,
                version: 1,
                kind: RefKind::Line,
            }),
            None => Err(ApiError::NotFound {
                what: format!("ref {}", req.name),
            }),
        }
    }

    async fn list_refs(&self, _: &CallContext, _: ListRefs) -> ApiResult<Vec<RefView>> {
        Self::unavailable()
    }

    async fn update_ref(&self, _: &CallContext, _: UpdateRef) -> ApiResult<RefView> {
        Self::unavailable()
    }

    async fn stat(&self, _: &CallContext, _: PathAt) -> ApiResult<EntryView> {
        Self::unavailable()
    }

    async fn list_tree(&self, _: &CallContext, req: ListTree) -> ApiResult<Entries> {
        self.check_at(req.at)?;
        let prefix = req
            .path
            .as_ref()
            .map(|p| format!("{p}/"))
            .unwrap_or_default();
        let mut entries = Vec::new();
        let mut dirs = BTreeSet::new();
        for (path, spec) in &self.files {
            let Some(rest) = path.strip_prefix(&prefix) else {
                continue;
            };
            match rest.split_once('/') {
                Some((dir, _)) if !req.recursive => {
                    if dirs.insert(format!("{prefix}{dir}")) {
                        entries.push(Self::dir_entry(&format!("{prefix}{dir}")));
                    }
                }
                _ => entries.push(Self::file_entry(path, spec)),
            }
        }
        if let Some(dir) = req.path.as_ref().filter(|_| entries.is_empty()) {
            return Err(ApiError::NotFound {
                what: format!("directory {dir}"),
            });
        }
        entries.sort_by(|a, b| a.path.as_str().cmp(b.path.as_str()));
        let start = match &req.page.token {
            Some(token) => token.parse::<usize>().map_err(|_| ApiError::NotFound {
                what: "cursor".into(),
            })?,
            None => 0,
        };
        let start = start.min(entries.len());
        let end = (start + req.page.limit as usize).min(entries.len());
        let next = (end < entries.len()).then(|| end.to_string());
        Ok(Entries {
            entries: entries[start..end].to_vec(),
            next,
        })
    }

    async fn read_file(&self, _: &CallContext, req: PathAt) -> ApiResult<FileContent> {
        self.check_at(req.at)?;
        let spec = self.file(&req.path)?;
        Ok(FileContent {
            kind: EntryKind::File,
            id: BlobId::hash(&spec.data),
            data: spec.data.clone(),
        })
    }

    async fn log(&self, _: &CallContext, req: Log) -> ApiResult<History> {
        let start = self
            .order
            .iter()
            .position(|id| *id == req.from)
            .ok_or_else(|| ApiError::NotFound {
                what: format!("changeset {}", req.from),
            })?;
        // The cursor is the order index of the next changeset to return.
        let first = match &req.page.token {
            Some(token) => token.parse::<usize>().map_err(|_| ApiError::NotFound {
                what: "cursor".into(),
            })?,
            None => start,
        };
        let indices: Vec<usize> = (0..=first).rev().collect();
        let take = (req.page.limit as usize).min(indices.len());
        let changesets = indices[..take]
            .iter()
            .map(|i| self.summary(self.order[*i]))
            .collect();
        let next = indices.get(take).map(|i| i.to_string());
        Ok(History { changesets, next })
    }

    async fn is_ancestor(&self, _: &CallContext, _: Pair) -> ApiResult<bool> {
        Self::unavailable()
    }

    async fn merge_base(&self, _: &CallContext, _: Pair) -> ApiResult<Option<ChangesetId>> {
        Self::unavailable()
    }

    async fn diff(&self, _: &CallContext, req: Diff) -> ApiResult<DiffResult> {
        if !req.paths.is_empty() {
            return Self::unavailable();
        }
        let from = self.position(req.from)?;
        let to = self.position(req.to)?;
        // The last change to each path in the changesets after `from`, up to
        // and including `to`. A range that runs backwards has no changes.
        let mut net: BTreeMap<&str, &Change> = BTreeMap::new();
        for id in self
            .order
            .iter()
            .skip(from + 1)
            .take(to.saturating_sub(from))
        {
            for change in &self.changesets[id].changes {
                net.insert(change.path().as_str(), change);
            }
        }
        let all: Vec<&Change> = net.into_values().collect();
        let start = match &req.page.token {
            Some(token) => token.parse::<usize>().map_err(|_| ApiError::NotFound {
                what: "cursor".into(),
            })?,
            None => 0,
        }
        .min(all.len());
        let end = (start + req.page.limit as usize).min(all.len());
        let files = all[start..end]
            .iter()
            .map(|change| {
                let (patch, truncated) = match (&req.patch, change) {
                    (Some(opts), Change::Put { path, .. }) => {
                        match self.files.get(path.as_str()).and_then(FileSpec::patch) {
                            Some(text) => {
                                let (text, cut) = clip_patch(text, opts.max_bytes_per_file);
                                (Some(text), cut)
                            }
                            None => (None, false),
                        }
                    }
                    _ => (None, false),
                };
                FileDiff {
                    change: (*change).clone(),
                    patch,
                    truncated,
                }
            })
            .collect();
        Ok(DiffResult {
            files,
            next: (end < all.len()).then(|| end.to_string()),
        })
    }

    async fn blame(&self, _: &CallContext, req: Blame) -> ApiResult<BlameResult> {
        self.check_at(req.at)?;
        let spec = self.file(&req.path)?;
        let (start, end) = req.lines.unwrap_or((0, u32::MAX));
        let end = end.min(spec.origins.len() as u32);
        let mut ranges: Vec<BlameRange> = Vec::new();
        for line in start..end {
            let origin = spec.origins[line as usize];
            match ranges.last_mut() {
                Some(run) if run.origin.id == origin && run.end == line => run.end = line + 1,
                _ => ranges.push(BlameRange {
                    start: line,
                    end: line + 1,
                    origin: self.summary(origin),
                }),
            }
        }
        Ok(BlameResult { ranges })
    }

    async fn rebase(&self, _: &CallContext, _: Rebase) -> ApiResult<ChangesetId> {
        Self::unavailable()
    }

    async fn revert(&self, _: &CallContext, _: Revert) -> ApiResult<ChangesetId> {
        Self::unavailable()
    }

    async fn land(&self, _: &CallContext, _: Land) -> ApiResult<Landed> {
        Self::unavailable()
    }

    async fn attest(&self, _: &CallContext, _: Attest) -> ApiResult<()> {
        Self::unavailable()
    }

    async fn watch(&self, _: &CallContext, _: Watch) -> ApiResult<JournalPage> {
        Self::unavailable()
    }
}

impl WorkspaceApi for Fake {
    async fn open_workspace(&self, _: &CallContext, _: OpenWorkspace) -> ApiResult<WorkspaceView> {
        Self::unavailable()
    }

    async fn get_workspace(&self, _: &CallContext, req: WorkspaceRef) -> ApiResult<WorkspaceView> {
        if !self.upstream_workspace {
            return Self::unavailable();
        }
        Ok(WorkspaceView {
            name: req.name,
            owner: "test-agent".into(),
            session: None,
            line: "main".into(),
            base: cid(1),
            head: None,
            lease_until_ms: 0,
        })
    }

    async fn list_workspaces(
        &self,
        _: &CallContext,
        _: String,
        _: Page,
    ) -> ApiResult<Vec<WorkspaceView>> {
        Self::unavailable()
    }

    async fn renew_lease(
        &self,
        _: &CallContext,
        _: WorkspaceRef,
        _: u64,
    ) -> ApiResult<WorkspaceView> {
        Self::unavailable()
    }

    async fn set_head(&self, _: &CallContext, _: SetHead) -> ApiResult<WorkspaceView> {
        Self::unavailable()
    }

    async fn sync(
        &self,
        _: &CallContext,
        req: WorkspaceRef,
        _: Option<ChangesetId>,
    ) -> ApiResult<Synced> {
        if self.synced_paths.is_empty() {
            return Self::unavailable();
        }
        Ok(Synced {
            workspace: WorkspaceView {
                name: req.name,
                owner: "test-agent".into(),
                session: None,
                line: "main".into(),
                base: cid(1),
                head: Some(cid(1)),
                lease_until_ms: 0,
            },
            changed_paths: self.synced_paths.clone(),
        })
    }

    async fn stack(
        &self,
        _: &CallContext,
        _: WorkspaceRef,
        _: StackOp,
    ) -> ApiResult<WorkspaceView> {
        Self::unavailable()
    }

    async fn close_workspace(&self, _: &CallContext, _: WorkspaceRef, _: bool) -> ApiResult<()> {
        Self::unavailable()
    }

    async fn get_conflicts(
        &self,
        _cx: &graft_proto::CallContext,
        req: graft_proto::types::GetConflicts,
    ) -> graft_proto::error::ApiResult<graft_proto::types::Conflicts> {
        if self.conflicts.is_empty() {
            return Err(graft_proto::ApiError::unimplemented("GetConflicts"));
        }
        let start = req
            .page
            .token
            .as_deref()
            .and_then(|t| t.parse::<usize>().ok())
            .unwrap_or(0);
        let end = (start + req.page.limit as usize).min(self.conflicts.len());
        let next = (end < self.conflicts.len()).then(|| end.to_string());
        Ok(Conflicts {
            onto: cid(1),
            draft: Some(cid(2)),
            paths: self.conflicts[start..end].to_vec(),
            next,
        })
    }

    async fn resolve(
        &self,
        _cx: &graft_proto::CallContext,
        _req: graft_proto::types::Resolve,
    ) -> graft_proto::error::ApiResult<graft_proto::types::Resolved> {
        Err(graft_proto::ApiError::unimplemented("Resolve"))
    }

    async fn overlapping_drafts(
        &self,
        _: &graft_proto::CallContext,
        req: graft_proto::types::OverlappingDrafts,
    ) -> graft_proto::error::ApiResult<graft_proto::types::DraftOverlap> {
        if self.overlaps.is_empty() {
            return Err(graft_proto::ApiError::unimplemented("OverlappingDrafts"));
        }
        let start = req
            .page
            .token
            .as_deref()
            .and_then(|t| t.parse::<usize>().ok())
            .unwrap_or(0);
        let end = (start + req.page.limit as usize).min(self.overlaps.len());
        let page = &self.overlaps[start..end];
        let next = (end < self.overlaps.len()).then(|| end.to_string());
        use graft_proto::types::{DraftHit, DraftWorkspace};
        Ok(graft_proto::types::DraftOverlap {
            hits: page
                .iter()
                .map(|(path, workspace, _)| DraftHit {
                    path: RepoPath::new(path).expect("fixture path"),
                    workspace: workspace.clone(),
                })
                .collect(),
            workspaces: page
                .iter()
                .map(|(_, workspace, owner)| DraftWorkspace {
                    workspace: workspace.clone(),
                    owner: owner.clone(),
                    session: None,
                    work_item: None,
                    draft_age_ms: 1,
                    lease_until_ms: 0,
                })
                .collect(),
            next,
        })
    }
}

use graft_mcp::McpServer;
use rmcp::ServiceExt;
use rmcp::model::{CallToolRequestParams, CallToolResult};
use rmcp::service::{RoleClient, RunningService};

/// Serves `api` over an in-memory duplex with stdio framing and returns a
/// connected client.
pub async fn connect(api: Arc<Fake>) -> RunningService<RoleClient, ()> {
    connect_with(api, None).await
}

/// Like [`connect`], optionally attaching a working copy.
pub async fn connect_with(
    api: Arc<Fake>,
    working_copy: Option<Arc<dyn graft_mcp::WorkingCopyTools>>,
) -> RunningService<RoleClient, ()> {
    let (client_io, server_io) = tokio::io::duplex(1 << 20);
    let (server_read, server_write) = tokio::io::split(server_io);
    let mut server = McpServer::new(api, principal());
    if let Some(wc) = working_copy {
        server = server.with_working_copy(wc);
    }
    tokio::spawn(async move {
        let _ = server.serve_io(server_read, server_write).await;
    });
    let (client_read, client_write) = tokio::io::split(client_io);
    ().serve((client_read, client_write))
        .await
        .expect("the MCP handshake completes")
}

/// Calls a tool with a JSON object of arguments.
pub async fn call(
    client: &RunningService<RoleClient, ()>,
    tool: &'static str,
    args: serde_json::Value,
) -> CallToolResult {
    let arguments = args.as_object().expect("arguments are an object").clone();
    client
        .call_tool(CallToolRequestParams::new(tool).with_arguments(arguments))
        .await
        .expect("the call completes")
}

/// Compares `actual` with `tests/golden/<name>.json`, byte for byte. With
/// `GRAFT_MCP_BLESS=1` the golden is written instead; review the diff before
/// committing it.
pub fn assert_golden(name: &str, actual: &serde_json::Value) {
    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/golden")
        .join(format!("{name}.json"));
    let pretty = format!("{}\n", serde_json::to_string_pretty(actual).unwrap());
    if std::env::var_os("GRAFT_MCP_BLESS").is_some() {
        std::fs::write(&path, &pretty).expect("write golden");
    }
    let expected = std::fs::read_to_string(&path)
        .unwrap_or_else(|_| panic!("no golden {name}; run with GRAFT_MCP_BLESS=1"));
    assert_eq!(expected, pretty, "golden {name} differs");
}
