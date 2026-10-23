//! `Gc` through `graft-gc` (`docs/design.md` §8.6): the dry run, and one
//! sweep pass. The sweep refuses a repository whose deletion switch the
//! operator has not turned on, so nothing here deletes until they do.

use std::collections::HashMap;
use std::sync::Arc;

use graft_gc::{Collector, GcConfig, GcError, Pace, Shape, SweepLeases, SweepReport};
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

/// How the engine runs the collector: its timings, the pause between
/// batches, and the sweep leases every pass of this process shares.
#[derive(Clone)]
pub struct GcSettings {
    /// Timings and limits.
    pub config: GcConfig,
    /// The rate limit's timer.
    pub pace: Arc<dyn Pace>,
    /// This process's sweep leases.
    pub leases: SweepLeases,
}

impl Default for GcSettings {
    fn default() -> Self {
        Self {
            config: GcConfig::default(),
            pace: Arc::new(graft_gc::ThreadPace),
            leases: SweepLeases::new(),
        }
    }
}

impl std::fmt::Debug for GcSettings {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("GcSettings")
            .field("config", &self.config)
            .field("leases", &self.leases)
            .finish_non_exhaustive()
    }
}

impl<M: MetaStore, O: ObjectStore> Engine<M, O> {
    pub(crate) async fn do_gc(&self, cx: &CallContext, req: Gc) -> ApiResult<GcReport> {
        if req.dry_run {
            require(cx, &req.repo, Need::Admin)?;
            let repo = self.repo_id(&req.repo).await?;
            let report = self
                .collector()
                .dry_run(repo, self.clock.now().ms, &self.gc_shape())
                .await
                .map_err(gc_error)?;
            return Ok(GcReport {
                changesets: report.changesets,
                objects: report.doomed,
                bytes: report.bytes,
            });
        }
        let swept = self.gc_sweep(cx, &req.repo, false).await?;
        Ok(GcReport {
            changesets: swept.collected,
            objects: swept.deleted + swept.recovered,
            bytes: swept.bytes,
        })
    }

    /// One sweep pass over the repository `repo` (design §8.6), for `Gc`,
    /// `admin gc` and the server's scheduler, which records the report in
    /// its metrics. With `verify`, the plan is checked against the ancestry
    /// walk first. It needs the repository's admin scope, and is refused
    /// while the operator has not turned deletion on.
    pub async fn gc_sweep(
        &self,
        cx: &CallContext,
        repo: &str,
        verify: bool,
    ) -> ApiResult<SweepReport> {
        require(cx, repo, Need::Admin)?;
        let id = self.repo_id(repo).await?;
        self.collector()
            .sweep(id, &self.gc_shape(), verify)
            .await
            .map_err(gc_error)
    }

    fn collector(&self) -> Collector<Arc<M>, Arc<O>> {
        Collector::new(
            self.meta.clone(),
            self.objects.clone(),
            self.gc.config.clone(),
        )
        .with_clock(self.clock.clone())
        .with_pace(self.gc.pace.clone())
        .with_leases(self.gc.leases.clone())
    }

    fn gc_shape(&self) -> GcShape<'_, M, O> {
        GcShape {
            meta: &self.meta,
            objects: &self.objects,
        }
    }
}

/// A collector refusal as the API reports it. Busy is retryable; the rest
/// are the operator's to fix.
fn gc_error(e: GcError) -> ApiError {
    match e {
        GcError::Store(e) => errors::store(e),
        GcError::Busy => ApiError::Unavailable {
            retry_after_ms: 60_000,
        },
        other => ApiError::Rejected {
            policy: "gc".to_owned(),
            detail: other.to_string(),
        },
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
