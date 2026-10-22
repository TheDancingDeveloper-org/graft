//! Import a git repository into graft, verifying fidelity per commit
//! (`docs/design.md` §11.1).
//!
//! 1. Resolve the configured refs and walk every commit reachable from them,
//!    oldest first, stopping at commits the mirror map already holds.
//! 2. Check, before writing anything, every path each commit introduces
//!    (decision D11) and that every commit is representable. Any failure stops
//!    the import with the full list.
//! 3. For each commit, diff its tree against its first parent's, walking only
//!    subtrees whose ids differ; read and hash new blobs; derive the root by
//!    path copying (as the server will); learn the raw trees the export needs;
//!    and verify that the export reproduces the commit's sha.
//! 4. Upload blobs, then send `ImportChangesets` batches, then record the
//!    batch in the mirror map. Objects are stored before anything names them.
//!
//! Setting graft refs from the imported tips is the mirror's job (G1.12);
//! the report carries the tips.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::PathBuf;
use std::sync::Mutex;

use graft_model::{
    ApplyOptions, Author, AuthorKind, BlobId, Change, Changeset, ChangesetId, CheckOptions,
    EntryKind, Metadata, ObjectId, ObjectKind, ObjectSource, RepoPath, SourceError, Timestamp,
    Tree, TreeEntry, TreeId, apply_changes_with,
};
use graft_proto::api::{AdminApi, ObjectApi, RepoApi};
use graft_proto::types::{GetChangesets, GetObjects, HasObjects, ImportChangesets, PutObjects};
use graft_proto::{CallContext, Frame};

use crate::diff::{DirDiff, changed_dirs, check_paths, git_changes};
use crate::export::{GitSource, commit_sha, export_tree, learn_raw_trees, tree_sha};
use crate::git::{Kind, RawCommit, Repo, author, parse_commit, submodule_id};
use crate::map::{ImportedCommit, MirrorMap};
use crate::walk::topo;
use crate::{BadPath, GitError, GitSha, Unrepresentable};

/// Blobs per `PutObjects` call.
const PUT_BLOBS_MAX: usize = 256;
/// Bytes of blobs held before they are uploaded.
const PUT_BYTES_MAX: usize = 16 * 1024 * 1024;
/// Ids per `HasObjects` call.
const HAS_OBJECTS_MAX: usize = 1024;

/// What to import.
#[derive(Clone, Debug)]
pub struct ImportConfig {
    /// A local git repository (bare, normally).
    pub git_dir: PathBuf,
    /// The graft repository.
    pub repo: String,
    /// Git refs to import, mapped to graft lines (`refs/heads/main` → `main`).
    pub lines: Vec<(String, String)>,
    /// Import every tag under `refs/tags/` that names a commit.
    pub tags: bool,
    /// Commits allowed to fail the fidelity check (`--allow-lossy`).
    pub allow_lossy: Vec<GitSha>,
    /// Changesets per `ImportChangesets` batch.
    pub batch: usize,
}

impl ImportConfig {
    /// Imports `refs/heads/main` as line `main`, with tags, in batches of 256.
    pub fn new(git_dir: impl Into<PathBuf>, repo: impl Into<String>) -> Self {
        ImportConfig {
            git_dir: git_dir.into(),
            repo: repo.into(),
            lines: vec![("refs/heads/main".to_owned(), "main".to_owned())],
            tags: true,
            allow_lossy: Vec::new(),
            batch: 256,
        }
    }
}

/// What an import did.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ImportReport {
    /// Commits imported by this run.
    pub commits: u64,
    /// Blobs uploaded by this run.
    pub blobs: u64,
    /// Commits whose re-export reproduced their sha.
    pub verified: u64,
    /// Commits that failed the fidelity check but were allowed to.
    pub lossy: Vec<GitSha>,
    /// Raw trees recorded in the mirror map.
    pub raw_trees: u64,
    /// Each configured line and the changeset its ref names.
    pub lines: Vec<(String, ChangesetId)>,
    /// Each imported tag (name without `refs/tags/`) and its changeset.
    pub tags: Vec<(String, ChangesetId)>,
}

/// Imports everything reachable from the configured refs that is not yet in
/// `map`, oldest first. See the module documentation for the steps.
pub async fn import<A, M>(
    api: &A,
    cx: &CallContext,
    map: &M,
    config: &ImportConfig,
) -> Result<ImportReport, GitError>
where
    A: AdminApi + ObjectApi + RepoApi,
    M: MirrorMap,
{
    let repo = Repo::open(&config.git_dir)?;
    let mut line_tips = Vec::new();
    for (git_ref, line) in &config.lines {
        let sha = repo
            .resolve(git_ref)?
            .ok_or_else(|| GitError::Read(format!("{git_ref} does not name a commit")))?;
        line_tips.push((line.clone(), sha));
    }
    let mut tag_tips = Vec::new();
    if config.tags {
        for name in repo.refs_under("refs/tags/")? {
            if let Some(sha) = repo.resolve(&name)? {
                tag_tips.push((name["refs/tags/".len()..].to_owned(), sha));
            }
        }
    }
    let tips: Vec<GitSha> = line_tips.iter().chain(&tag_tips).map(|(_, s)| *s).collect();
    let walk = topo(&repo, &tips, |sha| Ok(map.changeset_of(sha)?.is_some()))?;
    let mut importer = Importer::new(api, cx, map, config, repo);
    importer.precheck(&walk.order, walk.misshapen)?;
    for sha in &walk.order {
        importer.import_commit(*sha).await?;
        if importer.pending.len() >= config.batch.max(1) {
            importer.flush().await?;
        }
    }
    importer.flush().await?;
    if !importer.indistinct.is_empty() {
        return Err(GitError::Indistinct(importer.indistinct));
    }
    if !importer.failed.is_empty() {
        return Err(GitError::Fidelity(importer.failed));
    }
    let mut report = importer.report;
    for (line, sha) in line_tips {
        report.lines.push((line, importer.ids.changeset(map, sha)?));
    }
    for (tag, sha) in tag_tips {
        report.tags.push((tag, importer.ids.changeset(map, sha)?));
    }
    Ok(report)
}

/// Changeset ids of this run's commits, falling back to the mirror map.
#[derive(Default)]
struct Ids {
    this_run: HashMap<GitSha, ChangesetId>,
    shas: HashMap<ChangesetId, GitSha>,
}

impl Ids {
    fn changeset<M: MirrorMap>(&self, map: &M, sha: GitSha) -> Result<ChangesetId, GitError> {
        match self.this_run.get(&sha) {
            Some(id) => Ok(*id),
            None => map
                .changeset_of(sha)?
                .ok_or_else(|| GitError::Map(format!("commit {sha} is not imported"))),
        }
    }

    fn insert(&mut self, sha: GitSha, id: ChangesetId) {
        self.this_run.insert(sha, id);
        self.shas.insert(id, sha);
    }

    fn sha_if_known<M: MirrorMap>(
        &self,
        map: &M,
        id: ChangesetId,
    ) -> Result<Option<GitSha>, GitError> {
        match self.shas.get(&id) {
            Some(sha) => Ok(Some(*sha)),
            None => map.sha_of(id),
        }
    }

    fn sha<M: MirrorMap>(&self, map: &M, id: ChangesetId) -> Result<GitSha, GitError> {
        if let Some(sha) = self.shas.get(&id) {
            return Ok(*sha);
        }
        map.sha_of(id)?
            .ok_or_else(|| GitError::Map(format!("changeset {id} has no git sha")))
    }
}

struct Importer<'a, A, M> {
    api: &'a A,
    cx: &'a CallContext,
    map: &'a M,
    config: &'a ImportConfig,
    repo: Repo,
    /// Git root tree of each commit read so far.
    commit_trees: HashMap<GitSha, GitSha>,
    /// Graft root of each commit imported or verified so far.
    roots: HashMap<GitSha, TreeId>,
    ids: Ids,
    /// git blob sha → graft blob id, and back.
    blob_ids: HashMap<GitSha, BlobId>,
    blob_shas: HashMap<BlobId, GitSha>,
    /// Graft trees derived (or fetched) so far.
    trees: Mutex<HashMap<TreeId, Tree>>,
    /// git tree → graft tree (`None`: no entries), for checking raw trees.
    converted: HashMap<GitSha, Option<TreeId>>,
    uploaded: HashSet<BlobId>,
    blob_queue: Vec<(BlobId, Vec<u8>)>,
    blob_queue_bytes: usize,
    new_blob_shas: Vec<(BlobId, GitSha)>,
    pending: Vec<(Changeset, ImportedCommit)>,
    failed: Vec<GitSha>,
    indistinct: Vec<(GitSha, GitSha)>,
    report: ImportReport,
}

/// Graft trees for [`apply_changes_with`]: those this import derived, then
/// the server's.
struct Trees<'a, A> {
    cache: &'a Mutex<HashMap<TreeId, Tree>>,
    api: &'a A,
    cx: &'a CallContext,
    repo: &'a str,
}

impl<A: ObjectApi> ObjectSource for Trees<'_, A> {
    async fn tree(&self, id: TreeId) -> Result<Tree, SourceError> {
        if let Some(t) = self.cache.lock().expect("tree cache").get(&id) {
            return Ok(t.clone());
        }
        let got = self
            .api
            .get_objects(
                self.cx,
                GetObjects {
                    repo: self.repo.to_owned(),
                    ids: vec![id.0],
                },
            )
            .await
            .map_err(|e| SourceError::Backend(format!("{e:?}")))?;
        let (_, frame) = got
            .found
            .into_iter()
            .find(|(i, f)| *i == id.0 && f.kind == ObjectKind::Tree)
            .ok_or(SourceError::Missing(id))?;
        let payload = frame
            .decode_verified(id.0)
            .map_err(|_| SourceError::Corrupt(id))?;
        let tree = Tree::decode(&payload).map_err(|_| SourceError::Malformed(id))?;
        self.cache
            .lock()
            .expect("tree cache")
            .insert(id, tree.clone());
        Ok(tree)
    }
}

/// The export's view of this import: git trees from the repository, blob
/// shas from what the import hashed.
struct Src<'a> {
    repo: &'a Repo,
    blob_shas: &'a HashMap<BlobId, GitSha>,
    map: &'a dyn MirrorMap,
}

impl GitSource for Src<'_> {
    fn raw_tree(&mut self, sha: GitSha) -> Result<Vec<u8>, GitError> {
        self.repo.read(sha, Kind::Tree)
    }

    fn blob_sha(&mut self, id: BlobId) -> Result<GitSha, GitError> {
        match self.blob_shas.get(&id) {
            Some(sha) => Ok(*sha),
            None => self
                .map
                .blob_sha(id)?
                .ok_or_else(|| GitError::Map(format!("no git sha for blob {id}"))),
        }
    }
}

fn empty_root() -> TreeId {
    Tree::default().id()
}

fn dir_path(d: &DirDiff) -> Result<String, GitError> {
    String::from_utf8(d.path.clone()).map_err(|_| GitError::Read("unchecked path".into()))
}

impl<'a, A, M> Importer<'a, A, M>
where
    A: AdminApi + ObjectApi + RepoApi,
    M: MirrorMap,
{
    fn new(
        api: &'a A,
        cx: &'a CallContext,
        map: &'a M,
        config: &'a ImportConfig,
        repo: Repo,
    ) -> Self {
        Importer {
            api,
            cx,
            map,
            config,
            repo,
            commit_trees: HashMap::new(),
            roots: HashMap::new(),
            ids: Ids::default(),
            blob_ids: HashMap::new(),
            blob_shas: HashMap::new(),
            trees: Mutex::new(HashMap::new()),
            converted: HashMap::new(),
            uploaded: HashSet::new(),
            blob_queue: Vec::new(),
            blob_queue_bytes: 0,
            new_blob_shas: Vec::new(),
            pending: Vec::new(),
            failed: Vec::new(),
            indistinct: Vec::new(),
            report: ImportReport::default(),
        }
    }

    fn commit(&mut self, sha: GitSha) -> Result<RawCommit, GitError> {
        let raw = parse_commit(&self.repo.read(sha, Kind::Commit)?)
            .ok_or_else(|| GitError::Read(format!("commit {sha} has an unexpected shape")))?;
        self.commit_trees.insert(sha, raw.tree);
        Ok(raw)
    }

    fn tree_of(&mut self, sha: GitSha) -> Result<GitSha, GitError> {
        match self.commit_trees.get(&sha) {
            Some(t) => Ok(*t),
            None => Ok(self.commit(sha)?.tree),
        }
    }

    /// Step 2: every path rule and representability check, before any write.
    fn precheck(&mut self, order: &[GitSha], misshapen: Vec<GitSha>) -> Result<(), GitError> {
        let mut bad: BTreeMap<(Vec<u8>, String), GitSha> = BTreeMap::new();
        let skip: HashSet<GitSha> = misshapen.iter().copied().collect();
        let mut unrepresentable: Vec<Unrepresentable> = misshapen
            .into_iter()
            .map(|commit| Unrepresentable {
                commit,
                reason: "commit-shape",
            })
            .collect();
        for &sha in order {
            if skip.contains(&sha) {
                continue;
            }
            let raw = self.commit(sha)?;
            if raw.parents.first().is_some_and(|p| skip.contains(p)) {
                continue;
            }
            let parent_tree = raw.parents.first().map(|p| self.tree_of(*p)).transpose()?;
            let dirs = changed_dirs(&mut self.repo, parent_tree, Some(raw.tree))?;
            let offences = check_paths(&mut self.repo, &dirs)?;
            if offences.is_empty() {
                if raw.parents.len() < 2 && git_changes(&mut self.repo, &dirs)?.is_empty() {
                    unrepresentable.push(Unrepresentable {
                        commit: sha,
                        reason: "empty-changeset",
                    });
                }
            } else {
                for o in offences {
                    bad.entry(o).or_insert(sha);
                }
            }
        }
        if !bad.is_empty() {
            let mut list: Vec<BadPath> = bad
                .into_iter()
                .map(|((path, reason), commit)| BadPath {
                    path: String::from_utf8_lossy(&path).into_owned(),
                    reason,
                    commit,
                })
                .collect();
            list.sort();
            return Err(GitError::BadPaths(list));
        }
        if !unrepresentable.is_empty() {
            return Err(GitError::Unrepresentable(unrepresentable));
        }
        Ok(())
    }

    /// The graft id of a git blob, reading and hashing it the first time and
    /// queueing it for upload.
    fn blob_id(&mut self, sha: GitSha) -> Result<BlobId, GitError> {
        if let Some(id) = self.blob_ids.get(&sha) {
            return Ok(*id);
        }
        let bytes = self.repo.read(sha, Kind::Blob)?;
        let id = BlobId::hash(&bytes);
        self.blob_ids.insert(sha, id);
        self.blob_shas.insert(id, sha);
        self.new_blob_shas.push((id, sha));
        if self.uploaded.insert(id) {
            self.blob_queue_bytes += bytes.len();
            self.blob_queue.push((id, bytes));
        }
        Ok(id)
    }

    /// The graft root of an imported commit: this run's, else the server's.
    async fn root_of(&mut self, sha: GitSha) -> Result<TreeId, GitError> {
        if let Some(r) = self.roots.get(&sha) {
            return Ok(*r);
        }
        let id = self.ids.changeset(self.map, sha)?;
        let got = self
            .api
            .get_changesets(
                self.cx,
                GetChangesets {
                    repo: self.config.repo.clone(),
                    ids: vec![id],
                },
            )
            .await?;
        let root = got
            .found
            .into_iter()
            .find(|v| v.changeset.id() == id)
            .map(|v| v.changeset.root)
            .ok_or_else(|| GitError::Map(format!("changeset {id} is not on the server")))?;
        self.roots.insert(sha, root);
        Ok(root)
    }

    /// Step 3 for one commit.
    async fn import_commit(&mut self, sha: GitSha) -> Result<(), GitError> {
        let raw = self.commit(sha)?;
        let parent_tree = raw.parents.first().map(|p| self.tree_of(*p)).transpose()?;
        let dirs = changed_dirs(&mut self.repo, parent_tree, Some(raw.tree))?;
        let mut changes = Vec::new();
        for c in git_changes(&mut self.repo, &dirs)? {
            changes.push(match c.new {
                None => Change::Delete { path: c.path },
                Some((EntryKind::Submodule, s)) => Change::Put {
                    path: c.path,
                    kind: EntryKind::Submodule,
                    id: submodule_id(s),
                },
                Some((kind, s)) => Change::Put {
                    path: c.path,
                    kind,
                    id: self.blob_id(s)?.0,
                },
            });
        }
        if self.blob_queue_bytes >= PUT_BYTES_MAX && self.writing() {
            self.upload_blobs().await?;
        }
        let mut parents = Vec::with_capacity(raw.parents.len());
        for p in &raw.parents {
            parents.push(self.ids.changeset(self.map, *p)?);
        }
        let base = match raw.parents.first() {
            Some(p) => Some(self.root_of(*p).await?).filter(|r| *r != empty_root()),
            None => None,
        };
        let applied = apply_changes_with(&self.trees(), base, &changes, &ApplyOptions::default())
            .await
            .map_err(|e| GitError::Invalid {
                commit: sha,
                rule: match e {
                    graft_model::ApplyError::Invalid(v) => v.rule.as_str().to_owned(),
                    graft_model::ApplyError::Source(s) => format!("source: {s}"),
                },
            })?;
        let root = applied.root.unwrap_or_else(empty_root);
        self.trees
            .lock()
            .expect("tree cache")
            .extend(applied.new_trees);
        let (name, email, secs, offset_min) = author(&raw.headers);
        let message = String::from_utf8_lossy(&raw.message).into_owned();
        let changeset = Changeset {
            parents,
            root,
            changes,
            author: Author {
                name,
                email,
                kind: AuthorKind::Human,
            },
            authored_at: Timestamp {
                ms: secs.saturating_mul(1000),
                offset_min,
            },
            message,
            metadata: Metadata::new(),
        };
        changeset
            .check(&CheckOptions {
                allow_root: true,
                portable_paths: true,
                ..CheckOptions::default()
            })
            .map_err(|v| GitError::Invalid {
                commit: sha,
                rule: v.rule.as_str().to_owned(),
            })?;
        let actual: Vec<(String, Option<GitSha>)> = dirs
            .iter()
            .map(|d| Ok((dir_path(d)?, d.new)))
            .collect::<Result<_, GitError>>()?;
        let raw_trees = learn_raw_trees(&mut self.src(), parent_tree, &changeset.changes, &actual)?;
        let record = ImportedCommit {
            sha,
            headers: raw.headers.clone(),
            message: (raw.message != changeset.message.as_bytes()).then(|| raw.message.clone()),
            raw_trees,
        };
        let id = changeset.id();
        if let Some(other) = self.ids.sha_if_known(self.map, id)?
            && other != sha
        {
            // Keep going, so the error lists every pair; nothing more is
            // written. Descendants see this commit as the changeset it
            // collides with.
            self.indistinct.push((other, sha));
            self.ids.this_run.insert(sha, id);
            self.roots.insert(sha, root);
            return Ok(());
        }
        if self.verify(&changeset, &record, parent_tree).await? {
            self.report.verified += 1;
        } else if self.config.allow_lossy.contains(&sha) {
            self.report.lossy.push(sha);
        } else {
            self.failed.push(sha);
        }
        self.ids.insert(sha, id);
        self.roots.insert(sha, root);
        self.pending.push((changeset, record));
        Ok(())
    }

    /// Whether every commit so far can be written.
    fn writing(&self) -> bool {
        self.failed.is_empty() && self.indistinct.is_empty()
    }

    fn trees(&self) -> Trees<'_, A> {
        Trees {
            cache: &self.trees,
            api: self.api,
            cx: self.cx,
            repo: &self.config.repo,
        }
    }

    fn src(&self) -> Src<'_> {
        Src {
            repo: &self.repo,
            blob_shas: &self.blob_shas,
            map: self.map,
        }
    }

    /// The fidelity check: the export, from graft's changeset, its parents'
    /// git shas and the record alone, reproduces the commit's sha; and every
    /// raw tree in the record is a git encoding of exactly the graft tree at
    /// its path, so a record can never stand in for a missing change.
    async fn verify(
        &mut self,
        changeset: &Changeset,
        record: &ImportedCommit,
        parent_tree: Option<GitSha>,
    ) -> Result<bool, GitError> {
        let tree = export_tree(
            &mut self.src(),
            parent_tree,
            &changeset.changes,
            &record.raw_trees,
        )?;
        let mut parents = Vec::with_capacity(changeset.parents.len());
        for p in &changeset.parents {
            parents.push(self.ids.sha(self.map, *p)?);
        }
        if commit_sha(tree, &parents, record, &changeset.message) != record.sha {
            return Ok(false);
        }
        for r in &record.raw_trees {
            let git_side = match &r.tree {
                Some(raw) => self.convert(tree_sha(raw))?,
                None => None,
            };
            if graft_at(&self.trees(), changeset.root, r.dir.as_ref()).await? != git_side {
                return Ok(false);
            }
        }
        Ok(true)
    }

    /// The graft tree a git tree maps to (`None`: it holds no entries once
    /// empty subtrees are dropped). Memoized; reads blobs it has not hashed.
    fn convert(&mut self, sha: GitSha) -> Result<Option<TreeId>, GitError> {
        if let Some(t) = self.converted.get(&sha) {
            return Ok(*t);
        }
        let entries = self.repo.tree(sha)?;
        let mut out = Vec::with_capacity(entries.len());
        for e in entries.iter() {
            let name = String::from_utf8(e.name.clone())
                .map_err(|_| GitError::Read("unchecked path".into()))?;
            let (kind, id): (EntryKind, ObjectId) = match e.kind() {
                Some(EntryKind::Tree) => match self.convert(e.sha)? {
                    Some(t) => (EntryKind::Tree, t.0),
                    None => continue,
                },
                Some(EntryKind::Submodule) => (EntryKind::Submodule, submodule_id(e.sha)),
                Some(kind) => (kind, self.blob_id(e.sha)?.0),
                None => return Err(GitError::Read("unchecked mode".into())),
            };
            out.push(TreeEntry { name, kind, id });
        }
        out.sort_by(|a, b| a.name.as_bytes().cmp(b.name.as_bytes()));
        let id = (!out.is_empty()).then(|| Tree { entries: out }.id());
        self.converted.insert(sha, id);
        Ok(id)
    }

    /// Uploads queued blobs the server does not have.
    async fn upload_blobs(&mut self) -> Result<(), GitError> {
        let queue = std::mem::take(&mut self.blob_queue);
        self.blob_queue_bytes = 0;
        let mut missing = HashSet::new();
        for chunk in queue.chunks(HAS_OBJECTS_MAX) {
            let ids = chunk.iter().map(|(id, _)| id.0).collect();
            let req = HasObjects {
                repo: self.config.repo.clone(),
                ids,
            };
            missing.extend(self.api.has_objects(self.cx, req).await?);
        }
        let mut batch = Vec::new();
        let mut bytes = 0;
        for (id, payload) in queue {
            if !missing.contains(&id.0) {
                continue;
            }
            bytes += payload.len();
            batch.push(Frame::encode(ObjectKind::Blob, &payload));
            if batch.len() >= PUT_BLOBS_MAX || bytes >= PUT_BYTES_MAX {
                self.put_blobs(std::mem::take(&mut batch)).await?;
                bytes = 0;
            }
        }
        if !batch.is_empty() {
            self.put_blobs(batch).await?;
        }
        Ok(())
    }

    async fn put_blobs(&mut self, blobs: Vec<Frame>) -> Result<(), GitError> {
        let n = blobs.len() as u64;
        let req = PutObjects {
            repo: self.config.repo.clone(),
            blobs,
        };
        self.api.put_objects(self.cx, req).await?;
        self.report.blobs += n;
        Ok(())
    }

    /// Step 4: blobs, then the changesets, then the mirror map. Writes
    /// nothing once a commit has failed the fidelity check or collided.
    async fn flush(&mut self) -> Result<(), GitError> {
        if !self.writing() || self.pending.is_empty() {
            return Ok(());
        }
        self.upload_blobs().await?;
        let pending = std::mem::take(&mut self.pending);
        let changesets: Vec<Changeset> = pending.iter().map(|(c, _)| c.clone()).collect();
        let created = self
            .api
            .import_changesets(
                self.cx,
                ImportChangesets {
                    repo: self.config.repo.clone(),
                    changesets,
                },
            )
            .await?;
        if created.len() != pending.len() {
            return Err(GitError::Map(format!(
                "server created {} of {} changesets",
                created.len(),
                pending.len()
            )));
        }
        self.map
            .put_blob_shas(&std::mem::take(&mut self.new_blob_shas))?;
        for ((changeset, record), c) in pending.into_iter().zip(created) {
            let id = changeset.id();
            if c.id != id {
                return Err(GitError::Map(format!(
                    "server derived {} for commit {}, expected {id}",
                    c.id, record.sha
                )));
            }
            self.report.commits += 1;
            self.report.raw_trees += record.raw_trees.len() as u64;
            self.map.put_imported(id, record)?;
        }
        Ok(())
    }
}

/// The graft tree at `dir` in the snapshot rooted at `root`.
async fn graft_at<A: ObjectApi>(
    source: &Trees<'_, A>,
    root: TreeId,
    dir: Option<&RepoPath>,
) -> Result<Option<TreeId>, GitError> {
    if root == empty_root() {
        return Ok(None);
    }
    let mut at = root;
    for name in dir.into_iter().flat_map(RepoPath::components) {
        let tree = source
            .tree(at)
            .await
            .map_err(|e| GitError::Read(format!("graft tree: {e}")))?;
        match tree.get(name) {
            Some(e) if e.kind.is_tree() => at = TreeId(e.id),
            _ => return Ok(None),
        }
    }
    Ok(Some(at))
}
