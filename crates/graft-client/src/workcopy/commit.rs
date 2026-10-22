//! `commit` (`docs/design.md` §3, §7.4, §8.5, §9.2): turns the slice's
//! changes into a draft on the server.
//!
//! A commit reads each changed file once, uploads only the blobs the server
//! lacks (`HasObjects`, then `PutObjects`), and creates the changeset with
//! the workspace, so the server moves the workspace head in the same
//! transaction. `--amend` creates the replacement without the workspace and
//! moves the head to it with `SetHead`, which supersedes the old head.
//!
//! Reads are guarded against a file that changes underneath: the file is
//! opened without following a symlink, and its stat before and after the
//! read must agree, or it is read again. Paths outside the slice are never
//! read, and no change names one (an amend keeps the old head's changes
//! there as they were).
//!
//! **Crash safety.** Before the changeset is created, the request and the
//! index updates it implies are written to `.graft/pending`. The local
//! state (index, then state) changes only after the server answered, and
//! the pending file goes last. A later command that finds the file re-sends
//! the same request, which is idempotent (the id is content-only), and then
//! applies the local updates; when the request can no longer succeed (its
//! blobs are gone, or the workspace moved elsewhere), the file is dropped
//! and the working copy stays as it was before the commit: a clean roll
//! back, with the changes still showing in `status`.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::fs::{self, File, Metadata as FsMetadata};
use std::io::{ErrorKind, Read};
use std::os::unix::fs::MetadataExt;
use std::path::Path;

use graft_model::{
    Author, AuthorKind, BlobId, Change, ChangesetId, EntryKind, Metadata, ObjectId, ObjectKind,
    RepoPath, Timestamp,
};
use graft_proto::api::{ObjectApi, RepoApi, WorkspaceApi};
use graft_proto::types::{
    CreateChangeset, Diff, GetChangesets, HasObjects, Page, PutObjects, SetHead, WorkspaceRef,
};
use graft_proto::{ApiError, CallContext, Frame};

use super::{
    DIFF_PAGE, FileStatus, GRAFT_DIR, IndexEntry, RealDirs, Stat, WorkingCopy, check_reserved,
    kind_from_name, kind_name, write_atomic,
};
use crate::fetch::{MAX_BATCH, over_budget};
use crate::sparse::{escape, unescape};
use crate::{Error, Result};

/// The pending-commit file in `.graft/`.
const PENDING_FILE: &str = "pending";
const PENDING_HEADER: &str = "graft-pending 1";

/// Metadata keys the server reserves; an amend does not carry them over.
const RESERVED_METADATA: &str = "graft.";

/// Limits on what one commit reads and sends.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CommitLimits {
    /// The largest file, bytes (the engine's default blob limit).
    pub max_blob_bytes: u64,
    /// The most changes in one changeset (the engine's default).
    pub max_changes: usize,
    /// Frame bytes per `PutObjects` call (a larger single blob goes alone).
    pub put_batch_bytes: usize,
    /// Reads of a file that keeps changing before the commit gives up.
    pub read_attempts: u32,
}

impl Default for CommitLimits {
    fn default() -> Self {
        Self {
            max_blob_bytes: 64 << 20,
            max_changes: 100_000,
            put_batch_bytes: 16 << 20,
            read_attempts: 3,
        }
    }
}

/// What to commit.
#[derive(Clone, Debug)]
pub struct CommitOptions {
    /// The message; with `amend`, `None` keeps the old head's.
    pub message: Option<String>,
    /// The author.
    pub author: Author,
    /// The authoring time.
    pub authored_at: Timestamp,
    /// Extra metadata (tool keys such as `vogt.session`).
    pub metadata: Metadata,
    /// Replace the head draft instead of stacking on it.
    pub amend: bool,
    /// Limits.
    pub limits: CommitLimits,
}

/// What a blob upload did.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct UploadStats {
    /// Distinct blobs the commit references from the working copy.
    pub blobs: usize,
    /// Of those, the blobs the server lacked and were sent.
    pub uploaded: usize,
    /// Bytes sent (encoded frames).
    pub bytes: u64,
    /// `PutObjects` calls.
    pub requests: usize,
}

/// A created draft.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Committed {
    /// The new draft, now the workspace head.
    pub id: ChangesetId,
    /// Its first parent.
    pub parent: ChangesetId,
    /// The workspace.
    pub workspace: String,
    /// With `amend`, the draft it replaced (now superseded).
    pub superseded: Option<ChangesetId>,
    /// The changes, sorted by path.
    pub changes: Vec<Change>,
    /// The upload.
    pub upload: UploadStats,
}

/// A commit's request and the local updates it implies, as saved in
/// `.graft/pending` until both sides agree.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Pending {
    pub(crate) workspace: String,
    /// With an amend, the head being replaced.
    pub(crate) amend: Option<ChangesetId>,
    pub(crate) parents: Vec<ChangesetId>,
    pub(crate) changes: Vec<Change>,
    pub(crate) author: Author,
    pub(crate) authored_at: Timestamp,
    pub(crate) message: String,
    pub(crate) metadata: Metadata,
    /// Index entries to set (`Some`) or remove (`None`).
    pub(crate) index: Vec<(RepoPath, Option<IndexEntry>)>,
}

/// One file as read for a commit.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Snap {
    kind: EntryKind,
    id: BlobId,
    stat: Stat,
}

impl WorkingCopy {
    /// Commits the slice's changes as a draft on top of the workspace head,
    /// or, with `amend`, in place of it. [`WorkingCopy::begin`] must have
    /// run first (it opens the workspace and finishes an interrupted
    /// commit).
    pub async fn commit<A: ObjectApi + RepoApi + WorkspaceApi>(
        &mut self,
        api: &A,
        cx: &CallContext,
        opts: &CommitOptions,
    ) -> Result<Committed> {
        let workspace = self.workspace_name()?;
        let statuses = self.status()?;
        let limits = &opts.limits;
        // The working copy's own changes against the snapshot it holds:
        // `Some` for the new content, `None` for a removal.
        let mut wc: BTreeMap<RepoPath, Option<Snap>> = BTreeMap::new();
        let mut refreshed: Vec<(RepoPath, IndexEntry)> = Vec::new();
        for st in &statuses {
            let path = st.path();
            if !self.sparse.includes(path) {
                continue;
            }
            check_reserved(path)?;
            let snap = match st {
                FileStatus::Deleted(_) => None,
                _ => read_snap(&self.root, path, limits)?,
            };
            let indexed = self.index.entries.get(path);
            match (snap, indexed) {
                (None, None) => {}
                (Some(s), Some(e)) if s.kind == e.kind && s.id == e.id => {
                    // Rewritten with the same content: only the stat moved.
                    refreshed.push((
                        path.clone(),
                        IndexEntry {
                            kind: s.kind,
                            id: s.id,
                            stat: s.stat,
                        },
                    ));
                }
                (snap, _) => {
                    wc.insert(path.clone(), snap);
                }
            }
            if wc.len() > limits.max_changes {
                return Err(over_budget("max_changes", limits.max_changes as u64));
            }
        }

        let (parents, changes, message, metadata, amend) = if opts.amend {
            self.amend_request(api, cx, &wc, opts).await?
        } else {
            if wc.is_empty() {
                return Err(Error::NothingToCommit);
            }
            let message = opts
                .message
                .clone()
                .ok_or_else(|| Error::State("a commit needs a message".to_owned()))?;
            let changes = wc
                .iter()
                .map(|(path, snap)| match snap {
                    Some(s) => Change::Put {
                        path: path.clone(),
                        kind: s.kind,
                        id: s.id.0,
                    },
                    None => Change::Delete { path: path.clone() },
                })
                .collect();
            (
                vec![self.snapshot()],
                changes,
                message,
                opts.metadata.clone(),
                None,
            )
        };
        if changes.len() > limits.max_changes {
            return Err(over_budget("max_changes", limits.max_changes as u64));
        }

        let sources: BTreeMap<BlobId, RepoPath> = wc
            .iter()
            .filter_map(|(p, s)| s.map(|s| (s.id, p.clone())))
            .collect();
        let upload = self.upload(api, cx, &sources, limits).await?;

        let mut index: Vec<(RepoPath, Option<IndexEntry>)> = wc
            .iter()
            .map(|(p, s)| {
                (
                    p.clone(),
                    s.map(|s| IndexEntry {
                        kind: s.kind,
                        id: s.id,
                        stat: s.stat,
                    }),
                )
            })
            .collect();
        index.extend(refreshed.into_iter().map(|(p, e)| (p, Some(e))));
        let pending = Pending {
            workspace: workspace.clone(),
            amend,
            parents: parents.clone(),
            changes: changes.clone(),
            author: opts.author.clone(),
            authored_at: opts.authored_at,
            message,
            metadata,
            index,
        };
        self.save_pending(&pending)?;
        let id = match self.send(api, cx, &pending, limits).await {
            Ok(id) => id,
            Err(e) => {
                if !retryable(&e) {
                    self.drop_pending()?;
                }
                return Err(e);
            }
        };
        self.apply_pending(&pending, id)?;
        Ok(Committed {
            id,
            parent: parents[0],
            workspace,
            superseded: amend,
            changes,
            upload,
        })
    }

    /// The workspace this working copy writes through.
    pub(crate) fn workspace_name(&self) -> Result<String> {
        self.config
            .workspace
            .clone()
            .ok_or_else(|| Error::State("no workspace is open".to_owned()))
    }

    /// The request for an amend: the old head's changes composed with the
    /// working copy's, against the old head's parent. Returns the parents,
    /// changes, message, metadata and the head it replaces.
    async fn amend_request<A: RepoApi>(
        &self,
        api: &A,
        cx: &CallContext,
        wc: &BTreeMap<RepoPath, Option<Snap>>,
        opts: &CommitOptions,
    ) -> Result<(
        Vec<ChangesetId>,
        Vec<Change>,
        String,
        Metadata,
        Option<ChangesetId>,
    )> {
        let head = self.state.head.ok_or(Error::NoDraft)?;
        let repo = self.config.repo.clone();
        let got = api
            .get_changesets(
                cx,
                GetChangesets {
                    repo: repo.clone(),
                    ids: vec![head],
                },
            )
            .await?;
        let old = got
            .found
            .into_iter()
            .map(|v| v.changeset)
            .find(|c| c.id() == head)
            .ok_or_else(|| ApiError::NotFound {
                what: format!("changeset {head}"),
            })?;
        let parent = *old
            .parents
            .first()
            .ok_or_else(|| Error::State("the head draft has no parent".to_owned()))?;

        // What the parent holds at each path the old head changed: the
        // reverse diff, which is exactly as long as the old head's changes.
        let mut before: HashMap<RepoPath, Option<(EntryKind, ObjectId)>> = HashMap::new();
        let mut token = None;
        loop {
            let page = api
                .diff(
                    cx,
                    Diff {
                        repo: repo.clone(),
                        from: head,
                        to: parent,
                        paths: Vec::new(),
                        patch: None,
                        page: Page {
                            limit: DIFF_PAGE,
                            token,
                        },
                    },
                )
                .await?;
            // A diff never repeats a path, and an empty page with a token
            // makes no progress. Refusing both means every page adds a new
            // path, so the loop runs at most `old.changes.len() + 1` times,
            // even against a server that keeps handing out tokens.
            if page.files.is_empty() && page.next.is_some() {
                return Err(protocol("the reverse diff sent an empty page with a token"));
            }
            for f in page.files {
                let (path, value) = match f.change {
                    Change::Put { path, kind, id } => (path, Some((kind, id))),
                    Change::Delete { path } => (path, None),
                };
                if before.insert(path, value).is_some() {
                    return Err(protocol("the reverse diff repeats a path"));
                }
            }
            if before.len() > old.changes.len() {
                return Err(protocol("the reverse diff is longer than the changeset"));
            }
            match page.next {
                Some(next) => token = Some(next),
                None => break,
            }
        }
        let head_value = |c: &Change| match c {
            Change::Put { kind, id, .. } => Some((*kind, *id)),
            Change::Delete { .. } => None,
        };
        if before.len() != old.changes.len()
            || old.changes.iter().any(|c| !before.contains_key(c.path()))
        {
            return Err(protocol("the reverse diff does not match the changeset"));
        }

        let mut paths: Vec<RepoPath> = old.changes.iter().map(|c| c.path().clone()).collect();
        paths.extend(wc.keys().cloned());
        paths.sort();
        paths.dedup();
        let old_changes: HashMap<&RepoPath, &Change> =
            old.changes.iter().map(|c| (c.path(), c)).collect();
        let mut changes = Vec::new();
        for path in paths {
            let in_head = old_changes.get(&path).map(|c| head_value(c));
            let parent_value = match in_head {
                Some(_) => before[&path],
                // Untouched by the old head: the parent holds what the head
                // holds, which is the index entry.
                None => self.index.entries.get(&path).map(|e| (e.kind, e.id.0)),
            };
            let final_value = if self.sparse.includes(&path) {
                match wc.get(&path) {
                    Some(snap) => snap.map(|s| (s.kind, s.id.0)),
                    None => match in_head {
                        Some(v) => v,
                        None => self.index.entries.get(&path).map(|e| (e.kind, e.id.0)),
                    },
                }
            } else {
                // Outside the slice: kept exactly as the old head had it.
                in_head.flatten()
            };
            if final_value == parent_value {
                continue;
            }
            changes.push(match final_value {
                Some((kind, id)) => Change::Put { path, kind, id },
                None => Change::Delete { path },
            });
        }
        if changes.is_empty() {
            return Err(Error::NothingToCommit);
        }
        let message = opts.message.clone().unwrap_or(old.message);
        let mut metadata: Metadata = old
            .metadata
            .into_iter()
            .filter(|(k, _)| !k.starts_with(RESERVED_METADATA))
            .collect();
        metadata.extend(opts.metadata.clone());
        Ok((old.parents, changes, message, metadata, Some(head)))
    }

    /// Uploads the blobs in `sources` the server lacks, re-reading each
    /// from its file and checking it still hashes to its id.
    async fn upload<A: ObjectApi>(
        &self,
        api: &A,
        cx: &CallContext,
        sources: &BTreeMap<BlobId, RepoPath>,
        limits: &CommitLimits,
    ) -> Result<UploadStats> {
        let repo = self.config.repo.clone();
        let mut stats = UploadStats {
            blobs: sources.len(),
            ..UploadStats::default()
        };
        let ids: Vec<ObjectId> = sources.keys().map(|b| b.0).collect();
        let mut missing = Vec::new();
        for chunk in ids.chunks(MAX_BATCH) {
            let asked: HashSet<ObjectId> = chunk.iter().copied().collect();
            let lacks = api
                .has_objects(
                    cx,
                    HasObjects {
                        repo: repo.clone(),
                        ids: chunk.to_vec(),
                    },
                )
                .await?;
            // Only ids that were asked about, each once.
            let mut seen = HashSet::new();
            missing.extend(
                lacks
                    .into_iter()
                    .filter(|id| asked.contains(id) && seen.insert(*id)),
            );
        }
        let send: Vec<(BlobId, &RepoPath)> = missing
            .into_iter()
            .map(|id| (BlobId(id), &sources[&BlobId(id)]))
            .collect();
        self.put_blobs(api, cx, &send, limits, &mut stats).await?;
        Ok(stats)
    }

    /// Sends the blobs, each re-read from its file, in batches.
    async fn put_blobs<A: ObjectApi>(
        &self,
        api: &A,
        cx: &CallContext,
        blobs: &[(BlobId, &RepoPath)],
        limits: &CommitLimits,
        stats: &mut UploadStats,
    ) -> Result<()> {
        let mut batch: Vec<Frame> = Vec::new();
        let mut size = 0usize;
        for (id, path) in blobs {
            let data = read_blob(&self.root, path, *id, limits)?;
            let frame = Frame::encode(ObjectKind::Blob, &data);
            let len = frame.body.len();
            if !batch.is_empty()
                && (size + len > limits.put_batch_bytes || batch.len() >= MAX_BATCH)
            {
                self.put_batch(api, cx, std::mem::take(&mut batch), stats)
                    .await?;
                size = 0;
            }
            size += len;
            batch.push(frame);
        }
        if !batch.is_empty() {
            self.put_batch(api, cx, batch, stats).await?;
        }
        Ok(())
    }

    async fn put_batch<A: ObjectApi>(
        &self,
        api: &A,
        cx: &CallContext,
        blobs: Vec<Frame>,
        stats: &mut UploadStats,
    ) -> Result<()> {
        stats.uploaded += blobs.len();
        stats.bytes += blobs.iter().map(|f| f.body.len() as u64).sum::<u64>();
        stats.requests += 1;
        api.put_objects(
            cx,
            PutObjects {
                repo: self.config.repo.clone(),
                blobs,
            },
        )
        .await?;
        Ok(())
    }

    /// Sends a pending commit: creates the changeset (re-uploading blobs
    /// the server reports missing, when the files still hold them) and,
    /// for an amend, moves the head to it. Idempotent.
    pub(crate) async fn send<A: ObjectApi + RepoApi + WorkspaceApi>(
        &self,
        api: &A,
        cx: &CallContext,
        p: &Pending,
        limits: &CommitLimits,
    ) -> Result<ChangesetId> {
        let req = CreateChangeset {
            repo: self.config.repo.clone(),
            parents: p.parents.clone(),
            changes: p.changes.clone(),
            author: p.author.clone(),
            authored_at: p.authored_at,
            message: p.message.clone(),
            metadata: p.metadata.clone(),
            inline_blobs: Vec::new(),
            // An amend's parent is not the head, so the head moves by
            // `SetHead` instead.
            workspace: p.amend.is_none().then(|| p.workspace.clone()),
        };
        let mut reuploads = 0;
        let created = loop {
            match api.create_changeset(cx, req.clone()).await {
                Ok(c) => break c,
                // A blob the server lost (collected between the check and
                // the create): send it again, once, if the file still has it.
                Err(ApiError::MissingObjects { ids }) if reuploads == 0 => {
                    reuploads += 1;
                    let sources: HashMap<BlobId, &RepoPath> = p
                        .index
                        .iter()
                        .filter_map(|(path, e)| e.as_ref().map(|e| (e.id, path)))
                        .collect();
                    let mut send = Vec::new();
                    for id in ids {
                        match sources.get(&BlobId(id)) {
                            Some(path) => send.push((BlobId(id), *path)),
                            None => return Err(ApiError::MissingObjects { ids: vec![id] }.into()),
                        }
                    }
                    let mut stats = UploadStats::default();
                    self.put_blobs(api, cx, &send, limits, &mut stats).await?;
                }
                Err(ApiError::RefConflict { actual, .. }) => {
                    return Err(Error::Diverged {
                        workspace: p.workspace.clone(),
                        server: actual,
                        local: self.state.head,
                    });
                }
                Err(e) => return Err(e.into()),
            }
        };
        let id = created.id;
        self.check_created(api, cx, p, id).await?;
        if let Some(old) = p.amend {
            let moved = api
                .set_head(
                    cx,
                    SetHead {
                        workspace: WorkspaceRef {
                            repo: self.config.repo.clone(),
                            name: p.workspace.clone(),
                        },
                        expected: Some(old),
                        new: Some(id),
                    },
                )
                .await;
            match moved {
                Ok(_) => {}
                // A retry after the move succeeded.
                Err(ApiError::RefConflict { actual, .. }) if actual == Some(id) => {}
                Err(ApiError::RefConflict { actual, .. }) => {
                    return Err(Error::Diverged {
                        workspace: p.workspace.clone(),
                        server: actual,
                        local: Some(old),
                    });
                }
                Err(e) => return Err(e.into()),
            }
        }
        Ok(id)
    }

    /// Refuses an answer that names a changeset other than the one sent:
    /// the server must hold `id`, and its content must be the request's.
    async fn check_created<A: RepoApi>(
        &self,
        api: &A,
        cx: &CallContext,
        p: &Pending,
        id: ChangesetId,
    ) -> Result<()> {
        let got = api
            .get_changesets(
                cx,
                GetChangesets {
                    repo: self.config.repo.clone(),
                    ids: vec![id],
                },
            )
            .await?;
        let ours = got.found.iter().any(|v| {
            let c = &v.changeset;
            c.id() == id
                && c.parents == p.parents
                && c.changes == p.changes
                && c.author == p.author
                && c.authored_at == p.authored_at
                && c.message == p.message
                && c.metadata == p.metadata
        });
        if !ours {
            return Err(protocol("the created changeset is not the one sent"));
        }
        Ok(())
    }

    /// Applies a sent commit locally: the index, then the head, then the
    /// pending file goes.
    pub(crate) fn apply_pending(&mut self, p: &Pending, id: ChangesetId) -> Result<()> {
        for (path, entry) in &p.index {
            match entry {
                Some(e) => {
                    self.index.entries.insert(path.clone(), e.clone());
                }
                None => {
                    self.index.entries.remove(path);
                }
            }
        }
        self.save_index()?;
        self.state.head = Some(id);
        self.save_state()?;
        self.drop_pending()
    }

    fn pending_path(&self) -> std::path::PathBuf {
        self.root.join(GRAFT_DIR).join(PENDING_FILE)
    }

    pub(crate) fn save_pending(&self, p: &Pending) -> Result<()> {
        write_atomic(&self.pending_path(), pending_text(p).as_bytes())?;
        Ok(())
    }

    /// The interrupted commit, if there is one.
    pub(crate) fn load_pending(&self) -> Result<Option<Pending>> {
        match fs::read_to_string(self.pending_path()) {
            Ok(text) => parse_pending(&text).map(Some),
            Err(e) if e.kind() == ErrorKind::NotFound => Ok(None),
            Err(e) => Err(e.into()),
        }
    }

    pub(crate) fn drop_pending(&self) -> Result<()> {
        match fs::remove_file(self.pending_path()) {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == ErrorKind::NotFound => Ok(()),
            Err(e) => Err(e.into()),
        }
    }
}

/// Whether a failed send may succeed if sent again unchanged (the pending
/// file is kept for the next command), as opposed to a request that can
/// never succeed (the file is dropped, which rolls the commit back).
pub(crate) fn retryable(e: &Error) -> bool {
    match e {
        Error::Api(ApiError::Unavailable { .. } | ApiError::LeaseExpired { .. }) => true,
        Error::Api(_) | Error::Diverged { .. } | Error::Unstable(_) => false,
        _ => true,
    }
}

/// A server answer that breaks the protocol.
fn protocol(detail: &str) -> Error {
    Error::Api(ApiError::Rejected {
        policy: "protocol".to_owned(),
        detail: detail.to_owned(),
    })
}

fn same_file(a: &FsMetadata, b: &FsMetadata) -> bool {
    a.ino() == b.ino() && a.dev() == b.dev()
}

/// Reads one working file for a commit: its kind, blob id and stat, or
/// `None` when it is gone (or a parent is no longer a real directory).
/// Never follows a symlink, never blocks on a FIFO, and retries a file whose
/// stat changes during the read.
fn read_snap(root: &Path, path: &RepoPath, limits: &CommitLimits) -> Result<Option<Snap>> {
    Ok(read_file(root, path, limits)?.map(|(snap, _)| snap))
}

/// [`read_snap`] with the content.
fn read_file(
    root: &Path,
    path: &RepoPath,
    limits: &CommitLimits,
) -> Result<Option<(Snap, Vec<u8>)>> {
    let full = root.join(path.as_str());
    for _ in 0..limits.read_attempts.max(1) {
        if !RealDirs::default().parent_is_real(root, path)? {
            return Ok(None);
        }
        let before = match fs::symlink_metadata(&full) {
            Ok(m) => m,
            Err(e) if e.kind() == ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(e.into()),
        };
        let ft = before.file_type();
        if ft.is_dir() {
            return Ok(None);
        }
        if ft.is_symlink() {
            let target = match fs::read_link(&full) {
                Ok(t) => t,
                Err(e) if e.kind() == ErrorKind::NotFound => return Ok(None),
                Err(e) if e.kind() == ErrorKind::InvalidInput => continue,
                Err(e) => return Err(e.into()),
            };
            let after = fs::symlink_metadata(&full)?;
            if !same_file(&before, &after) || Stat::of(&before) != Stat::of(&after) {
                continue;
            }
            use std::os::unix::ffi::OsStrExt;
            let data = target.as_os_str().as_bytes().to_vec();
            let snap = Snap {
                kind: EntryKind::Symlink,
                id: BlobId::hash(&data),
                stat: Stat::of(&after),
            };
            return Ok(Some((snap, data)));
        }
        if !ft.is_file() {
            return Err(Error::Api(ApiError::InvalidPath {
                path: path.as_str().to_owned(),
                reason: "not a regular file or symlink".to_owned(),
            }));
        }
        if before.len() > limits.max_blob_bytes {
            return Err(too_big(limits));
        }
        let mut file = match open_nofollow(&full) {
            Ok(f) => f,
            Err(e) if e.kind() == ErrorKind::NotFound => return Ok(None),
            // Replaced by a symlink after the stat (ELOOP): look again.
            Err(_) => continue,
        };
        let m1 = file.metadata()?;
        if !m1.is_file() || !same_file(&before, &m1) {
            continue;
        }
        let mut data = Vec::with_capacity(m1.len().min(limits.max_blob_bytes) as usize);
        (&mut file)
            .take(limits.max_blob_bytes + 1)
            .read_to_end(&mut data)?;
        if data.len() as u64 > limits.max_blob_bytes {
            return Err(too_big(limits));
        }
        let m2 = file.metadata()?;
        if Stat::of(&m1) != Stat::of(&m2) || data.len() as u64 != m2.len() {
            continue;
        }
        let kind = if m2.mode() & 0o100 != 0 {
            EntryKind::Exec
        } else {
            EntryKind::File
        };
        let snap = Snap {
            kind,
            id: BlobId::hash(&data),
            stat: Stat::of(&m2),
        };
        return Ok(Some((snap, data)));
    }
    Err(Error::Unstable(path.clone()))
}

/// The content of `path` for upload, which must still hash to `id`.
fn read_blob(root: &Path, path: &RepoPath, id: BlobId, limits: &CommitLimits) -> Result<Vec<u8>> {
    match read_file(root, path, limits)? {
        Some((snap, data)) if snap.id == id => Ok(data),
        _ => Err(Error::Unstable(path.clone())),
    }
}

fn too_big(limits: &CommitLimits) -> Error {
    over_budget("max_blob_bytes", limits.max_blob_bytes)
}

/// Opens a file for reading without following a final symlink and without
/// blocking on a FIFO or device.
fn open_nofollow(path: &Path) -> std::io::Result<File> {
    use rustix::fs::{Mode, OFlags};
    let fd = rustix::fs::open(
        path,
        OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::NONBLOCK | OFlags::CLOEXEC,
        Mode::empty(),
    )?;
    Ok(File::from(fd))
}

fn author_kind_name(k: AuthorKind) -> &'static str {
    match k {
        AuthorKind::Human => "human",
        AuthorKind::Agent => "agent",
        AuthorKind::Bot => "bot",
        AuthorKind::Ci => "ci",
    }
}

fn author_kind_from(s: &str) -> Option<AuthorKind> {
    Some(match s {
        "human" => AuthorKind::Human,
        "agent" => AuthorKind::Agent,
        "bot" => AuthorKind::Bot,
        "ci" => AuthorKind::Ci,
        _ => return None,
    })
}

/// `.graft/pending`: one record per line, `key value`, values escaped as
/// in the sparse file.
fn pending_text(p: &Pending) -> String {
    let mut s = format!("{PENDING_HEADER}\nworkspace {}\n", escape(&p.workspace));
    if let Some(a) = p.amend {
        s.push_str(&format!("amend {a}\n"));
    }
    for parent in &p.parents {
        s.push_str(&format!("parent {parent}\n"));
    }
    s.push_str(&format!("author-name {}\n", escape(&p.author.name)));
    s.push_str(&format!("author-email {}\n", escape(&p.author.email)));
    s.push_str(&format!(
        "author-kind {}\n",
        author_kind_name(p.author.kind)
    ));
    s.push_str(&format!(
        "time {} {}\n",
        p.authored_at.ms, p.authored_at.offset_min
    ));
    s.push_str(&format!("message {}\n", escape(&p.message)));
    for (k, v) in &p.metadata {
        s.push_str(&format!(
            "meta-key {}\nmeta-value {}\n",
            escape(k),
            escape(v)
        ));
    }
    for c in &p.changes {
        match c {
            Change::Put { path, kind, id } => s.push_str(&format!(
                "put {} {} {}\n",
                kind_name(*kind),
                id.to_hex(),
                escape(path.as_str())
            )),
            Change::Delete { path } => s.push_str(&format!("del {}\n", escape(path.as_str()))),
        }
    }
    for (path, e) in &p.index {
        match e {
            Some(e) => {
                let st = &e.stat;
                s.push_str(&format!(
                    "index {} {} {} {} {} {} {} {}\n",
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
            None => s.push_str(&format!("unindex {}\n", escape(path.as_str()))),
        }
    }
    s.push_str("end\n");
    s
}

fn parse_pending(text: &str) -> Result<Pending> {
    let bad = || Error::State("malformed pending commit".to_owned());
    let mut lines = text.lines();
    if lines.next() != Some(PENDING_HEADER) {
        return Err(Error::State("unknown pending commit version".to_owned()));
    }
    let text_of = |v: &str| unescape(v).ok_or_else(bad);
    let path_of = |v: &str| {
        unescape(v)
            .and_then(|p| RepoPath::new(&p).ok())
            .ok_or_else(bad)
    };
    let id_of = |v: &str| ChangesetId::parse(v).map_err(|_| bad());
    let mut workspace = None;
    let mut amend = None;
    let mut parents = Vec::new();
    let (mut name, mut email, mut kind) = (None, None, None);
    let mut time = None;
    let mut message = None;
    let mut metadata = Metadata::new();
    let mut key: Option<String> = None;
    let mut changes = Vec::new();
    let mut index = Vec::new();
    let mut ended = false;
    for line in lines {
        let (k, v) = line.split_once(' ').unwrap_or((line, ""));
        match k {
            "workspace" => workspace = Some(text_of(v)?),
            "amend" => amend = Some(id_of(v)?),
            "parent" => parents.push(id_of(v)?),
            "author-name" => name = Some(text_of(v)?),
            "author-email" => email = Some(text_of(v)?),
            "author-kind" => kind = Some(author_kind_from(v).ok_or_else(bad)?),
            "time" => {
                let (ms, off) = v.split_once(' ').ok_or_else(bad)?;
                time = Some(Timestamp {
                    ms: ms.parse().map_err(|_| bad())?,
                    offset_min: off.parse().map_err(|_| bad())?,
                });
            }
            "message" => message = Some(text_of(v)?),
            "meta-key" => key = Some(text_of(v)?),
            "meta-value" => {
                let k = key.take().ok_or_else(bad)?;
                metadata.insert(k, text_of(v)?);
            }
            "put" => {
                let f: Vec<&str> = v.splitn(3, ' ').collect();
                if f.len() != 3 {
                    return Err(bad());
                }
                changes.push(Change::Put {
                    kind: kind_from_name(f[0]).ok_or_else(bad)?,
                    id: ObjectId::from_hex(f[1]).map_err(|_| bad())?,
                    path: path_of(f[2])?,
                });
            }
            "del" => changes.push(Change::Delete { path: path_of(v)? }),
            "index" => {
                let f: Vec<&str> = v.splitn(8, ' ').collect();
                if f.len() != 8 {
                    return Err(bad());
                }
                let num = |s: &str| s.parse::<i64>().map_err(|_| bad());
                let unum = |s: &str| s.parse::<u64>().map_err(|_| bad());
                index.push((
                    path_of(f[7])?,
                    Some(IndexEntry {
                        kind: kind_from_name(f[0]).ok_or_else(bad)?,
                        id: BlobId(ObjectId::from_hex(f[1]).map_err(|_| bad())?),
                        stat: Stat {
                            size: unum(f[2])?,
                            mtime_ns: num(f[3])?,
                            ctime_ns: num(f[4])?,
                            ino: unum(f[5])?,
                            mode: f[6].parse().map_err(|_| bad())?,
                        },
                    }),
                ));
            }
            "unindex" => index.push((path_of(v)?, None)),
            "end" => ended = true,
            _ => return Err(bad()),
        }
    }
    // A file cut short is not a commit to finish.
    if !ended || parents.is_empty() {
        return Err(bad());
    }
    Ok(Pending {
        workspace: workspace.ok_or_else(bad)?,
        amend,
        parents,
        changes,
        author: Author {
            name: name.ok_or_else(bad)?,
            email: email.ok_or_else(bad)?,
            kind: kind.ok_or_else(bad)?,
        },
        authored_at: time.ok_or_else(bad)?,
        message: message.ok_or_else(bad)?,
        metadata,
        index,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn p(s: &str) -> RepoPath {
        RepoPath::new(s).unwrap()
    }

    #[test]
    fn pending_round_trips() {
        let pending = Pending {
            workspace: "w-1".into(),
            amend: Some(ChangesetId::hash(b"old")),
            parents: vec![ChangesetId::hash(b"parent")],
            changes: vec![
                Change::Put {
                    path: p("a b/c%d"),
                    kind: EntryKind::Exec,
                    id: BlobId::hash(b"x").0,
                },
                Change::Delete { path: p("gone") },
            ],
            author: Author {
                name: "A Person".into(),
                email: "a@example.com".into(),
                kind: AuthorKind::Agent,
            },
            authored_at: Timestamp {
                ms: 1_700_000_000_000,
                offset_min: -300,
            },
            message: "subject\n\nbody with % and \r".into(),
            metadata: [("vogt.session".to_owned(), "s 1".to_owned())].into(),
            index: vec![
                (
                    p("a b/c%d"),
                    Some(IndexEntry {
                        kind: EntryKind::Exec,
                        id: BlobId::hash(b"x"),
                        stat: Stat {
                            size: 1,
                            mtime_ns: 2,
                            ctime_ns: 3,
                            ino: 4,
                            mode: 0o100755,
                        },
                    }),
                ),
                (p("gone"), None),
            ],
        };
        let text = pending_text(&pending);
        assert_eq!(parse_pending(&text).unwrap(), pending);
        // Cut short anywhere before the end marker: refused, not half read.
        let cut = &text[..text.len() - "end\n".len()];
        assert!(parse_pending(cut).is_err());
    }

    #[test]
    fn reads_refuse_special_files_and_huge_files_and_follow_no_symlink() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let limits = CommitLimits {
            max_blob_bytes: 8,
            ..CommitLimits::default()
        };
        fs::write(root.join("small"), b"1234").unwrap();
        fs::write(root.join("big"), b"123456789").unwrap();
        std::os::unix::fs::symlink("/etc/passwd", root.join("link")).unwrap();
        rustix::fs::mkfifoat(rustix::fs::CWD, root.join("fifo"), rustix::fs::Mode::RUSR).unwrap();

        let s = read_snap(root, &p("small"), &limits).unwrap().unwrap();
        assert_eq!((s.kind, s.id), (EntryKind::File, BlobId::hash(b"1234")));
        assert!(matches!(
            read_snap(root, &p("big"), &limits),
            Err(Error::Api(ApiError::LimitExceeded { .. }))
        ));
        let l = read_snap(root, &p("link"), &limits).unwrap().unwrap();
        assert_eq!(
            (l.kind, l.id),
            (EntryKind::Symlink, BlobId::hash(b"/etc/passwd"))
        );
        assert!(matches!(
            read_snap(root, &p("fifo"), &limits),
            Err(Error::Api(ApiError::InvalidPath { .. }))
        ));
        assert_eq!(read_snap(root, &p("absent"), &limits).unwrap(), None);
        // A file under a symlinked directory is not read through it.
        std::os::unix::fs::symlink("/etc", root.join("etc")).unwrap();
        assert_eq!(read_snap(root, &p("etc/passwd"), &limits).unwrap(), None);
    }

    #[test]
    fn a_blob_that_no_longer_hashes_to_its_id_is_not_sent() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("f"), b"new").unwrap();
        let limits = CommitLimits::default();
        assert!(matches!(
            read_blob(dir.path(), &p("f"), BlobId::hash(b"old"), &limits),
            Err(Error::Unstable(_))
        ));
        assert_eq!(
            read_blob(dir.path(), &p("f"), BlobId::hash(b"new"), &limits).unwrap(),
            b"new"
        );
    }
}
