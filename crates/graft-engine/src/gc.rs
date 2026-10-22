//! `Gc` through `graft-gc` (`docs/design.md` §8.6). Only the dry run is
//! built: the sweep is refused until G3.2, so no path here deletes.

use std::collections::HashMap;

use graft_gc::{GcConfig, Shape, dry_run_report};
use graft_model::{
    Changeset, ChangesetId, EntryKind, ObjectId, ObjectSource, SourceError, Tree, TreeEntry, TreeId,
};
use graft_proto::CallContext;
use graft_proto::error::{ApiError, ApiResult};
use graft_proto::types::{Gc, GcReport};
use graft_store::{MetaStore, ObjectStore, RepoId, StoreError, StoreResult};

use crate::Engine;
use crate::access::{Need, require};
use crate::errors;
use crate::source::Trees;

impl<M: MetaStore, O: ObjectStore> Engine<M, O> {
    pub(crate) async fn do_gc(&self, cx: &CallContext, req: Gc) -> ApiResult<GcReport> {
        require(cx, &req.repo, Need::Admin)?;
        if !req.dry_run {
            return Err(ApiError::Rejected {
                policy: "gc".to_owned(),
                detail: "the GC sweep is not built yet (docs/plan.md, G3.2)".to_owned(),
            });
        }
        let repo = self.repo_id(&req.repo).await?;
        let shape = GcShape {
            meta: &self.meta,
            objects: &self.objects,
        };
        let report = dry_run_report(
            &self.meta,
            &self.objects,
            &GcConfig::default(),
            repo,
            self.clock.now().ms,
            &shape,
        )
        .await
        .map_err(errors::store)?;
        Ok(GcReport {
            changesets: report.changesets,
            objects: report.doomed,
            bytes: report.bytes,
        })
    }
}

/// What the collector reads from stored changesets and trees.
struct GcShape<'a, M, O> {
    meta: &'a M,
    objects: &'a O,
}

impl<M: MetaStore, O: ObjectStore> Shape for GcShape<'_, M, O> {
    fn parents(&self, content: &[u8]) -> StoreResult<Vec<ChangesetId>> {
        Ok(decode(content)?.parents)
    }

    async fn introduced(
        &self,
        repo: RepoId,
        _id: ChangesetId,
        content: &[u8],
    ) -> StoreResult<Vec<ObjectId>> {
        let changeset = decode(content)?;
        // A parent we no longer hold makes the base the empty tree: a
        // superset, which the collector's row checks keep safe.
        let base = match changeset.parents.first() {
            Some(parent) => match self.meta.changeset(repo, *parent).await? {
                Some((bytes, _)) => Some(decode(&bytes)?.root),
                None => None,
            },
            None => None,
        };
        introduced_by(self.objects, repo, base, changeset.root).await
    }
}

fn decode(content: &[u8]) -> StoreResult<Changeset> {
    Changeset::decode(content).map_err(|_| StoreError::Corrupt("changeset content".to_owned()))
}

/// The objects a change holds rows for: the blobs and trees on the paths
/// that differ from `base`, found by an id-pruned walk, so the cost follows
/// the change and not the snapshot.
async fn introduced_by<O: ObjectStore>(
    objects: &O,
    repo: RepoId,
    base: Option<TreeId>,
    root: TreeId,
) -> StoreResult<Vec<ObjectId>> {
    let trees = Trees::new(objects, repo);
    let mut out = Vec::new();
    let mut stack = vec![(base, root)];
    while let Some((old, new)) = stack.pop() {
        if old == Some(new) {
            continue;
        }
        out.push(new.0);
        let new_tree = trees.tree(new).await.map_err(source)?;
        let old_tree: Option<Tree> = match old {
            Some(t) => Some(trees.tree(t).await.map_err(source)?),
            None => None,
        };
        let before: HashMap<&str, &TreeEntry> = old_tree
            .iter()
            .flat_map(|t| t.entries.iter())
            .map(|e| (e.name.as_str(), e))
            .collect();
        for entry in &new_tree.entries {
            let previous = before.get(entry.name.as_str()).copied();
            if previous.is_some_and(|p| p.id == entry.id && p.kind == entry.kind) {
                continue;
            }
            match entry.kind {
                EntryKind::Submodule => {}
                EntryKind::Tree => stack.push((
                    previous.filter(|p| p.kind.is_tree()).map(|p| TreeId(p.id)),
                    TreeId(entry.id),
                )),
                _ => out.push(entry.id),
            }
        }
    }
    Ok(out)
}

fn source(e: SourceError) -> StoreError {
    match e {
        SourceError::Backend(msg) => StoreError::Backend(msg),
        other => StoreError::Corrupt(format!("{other:?}")),
    }
}
