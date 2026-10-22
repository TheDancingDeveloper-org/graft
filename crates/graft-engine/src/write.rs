//! The write path: `CreateRepo`, `CreateChangeset` and `ImportChangesets`
//! (`docs/design.md` §4.5, §6.6, §7.4, §7.9).
//!
//! A changeset is validated, its root derived by path copying, its new
//! trees written to the object store, and then one metadata transaction
//! commits the content, the record, the holder rows, the eager derived rows
//! and the `ChangesetCreated` journal entry. The id commits to content only,
//! so a retry finds the stored changeset and writes nothing.
//!
//! With `workspace`, the new changeset is a draft held by that workspace,
//! and the same transaction moves the workspace head from `parents[0]` to
//! it by CAS: a draft never exists without its holder. Without one, nothing
//! holds it and it is created abandoned (a line move still lands it).

use std::collections::HashSet;

use graft_model::{
    ApplyOptions, Change, Changeset, ChangesetId, CheckOptions, EntryKind, ObjectId, ObjectKind,
    Rule, Tree, TreeId, Violation, apply_changes_with,
};
use graft_proto::types::{CreateChangeset, Created, DraftState, ImportChangesets};
use graft_proto::{ApiError, CallContext, Frame};
use graft_store::{
    ChangesetRecord, MetaStore, MetaTxn, ObjectStore, Precondition, RecordState, RepoId,
    StoreError, TxnOutcome, WorkspaceRecord, Write,
};

use crate::access::{Need, require};
use crate::objects::Upload;
use crate::source::{Trees, empty_tree, model_root};
use crate::workspace::{
    CAS_RETRIES, check_name, contended, guarded, head_conflict, holder, transition,
};
use crate::{Engine, errors, journal};

/// The longest repository name.
pub const MAX_REPO_NAME: usize = 64;

/// Metadata keys under this prefix are the server's (`graft.rebased-from`
/// and friends); `CreateChangeset` refuses them.
const RESERVED_METADATA: &str = "graft.";

/// Checks a repository name: 1 to 64 of `[A-Za-z0-9._-]`, not starting with
/// `.` or `-`. Names become directory names in the object store.
pub(crate) fn check_repo_name(name: &str) -> Result<(), ApiError> {
    let ok = !name.is_empty()
        && name.len() <= MAX_REPO_NAME
        && !name.starts_with(['.', '-'])
        && name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-'));
    if ok {
        Ok(())
    } else {
        Err(ApiError::InvalidPath {
            path: name.to_owned(),
            reason: "repo-name".to_owned(),
        })
    }
}

/// The API state of a stored record. `Collected` never reaches here: the
/// loaders report it as `ApiError::Collected`.
pub(crate) fn draft_state(state: RecordState) -> DraftState {
    match state {
        RecordState::Draft | RecordState::Collected => DraftState::Draft,
        RecordState::Landed => DraftState::Landed,
        RecordState::Superseded => DraftState::Superseded,
        RecordState::Abandoned => DraftState::Abandoned,
    }
}

fn created(id: ChangesetId, root: TreeId, record: &ChangesetRecord) -> Created {
    Created {
        id,
        root,
        lsn: record.lsn,
        state: draft_state(record.state),
    }
}

/// How a new changeset may be parentless (validity rule 2).
#[derive(Clone, Copy, PartialEq, Eq)]
enum Roots {
    /// Only into an empty repository.
    IfEmpty,
    /// Always (admin import).
    Allowed,
}

impl<M: MetaStore, O: ObjectStore> Engine<M, O> {
    pub(crate) async fn do_create_repo(
        &self,
        cx: &CallContext,
        name: String,
    ) -> Result<(), ApiError> {
        check_repo_name(&name)?;
        require(cx, &name, Need::Admin)?;
        match self.meta.create_repo(&name).await {
            // Creating a repository that exists is a successful retry.
            Ok(_) | Err(StoreError::Exists) => Ok(()),
            Err(e) => Err(errors::store(e)),
        }
    }

    pub(crate) async fn do_create_changeset(
        &self,
        cx: &CallContext,
        req: CreateChangeset,
    ) -> Result<Created, ApiError> {
        if req.changes.is_empty() {
            require(cx, &req.repo, Need::Write(None))?;
        }
        for change in &req.changes {
            require(cx, &req.repo, Need::Write(Some(change.path())))?;
        }
        let repo = self.repo_id(&req.repo).await?;
        let hold = match &req.workspace {
            None => None,
            Some(name) => {
                check_name(name)?;
                // Whether the parent is the head is checked once the id is
                // known: a retry finds its draft already at the head.
                let ws = self.live_workspace(cx, &req.repo, repo, name).await?;
                Some(Hold {
                    cx,
                    repo_name: &req.repo,
                    ws,
                })
            }
        };
        if req
            .metadata
            .keys()
            .any(|k| k.starts_with(RESERVED_METADATA))
        {
            return Err(errors::invalid(None, "reserved-metadata"));
        }
        let inline_bytes: u64 = req.inline_blobs.iter().map(|f| f.body.len() as u64).sum();
        if inline_bytes > self.config.max_inline_bytes {
            return Err(errors::limit("inline-bytes", self.config.max_inline_bytes));
        }
        let inline = req
            .inline_blobs
            .iter()
            .map(|f| self.check_blob(f))
            .collect::<Result<Vec<_>, _>>()?;
        let cs = Changeset {
            parents: req.parents,
            root: empty_tree(),
            changes: req.changes,
            author: req.author,
            authored_at: req.authored_at,
            message: req.message,
            metadata: req.metadata,
        };
        self.insert(repo, cs, None, inline, Roots::IfEmpty, hold)
            .await
    }

    /// Checks that a changeset whose first parent is `parent` may go on top
    /// of `ws`: the parent is the head (or the base when there is none), and
    /// the stack stays within `max_stack`.
    async fn check_extends(
        &self,
        repo: RepoId,
        ws: &WorkspaceRecord,
        parent: Option<ChangesetId>,
    ) -> Result<(), ApiError> {
        let Some(parent) = parent else {
            return Err(errors::invalid(None, "workspace-root"));
        };
        if parent != ws.head.unwrap_or(ws.base) {
            return Err(head_conflict(ws));
        }
        if ws.head.is_some() {
            let depth = self
                .stack_depth(repo, ws.base, parent, &mut Vec::new())
                .await?;
            if depth >= u64::from(self.config.max_stack) {
                return Err(errors::limit("stack", self.config.max_stack.into()));
            }
        }
        Ok(())
    }

    /// Re-reads the holding workspace after a lost CAS, and re-checks it.
    async fn reload(
        &self,
        repo: RepoId,
        hold: &mut Hold<'_>,
        parent: Option<ChangesetId>,
    ) -> Result<(), ApiError> {
        hold.ws = self
            .live_workspace(hold.cx, hold.repo_name, repo, &hold.ws.name)
            .await?;
        self.check_extends(repo, &hold.ws, parent).await
    }

    /// A changeset that is already stored, asked for again: a retry, or the
    /// same content committed into a workspace. A workspace adopts it when
    /// nothing holds it.
    async fn existing(
        &self,
        repo: RepoId,
        id: ChangesetId,
        root: TreeId,
        parent: Option<ChangesetId>,
        mut hold: Option<Hold<'_>>,
    ) -> Result<Created, ApiError> {
        for _ in 0..CAS_RETRIES {
            let (_, record) = self.changeset(repo, id).await?;
            let Some(h) = hold.as_mut() else {
                return Ok(created(id, root, &record));
            };
            let rule = match record.state {
                RecordState::Draft
                    if record.holder.as_deref() == Some(holder(&h.ws.name).as_str()) =>
                {
                    return Ok(created(id, root, &record));
                }
                RecordState::Abandoned => None,
                RecordState::Draft => Some("draft-held"),
                RecordState::Landed => Some("draft-landed"),
                RecordState::Superseded => Some("draft-superseded"),
                RecordState::Collected => return Err(ApiError::Collected { id }),
            };
            if let Some(rule) = rule {
                return Err(errors::invalid(None, rule));
            }
            self.check_extends(repo, &h.ws, parent).await?;
            let mut txn = guarded(&h.ws);
            let mut next = h.ws.clone();
            next.head = Some(id);
            let name = h.ws.name.clone();
            transition(
                id,
                record.clone(),
                RecordState::Draft,
                Some(holder(&name)),
                None,
                &mut txn,
            );
            txn.writes.push(Write::PutWorkspace(next));
            txn.writes
                .push(journal::workspace_head_moved(&name, h.ws.head, Some(id)));
            match self.meta.commit(repo, txn).await.map_err(errors::store)? {
                TxnOutcome::Committed(lsns) => {
                    return Ok(Created {
                        id,
                        root,
                        lsn: lsns.first().copied().unwrap_or(record.lsn),
                        state: DraftState::Draft,
                    });
                }
                TxnOutcome::PreconditionFailed(_) => self.reload(repo, h, parent).await?,
            }
        }
        Err(contended())
    }

    pub(crate) async fn do_import_changesets(
        &self,
        cx: &CallContext,
        req: ImportChangesets,
    ) -> Result<Vec<Created>, ApiError> {
        require(cx, &req.repo, Need::Admin)?;
        let repo = self.repo_id(&req.repo).await?;
        if req.changesets.len() > self.config.max_import as usize {
            return Err(errors::limit("import", self.config.max_import.into()));
        }
        let mut out = Vec::with_capacity(req.changesets.len());
        // One transaction per changeset: a failure keeps the ones before it.
        for cs in req.changesets {
            let root = cs.root;
            out.push(
                self.insert(repo, cs, Some(root), Vec::new(), Roots::Allowed, None)
                    .await?,
            );
        }
        Ok(out)
    }

    /// Whether the repository has no changesets yet: its journal is empty
    /// (every changeset writes an entry).
    async fn is_empty(&self, repo: RepoId) -> Result<bool, ApiError> {
        Ok(self
            .meta
            .journal(repo, 1, 1)
            .await
            .map_err(errors::store)?
            .is_empty())
    }

    /// Validates `cs`, derives its root (which must equal `expected` when
    /// given), and stores it: as a draft held by `hold`'s workspace, whose
    /// head moves to it in the same transaction, or abandoned when nothing
    /// holds it. Returns the stored changeset on a retry.
    async fn insert(
        &self,
        repo: RepoId,
        mut cs: Changeset,
        expected: Option<TreeId>,
        inline: Vec<Upload>,
        roots: Roots,
        mut hold: Option<Hold<'_>>,
    ) -> Result<Created, ApiError> {
        // Rules 3, 8 and 9 (and portability); rule 2 waits for the id, so
        // that a retried root changeset still returns its id.
        let opts = CheckOptions {
            allow_root: true,
            max_changes: self.config.max_changes as usize,
            portable_paths: self.config.portable_paths,
        };
        cs.check(&opts).map_err(|v| match v.rule {
            Rule::TooManyChanges => errors::limit("changes", self.config.max_changes.into()),
            _ => errors::violation(v),
        })?;

        // Rule 1: every parent exists here.
        let mut base = None;
        for (i, &parent) in cs.parents.iter().enumerate() {
            match self.changeset_opt(repo, parent).await? {
                None => return Err(ApiError::UnknownParent { id: parent }),
                Some((p, _)) if i == 0 => base = model_root(p.root),
                Some(_) => {}
            }
        }

        // Rule 4: every referenced blob is stored or inline.
        let inline_ids: HashSet<ObjectId> = inline.iter().map(|u| u.id).collect();
        let blobs = put_blobs(&cs.changes);
        let wanted: Vec<ObjectId> = blobs
            .iter()
            .copied()
            .filter(|id| !inline_ids.contains(id))
            .collect();
        if !wanted.is_empty() {
            let present: HashSet<ObjectId> = self
                .objects
                .has(repo, &wanted)
                .await
                .map_err(errors::store)?
                .into_iter()
                .collect();
            let missing: Vec<ObjectId> = wanted
                .into_iter()
                .filter(|id| !present.contains(id))
                .collect();
            if !missing.is_empty() {
                return Err(ApiError::MissingObjects { ids: missing });
            }
        }

        // Rules 5 to 7, and the root, by path copying.
        let trees = Trees::new(&*self.objects, repo);
        let apply = ApplyOptions {
            case_clash: self.config.portable_paths,
        };
        let applied = apply_changes_with(&trees, base, &cs.changes, &apply)
            .await
            .map_err(errors::apply)?;
        let root = applied.root.unwrap_or_else(empty_tree);
        if expected.is_some_and(|e| e != root) {
            return Err(errors::invalid(None, "root-mismatch"));
        }
        cs.root = root;
        let id = cs.id();
        let parent = cs.first_parent();

        if self.changeset_opt(repo, id).await?.is_some() {
            return self.existing(repo, id, root, parent, hold).await;
        }
        if let Some(h) = &hold {
            self.check_extends(repo, &h.ws, parent).await?;
        }
        if cs.parents.is_empty() && roots == Roots::IfEmpty && !self.is_empty(repo).await? {
            return Err(errors::violation(Violation {
                path: None,
                rule: Rule::RootChangeset,
            }));
        }

        // Objects are durable before the metadata that references them.
        self.store_uploads(repo, inline).await?;
        let mut new_trees = applied.new_trees;
        if applied.root.is_none() {
            new_trees.push((root, Tree::default()));
        }
        for (tree_id, tree) in &new_trees {
            let frame = Frame::encode(ObjectKind::Tree, &tree.encode());
            self.objects
                .put(repo, tree_id.0, frame.to_bytes())
                .await
                .map_err(errors::store)?;
        }

        let object_holder = id.to_string();
        let state = match hold {
            Some(_) => RecordState::Draft,
            None => RecordState::Abandoned,
        };
        let mut writes = vec![Write::PutChangeset {
            id,
            content: cs.encode(),
            record: ChangesetRecord {
                lsn: 0,
                received_at_ms: self.clock.now().ms,
                state,
                holder: hold.as_ref().map(|h| holder(&h.ws.name)),
                superseded_by: None,
            },
        }];
        let mut held = HashSet::new();
        let introduced = blobs
            .into_iter()
            .chain(new_trees.iter().map(|(t, _)| t.0))
            .filter(|o| held.insert(*o));
        writes.extend(introduced.map(|object| Write::AddHolder {
            object,
            holder: object_holder.clone(),
        }));
        writes.extend(
            self.index
                .on_create(repo, &cs)
                .await
                .map_err(errors::store)?,
        );

        for _ in 0..CAS_RETRIES {
            let mut txn = MetaTxn {
                preconditions: vec![Precondition::ChangesetAbsent(id)],
                writes: Vec::new(),
            };
            match &hold {
                // Without a workspace there is nothing to retry: move the
                // writes rather than copy them.
                None => txn.writes = std::mem::take(&mut writes),
                Some(h) => {
                    txn.preconditions.extend(guarded(&h.ws).preconditions);
                    txn.writes = writes.clone();
                    let mut next = h.ws.clone();
                    next.head = Some(id);
                    txn.writes.push(Write::PutWorkspace(next));
                }
            }
            let ws_name = hold.as_ref().map(|h| h.ws.name.as_str());
            txn.writes.push(journal::changeset_created(id, ws_name));
            match self.meta.commit(repo, txn).await.map_err(errors::store)? {
                TxnOutcome::Committed(lsns) => {
                    return Ok(Created {
                        id,
                        root,
                        lsn: lsns.first().copied().unwrap_or_default(),
                        state: draft_state(state),
                    });
                }
                // A concurrent retry stored it first.
                TxnOutcome::PreconditionFailed(0) => {
                    return self.existing(repo, id, root, parent, hold).await;
                }
                // The workspace changed: still ours, live, and at the parent?
                TxnOutcome::PreconditionFailed(_) => {
                    let h = hold.as_mut().ok_or_else(contended)?;
                    self.reload(repo, h, parent).await?;
                }
            }
        }
        Err(contended())
    }
}

/// The workspace a new changeset goes into, and who asked.
struct Hold<'a> {
    cx: &'a CallContext,
    repo_name: &'a str,
    ws: WorkspaceRecord,
}

/// The blob ids `changes` put, each once, in order. Submodule ids are opaque
/// external pointers, not objects.
fn put_blobs(changes: &[Change]) -> Vec<ObjectId> {
    let mut seen = HashSet::new();
    changes
        .iter()
        .filter_map(|c| match c {
            Change::Put { kind, id, .. } if *kind != EntryKind::Submodule => Some(*id),
            _ => None,
        })
        .filter(|id| seen.insert(*id))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn repo_names() {
        for ok in [
            "r",
            "graft",
            "rust-torrent",
            "a.b_c",
            &"x".repeat(MAX_REPO_NAME),
        ] {
            assert!(check_repo_name(ok).is_ok(), "{ok}");
        }
        for bad in [
            "",
            ".hidden",
            "-flag",
            "a/b",
            "a:b",
            "sp ace",
            &"x".repeat(MAX_REPO_NAME + 1),
        ] {
            assert!(check_repo_name(bad).is_err(), "{bad}");
        }
    }
}
