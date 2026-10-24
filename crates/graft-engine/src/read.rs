//! Snapshot reads without fetching trees: `Stat`, `ListTree`, `ReadFile`,
//! and `GetChangesets` (`docs/design.md` §7.2).
//!
//! Each read touches only the directories on its path. A recursive listing
//! is a pre-order walk in component order, paged by the last path returned;
//! a resumed page skips every subtree wholly before that path unread.

use std::cmp::Ordering;

use graft_model::path::check_component;
use graft_model::{BlobId, ObjectKind, ObjectSource, RepoPath, Tree, TreeEntry, TreeId};
use graft_proto::types::{
    ChangesetView, Changesets, Entries, EntryView, FileContent, GetChangesets, ListTree, PathAt,
};
use graft_proto::{ApiError, CallContext};
use graft_store::{MetaStore, ObjectStore, RepoId};

use crate::access::{Need, require};
use crate::source::Trees;
use crate::write::draft_state;
use crate::{Engine, errors};

/// Pre-order position: paths compared component by component, so a
/// directory's entries follow it directly.
fn preorder(a: &RepoPath, b: &RepoPath) -> Ordering {
    a.components()
        .map(str::as_bytes)
        .cmp(b.components().map(str::as_bytes))
}

fn child(dir: Option<&RepoPath>, name: &str) -> Result<RepoPath, ApiError> {
    let joined = match dir {
        None => RepoPath::new(name),
        Some(d) => d.join(name),
    };
    // Stored trees only hold valid names, so a failure is a bad tree.
    joined.map_err(|e| errors::path(name, e))
}

fn entry_view(path: RepoPath, e: &TreeEntry) -> EntryView {
    EntryView {
        path,
        kind: e.kind,
        id: e.id,
    }
}

impl<M: MetaStore, O: ObjectStore> Engine<M, O> {
    /// The entry at `path` in `at`'s snapshot, or `NotFound`.
    async fn entry_at(
        &self,
        repo: RepoId,
        trees: &Trees<'_, O>,
        at: graft_model::ChangesetId,
        path: &RepoPath,
    ) -> Result<TreeEntry, ApiError> {
        let root = self.root_of(repo, at).await?;
        trees
            .lookup(root, path)
            .await
            .map_err(errors::source)?
            .ok_or_else(|| errors::not_found(format!("path {path} at {at}")))
    }

    pub(crate) async fn do_stat(
        &self,
        cx: &CallContext,
        req: PathAt,
    ) -> Result<EntryView, ApiError> {
        require(cx, &req.repo, Need::Read)?;
        let repo = self.repo_id(&req.repo).await?;
        let trees = Trees::new(&*self.objects, repo);
        let e = self.entry_at(repo, &trees, req.at, &req.path).await?;
        Ok(entry_view(req.path, &e))
    }

    pub(crate) async fn do_read_file(
        &self,
        cx: &CallContext,
        req: PathAt,
    ) -> Result<FileContent, ApiError> {
        require(cx, &req.repo, Need::Read)?;
        let repo = self.repo_id(&req.repo).await?;
        let trees = Trees::new(&*self.objects, repo);
        let e = self.entry_at(repo, &trees, req.at, &req.path).await?;
        if e.kind.is_tree() || e.kind == graft_model::EntryKind::Submodule {
            return Err(ApiError::InvalidPath {
                path: req.path.to_string(),
                reason: "not-a-file".to_owned(),
            });
        }
        let data = self
            .payload(repo, e.id, ObjectKind::Blob)
            .await?
            .ok_or(ApiError::MissingObjects { ids: vec![e.id] })?;
        Ok(FileContent {
            kind: e.kind,
            id: BlobId(e.id),
            data,
        })
    }

    pub(crate) async fn do_list_tree(
        &self,
        cx: &CallContext,
        req: ListTree,
    ) -> Result<Entries, ApiError> {
        require(cx, &req.repo, Need::Read)?;
        let repo = self.repo_id(&req.repo).await?;
        let trees = Trees::new(&*self.objects, repo);
        let dir = match &req.path {
            None => self.root_of(repo, req.at).await?,
            Some(path) => {
                let e = self.entry_at(repo, &trees, req.at, path).await?;
                if !e.kind.is_tree() {
                    return Err(ApiError::InvalidPath {
                        path: path.to_string(),
                        reason: "not-a-directory".to_owned(),
                    });
                }
                TreeId(e.id)
            }
        };
        let limit = self.page_size(req.page.limit) as usize;
        let token = req.page.token.as_deref();
        if req.recursive {
            let after = token
                .map(|t| RepoPath::new(t).map_err(|_| errors::invalid(None, "page-token")))
                .transpose()?;
            list_recursive(&trees, dir, req.path, after, limit).await
        } else {
            if let Some(t) = token {
                check_component(t).map_err(|_| errors::invalid(None, "page-token"))?;
            }
            let tree = trees.tree(dir).await.map_err(errors::source)?;
            let mut entries = Vec::new();
            let mut more = false;
            for e in &tree.entries {
                if token.is_some_and(|t| e.name.as_bytes() <= t.as_bytes()) {
                    continue;
                }
                if entries.len() == limit {
                    more = true;
                    break;
                }
                entries.push(entry_view(child(req.path.as_ref(), &e.name)?, e));
            }
            let next = more.then(|| entries.last().map(|e| e.path.name().to_owned()));
            Ok(Entries {
                entries,
                next: next.flatten(),
            })
        }
    }

    pub(crate) async fn do_get_changesets(
        &self,
        cx: &CallContext,
        req: GetChangesets,
    ) -> Result<Changesets, ApiError> {
        require(cx, &req.repo, Need::Read)?;
        let repo = self.repo_id(&req.repo).await?;
        if req.ids.len() > self.config.max_ids as usize {
            return Err(errors::limit("ids", self.config.max_ids.into()));
        }
        let mut out = Changesets::default();
        for id in req.ids {
            match self.changeset_opt(repo, id).await {
                Ok(Some((changeset, record))) => out.found.push(ChangesetView {
                    changeset,
                    lsn: record.lsn,
                    state: draft_state(record.state),
                    superseded_by: record.superseded_by,
                }),
                Ok(None) => out.missing.push(id),
                Err(ApiError::Collected { id }) => out.collected.push(id),
                Err(e) => return Err(e),
            }
        }
        Ok(out)
    }
}

/// One directory being walked: its path and the entries still to visit.
struct Frame {
    path: Option<RepoPath>,
    tree: Tree,
    next: usize,
}

/// Up to `limit` entries under `dir`, in pre-order, after `after`.
async fn list_recursive<O: ObjectStore>(
    trees: &Trees<'_, O>,
    dir: TreeId,
    dir_path: Option<RepoPath>,
    after: Option<RepoPath>,
    limit: usize,
) -> Result<Entries, ApiError> {
    let mut stack = vec![Frame {
        path: dir_path,
        tree: trees.tree(dir).await.map_err(errors::source)?,
        next: 0,
    }];
    let mut entries: Vec<EntryView> = Vec::new();
    while let Some(top) = stack.last_mut() {
        let Some(e) = top.tree.entries.get(top.next).cloned() else {
            stack.pop();
            continue;
        };
        top.next += 1;
        let path = child(top.path.as_ref(), &e.name)?;
        let descend = e.kind.is_tree();
        if let Some(after) = &after
            && preorder(&path, after) != Ordering::Greater
        {
            // At or before the token: skip it, and skip its subtree unless
            // the token lies inside it (or is the directory itself).
            if descend && after.starts_with(&path) {
                let tree = trees.tree(TreeId(e.id)).await.map_err(errors::source)?;
                stack.push(Frame {
                    path: Some(path),
                    tree,
                    next: 0,
                });
            }
            continue;
        }
        if entries.len() == limit {
            let next = entries.last().map(|v| v.path.to_string());
            return Ok(Entries { entries, next });
        }
        entries.push(entry_view(path.clone(), &e));
        if descend {
            let tree = trees.tree(TreeId(e.id)).await.map_err(errors::source)?;
            stack.push(Frame {
                path: Some(path),
                tree,
                next: 0,
            });
        }
    }
    Ok(Entries {
        entries,
        next: None,
    })
}
