//! `admin fsck`: verify that every object a repository references exists
//! and hashes to its id (`docs/design.md` §6.7, §14).
//!
//! Every changeset writes a `ChangesetCreated` journal entry in the
//! transaction that stores it, so the journal lists every changeset. For
//! each one fsck decodes the stored content, checks its id and parents, and
//! walks its snapshot, reading each tree and blob once. Refs must point at
//! stored changesets. Each workspace's draft path rows must match the paths
//! its stack (`base..head`) changes (`docs/notes/draft-overlap.md` §3).
//! This is the one operation that reads a whole
//! repository; it is an offline admin check, never a request path.

use std::collections::{BTreeSet, HashSet};

use graft_engine::journal::CHANGESET_CREATED;
use graft_model::{
    Changeset, ChangesetId, EntryKind, ObjectId, ObjectKind, RepoPath, Tree, TreeId,
};
use graft_proto::Frame;
use graft_store::{
    MAX_PAGE, MetaStore, ObjectStore, RecordState, RepoId, StoreError, WorkspaceRecord,
};

/// The deepest stack fsck walks before calling it broken.
const MAX_STACK_WALK: usize = 100_000;

/// Problems kept in the report; the count goes on past this.
const MAX_REPORTED: usize = 1000;

/// What fsck found.
#[derive(Debug, Default)]
pub(crate) struct Report {
    pub(crate) changesets: u64,
    pub(crate) collected: u64,
    pub(crate) trees: u64,
    pub(crate) blobs: u64,
    pub(crate) bytes: u64,
    pub(crate) refs: u64,
    pub(crate) workspaces: u64,
    pub(crate) problems: u64,
    pub(crate) details: Vec<String>,
}

impl Report {
    fn problem(&mut self, what: String) {
        self.problems += 1;
        if self.details.len() < MAX_REPORTED {
            self.details.push(what);
        }
    }

    /// Whether nothing is wrong.
    pub(crate) fn is_clean(&self) -> bool {
        self.problems == 0
    }
}

/// Checks repository `name`. Fails only when a store cannot be read; what
/// is missing or corrupt goes in the report.
pub(crate) async fn fsck<M: MetaStore, O: ObjectStore>(
    meta: &M,
    objects: &O,
    name: &str,
) -> Result<Report, String> {
    let repo = meta
        .repo(name)
        .await
        .map_err(store)?
        .ok_or_else(|| format!("no repository {name}"))?;
    let mut walk = Walk {
        meta,
        objects,
        repo,
        seen: HashSet::new(),
        report: Report::default(),
    };
    let mut from = 1;
    loop {
        let rows = meta.journal(repo, from, MAX_PAGE).await.map_err(store)?;
        let Some((last, _, _)) = rows.last() else {
            break;
        };
        from = last + 1;
        for (lsn, kind, body) in rows {
            if kind != CHANGESET_CREATED {
                continue;
            }
            match created_id(&body) {
                Some(id) => walk.changeset(id).await?,
                None => walk.report.problem(format!(
                    "journal entry {lsn}: unreadable ChangesetCreated body"
                )),
            }
        }
    }
    for r in meta.list_refs(repo, "", MAX_PAGE).await.map_err(store)? {
        walk.report.refs += 1;
        if meta
            .changeset(repo, r.target)
            .await
            .map_err(store)?
            .is_none()
        {
            walk.report.problem(format!(
                "ref {} points at missing changeset {}",
                r.name, r.target
            ));
        }
    }
    let now_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| i64::try_from(d.as_millis()).unwrap_or(i64::MAX));
    let mut after = String::new();
    loop {
        let page = meta
            .list_all_workspaces(repo, &after, MAX_PAGE)
            .await
            .map_err(store)?;
        let Some(last) = page.last() else {
            break;
        };
        after = last.name.clone();
        for ws in &page {
            walk.report.workspaces += 1;
            walk.draft_paths(ws, now_ms).await?;
        }
    }
    Ok(walk.report)
}

fn store(e: StoreError) -> String {
    format!("store: {e:?}")
}

/// The id in a `ChangesetCreated` body: `{"id":"c…",…}`.
fn created_id(body: &[u8]) -> Option<ChangesetId> {
    let v: serde_json::Value = serde_json::from_slice(body).ok()?;
    ChangesetId::parse(v.get("id")?.as_str()?).ok()
}

struct Walk<'a, M, O> {
    meta: &'a M,
    objects: &'a O,
    repo: RepoId,
    seen: HashSet<ObjectId>,
    report: Report,
}

impl<M: MetaStore, O: ObjectStore> Walk<'_, M, O> {
    async fn changeset(&mut self, id: ChangesetId) -> Result<(), String> {
        let Some((content, record)) = self.meta.changeset(self.repo, id).await.map_err(store)?
        else {
            self.report
                .problem(format!("changeset {id} is journalled but not stored"));
            return Ok(());
        };
        if record.state == RecordState::Collected {
            self.report.collected += 1;
            return Ok(());
        }
        self.report.changesets += 1;
        let cs = match Changeset::decode(&content) {
            Ok(cs) if cs.id() == id => cs,
            Ok(_) => {
                self.report
                    .problem(format!("changeset {id}: content does not hash to its id"));
                return Ok(());
            }
            Err(_) => {
                self.report
                    .problem(format!("changeset {id}: content does not decode"));
                return Ok(());
            }
        };
        for parent in &cs.parents {
            if self
                .meta
                .changeset(self.repo, *parent)
                .await
                .map_err(store)?
                .is_none()
            {
                self.report
                    .problem(format!("changeset {id}: parent {parent} is not stored"));
            }
        }
        self.snapshot(id, cs.root).await
    }

    /// Checks `ws`'s draft path rows against the union of the paths its
    /// stack changes. A row outside the union is always wrong. A missing
    /// row is wrong only while the workspace is live with a draft head:
    /// GC's lapsing pass clears the rows of an expired one first.
    async fn draft_paths(&mut self, ws: &WorkspaceRecord, now_ms: i64) -> Result<(), String> {
        let name = &ws.name;
        let mut rows = BTreeSet::new();
        let mut after = None;
        loop {
            let page = self
                .meta
                .workspace_draft_paths(self.repo, name, after.clone(), MAX_PAGE)
                .await
                .map_err(store)?;
            let full = page.len() as u64 >= u64::from(MAX_PAGE);
            after = page.last().cloned();
            rows.extend(page);
            if !full {
                break;
            }
        }
        let mut union: BTreeSet<RepoPath> = BTreeSet::new();
        let mut head_is_draft = false;
        let mut at = ws.head;
        let mut depth = 0;
        while let Some(id) = at {
            if id == ws.base {
                break;
            }
            depth += 1;
            if depth > MAX_STACK_WALK {
                self.report.problem(format!(
                    "workspace {name}: stack deeper than {MAX_STACK_WALK}"
                ));
                return Ok(());
            }
            let Some((content, record)) =
                self.meta.changeset(self.repo, id).await.map_err(store)?
            else {
                self.report.problem(format!(
                    "workspace {name}: stack changeset {id} is not stored"
                ));
                return Ok(());
            };
            if Some(id) == ws.head {
                head_is_draft = record.state == RecordState::Draft
                    && record.holder.as_deref() == Some(name.as_str());
            }
            let Ok(cs) = Changeset::decode(&content) else {
                // Reported by the changeset walk.
                return Ok(());
            };
            union.extend(cs.changes.iter().map(|c| c.path().clone()));
            at = cs.first_parent();
        }
        if at.is_none() && ws.head.is_some() {
            self.report.problem(format!(
                "workspace {name}: head does not descend from its base"
            ));
            return Ok(());
        }
        for path in rows.difference(&union) {
            self.report.problem(format!(
                "workspace {name}: draft path row {path} is not in its stack"
            ));
        }
        if head_is_draft && ws.lease_until_ms >= now_ms {
            for path in union.difference(&rows) {
                self.report.problem(format!(
                    "workspace {name}: stack path {path} has no draft path row"
                ));
            }
        }
        Ok(())
    }

    /// Walks a snapshot depth first, each object once per repository.
    async fn snapshot(&mut self, at: ChangesetId, root: TreeId) -> Result<(), String> {
        let mut pending = vec![root];
        while let Some(tree_id) = pending.pop() {
            if !self.seen.insert(tree_id.0) {
                continue;
            }
            let Some(payload) = self.object(at, tree_id.0, ObjectKind::Tree).await? else {
                continue;
            };
            self.report.trees += 1;
            let tree = match Tree::decode(&payload) {
                Ok(tree) => tree,
                Err(_) => {
                    self.report
                        .problem(format!("{tree_id} (in {at}): tree does not decode"));
                    continue;
                }
            };
            for entry in tree.entries {
                match entry.kind {
                    EntryKind::Tree => pending.push(TreeId(entry.id)),
                    // An opaque external pointer, not an object.
                    EntryKind::Submodule => {}
                    _ => {
                        if self.seen.insert(entry.id)
                            && self.object(at, entry.id, ObjectKind::Blob).await?.is_some()
                        {
                            self.report.blobs += 1;
                        }
                    }
                }
            }
        }
        Ok(())
    }

    /// Reads and verifies one object, reporting a missing or corrupt one.
    async fn object(
        &mut self,
        at: ChangesetId,
        id: ObjectId,
        kind: ObjectKind,
    ) -> Result<Option<Vec<u8>>, String> {
        let what = match kind {
            ObjectKind::Tree => graft_model::TreeId(id).to_string(),
            _ => graft_model::BlobId(id).to_string(),
        };
        let bytes = match self.objects.get(self.repo, id).await {
            Ok(Some(bytes)) => bytes,
            Ok(None) => {
                self.report.problem(format!("{what} (in {at}): missing"));
                return Ok(None);
            }
            Err(StoreError::Corrupt(_)) => {
                self.report.problem(format!("{what} (in {at}): corrupt"));
                return Ok(None);
            }
            Err(e) => return Err(store(e)),
        };
        self.report.bytes += bytes.len() as u64;
        let payload = Frame::from_bytes(&bytes)
            .ok()
            .filter(|f| f.kind == kind)
            .and_then(|f| f.decode_verified(id).ok());
        if payload.is_none() {
            self.report.problem(format!("{what} (in {at}): corrupt"));
        }
        Ok(payload)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn created_bodies_parse() {
        let id = ChangesetId(ObjectId([0xab; 32]));
        let body = format!("{{\"id\":\"{id}\",\"state\":\"draft\"}}");
        assert_eq!(created_id(body.as_bytes()), Some(id));
        assert_eq!(created_id(b"{}"), None);
        assert_eq!(created_id(b"not json"), None);
    }
}
