//! Reading stored objects and changesets: a verifying tree source for the
//! `graft-model` algorithms, path lookup in a snapshot, and the shared
//! loaders every operation starts from.

use std::collections::HashMap;
use std::sync::Mutex;

use graft_model::{
    Changeset, ChangesetId, ObjectId, ObjectKind, ObjectSource, RepoPath, SourceError, Tree,
    TreeEntry, TreeId,
};
use graft_proto::{ApiError, Frame};
use graft_store::{ChangesetRecord, MetaStore, ObjectStore, RecordState, RepoId};

use crate::{Engine, errors};

/// Reads and verifies trees from the object store, caching each tree for
/// the life of one request (a request reads each directory at most once).
pub(crate) struct Trees<'a, O> {
    objects: &'a O,
    repo: RepoId,
    cache: Mutex<HashMap<TreeId, Tree>>,
}

impl<'a, O: ObjectStore> Trees<'a, O> {
    pub(crate) fn new(objects: &'a O, repo: RepoId) -> Self {
        Self {
            objects,
            repo,
            cache: Mutex::new(HashMap::new()),
        }
    }

    /// Makes trees that are not stored yet readable through this source
    /// (a planned changeset's new trees, before they are written).
    pub(crate) fn remember(&self, trees: &[(TreeId, Tree)]) {
        let mut cache = self.cache.lock().expect("tree cache");
        for (id, tree) in trees {
            cache.insert(*id, tree.clone());
        }
    }

    fn cached(&self, id: TreeId) -> Option<Tree> {
        self.cache.lock().expect("tree cache").get(&id).cloned()
    }

    /// The entry at `path` in the snapshot rooted at `root`, reading only
    /// the directories on the path.
    pub(crate) async fn lookup(
        &self,
        root: TreeId,
        path: &RepoPath,
    ) -> Result<Option<TreeEntry>, SourceError> {
        let mut dir = root;
        let mut components = path.components().peekable();
        while let Some(name) = components.next() {
            let tree = self.tree(dir).await?;
            let Some(entry) = tree.get(name) else {
                return Ok(None);
            };
            if components.peek().is_none() {
                return Ok(Some(entry.clone()));
            }
            if !entry.kind.is_tree() {
                return Ok(None);
            }
            dir = TreeId(entry.id);
        }
        Ok(None)
    }
}

impl<O: ObjectStore> ObjectSource for Trees<'_, O> {
    async fn tree(&self, id: TreeId) -> Result<Tree, SourceError> {
        if let Some(tree) = self.cached(id) {
            return Ok(tree);
        }
        let bytes = self
            .objects
            .get(self.repo, id.0)
            .await
            .map_err(|_| SourceError::Backend("object store read".to_owned()))?
            .ok_or(SourceError::Missing(id))?;
        let payload = Frame::from_bytes(&bytes)
            .ok()
            .filter(|f| f.kind == ObjectKind::Tree)
            .and_then(|f| f.decode_verified(id.0).ok())
            .ok_or(SourceError::Corrupt(id))?;
        let tree = Tree::decode(&payload).map_err(|_| SourceError::Malformed(id))?;
        self.cache
            .lock()
            .expect("tree cache")
            .insert(id, tree.clone());
        Ok(tree)
    }
}

/// The id of the empty tree: the root of an empty snapshot.
pub(crate) fn empty_tree() -> TreeId {
    Tree::default().id()
}

/// The snapshot root as `graft-model` takes it: `None` when empty.
pub(crate) fn model_root(root: TreeId) -> Option<TreeId> {
    (root != empty_tree()).then_some(root)
}

impl<M: MetaStore, O: ObjectStore> Engine<M, O> {
    /// The repository's id, or `NotFound`.
    pub(crate) async fn repo_id(&self, name: &str) -> Result<RepoId, ApiError> {
        self.meta
            .repo(name)
            .await
            .map_err(errors::store)?
            .ok_or_else(|| errors::not_found(format!("repository {name}")))
    }

    /// A stored changeset and its record, or `NotFound` / `Collected`.
    pub(crate) async fn changeset(
        &self,
        repo: RepoId,
        id: ChangesetId,
    ) -> Result<(Changeset, ChangesetRecord), ApiError> {
        match self.changeset_opt(repo, id).await? {
            Some(found) => Ok(found),
            None => Err(errors::not_found(format!("changeset {id}"))),
        }
    }

    /// Like [`Engine::changeset`], but `None` for a changeset never stored.
    pub(crate) async fn changeset_opt(
        &self,
        repo: RepoId,
        id: ChangesetId,
    ) -> Result<Option<(Changeset, ChangesetRecord)>, ApiError> {
        let Some((content, record)) = self.meta.changeset(repo, id).await.map_err(errors::store)?
        else {
            return Ok(None);
        };
        if record.state == RecordState::Collected {
            return Err(ApiError::Collected { id });
        }
        let cs = Changeset::decode(&content).map_err(|_| errors::corrupt_changeset(id))?;
        Ok(Some((cs, record)))
    }

    /// A changeset's snapshot root.
    pub(crate) async fn root_of(&self, repo: RepoId, id: ChangesetId) -> Result<TreeId, ApiError> {
        Ok(self.changeset(repo, id).await?.0.root)
    }

    /// A verified object payload of `kind`, or `None` when it is not stored.
    pub(crate) async fn payload(
        &self,
        repo: RepoId,
        id: ObjectId,
        kind: ObjectKind,
    ) -> Result<Option<Vec<u8>>, ApiError> {
        let Some(bytes) = self.objects.get(repo, id).await.map_err(errors::store)? else {
            return Ok(None);
        };
        Frame::from_bytes(&bytes)
            .ok()
            .filter(|f| f.kind == kind)
            .and_then(|f| f.decode_verified(id).ok())
            .map(Some)
            .ok_or(ApiError::CorruptObject { id })
    }
}
