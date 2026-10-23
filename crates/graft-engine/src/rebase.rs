//! File-level `Rebase` and `Sync` (`docs/design.md` §7.8, §8.4).
//!
//! A rebase re-applies each draft's changes, relative to its first parent,
//! onto a new parent. For each path a draft changes it compares three
//! entries: the draft's base, the target, and the draft's own:
//!
//! | `base[p]` vs `onto[p]` | `c[p]` vs `onto[p]` | result |
//! | --- | --- | --- |
//! | equal | — | apply `c[p]` |
//! | differ | equal | drop: the change is already there |
//! | differ | differ, all three text files, `merge = line` | line merge |
//! | differ | differ, otherwise | conflict |
//!
//! An entry is its kind and id, so a mode or symlink change is a change, and
//! a directory compares by tree id. A file/directory clash (one side turns
//! `a` into a directory) is a conflict: either the entries at `a` differ, or
//! applying the kept changes to the target breaks validity rule 7, which is
//! reported as a conflict at that path.
//!
//! With `merge = line` (decision D16), a path both sides changed is merged
//! three ways (`graft_model::merge3`) when the base, ours and theirs are all
//! regular or executable files under 1 MiB of text. A clean merge puts the
//! merged blob, taking the executable bit from the side that changed it; a
//! merge whose result is already on the target is dropped. Anything else is a
//! conflict at file level. A rebased changeset records the paths its own
//! rebase merged in [`MERGED_PATHS`], a JSON array of path strings, so the
//! `attested` policy and reviewers can tell a merged result from its draft.
//! Merged blobs stay in memory until the changeset that introduces them is
//! stored, and go in with it, like `CreateChangeset`'s inline blobs. `Sync`
//! always merges by line; `Rebase` and `Land` take the request's mode.
//!
//! A stack rebases bottom-up. The whole stack is planned in memory first,
//! reading only the directories on each changed path, so any conflict fails
//! the rebase before anything is written. A draft whose changes are all
//! already on the target disappears from the stack. A draft whose parent did
//! not move is kept as it is, so a rebase onto an unchanged base is a no-op.
//! Each new changeset keeps its draft's author, time, message and metadata
//! and adds `graft.rebased-from`, so its id depends only on the draft and
//! the target: a repeated rebase returns the same id. New changesets are
//! stored through the ordinary write path, abandoned until a workspace
//! adopts them, so a crash part-way leaves only collectable orphans.
//!
//! Cost: O(changes × depth) tree reads per draft, plus one changeset read
//! per draft, plus the size of any line-merged files. It never depends on
//! the distance between base and target.

use std::borrow::Cow;
use std::collections::{BTreeSet, HashMap};

use graft_model::merge::{MAX_TEXT_BYTES, Merge3, merge3};
use graft_model::{
    ApplyError, ApplyOptions, Change, Changeset, ChangesetId, EntryKind, ObjectId, ObjectKind,
    RepoPath, Rule, TreeEntry, TreeId, apply_changes_with, diff_trees,
};
use graft_proto::error::ConflictPath;
use graft_proto::types::{MergeMode, Rebase, Synced, WorkspaceRef};
use graft_proto::{ApiError, CallContext, Frame};
use graft_store::{MetaStore, ObjectStore, RecordState, RepoId, TxnOutcome, Write};

use crate::access::{Need, require, require_changes};
use crate::objects::Upload;
use crate::source::{Trees, model_root};
use crate::workspace::{CAS_RETRIES, check_name, contended, guarded, held_by, holder, transition};
use crate::{Engine, errors, journal};

/// The metadata key naming the draft a rebased changeset came from.
pub const REBASED_FROM: &str = "graft.rebased-from";

/// The metadata key listing the paths a rebase resolved by line merge
/// (decision D16): a JSON array of path strings, sorted by path bytes.
pub const MERGED_PATHS: &str = "graft.merged-paths";

/// The [`MERGED_PATHS`] value for `paths` (sorted, each once).
pub(crate) fn encode_paths<'a>(paths: impl IntoIterator<Item = &'a RepoPath>) -> String {
    let mut out = String::from("[");
    for (i, path) in paths.into_iter().enumerate() {
        if i > 0 {
            out.push(',');
        }
        journal::json_str(&mut out, path.as_str());
    }
    out.push(']');
    out
}

/// What became of one draft.
pub(crate) struct Step {
    /// The draft.
    pub(crate) from: ChangesetId,
    /// Its successor: the draft itself when its parent did not move, a new
    /// changeset, or `None` when all its changes were already there.
    pub(crate) to: Option<ChangesetId>,
}

/// A planned rebase: nothing is written yet.
pub(crate) struct Plan {
    /// One step per draft, bottom first.
    pub(crate) steps: Vec<Step>,
    /// The changesets to store, bottom first.
    pub(crate) new: Vec<Changeset>,
    /// The new top: the last successor, or the target when every draft
    /// disappeared.
    pub(crate) head: ChangesetId,
    /// Its snapshot root.
    pub(crate) head_root: TreeId,
    /// The payloads of the blobs line merge made, by id. Each is stored
    /// with the first new changeset that puts it.
    pub(crate) blobs: HashMap<ObjectId, Vec<u8>>,
    /// Every path resolved by line merge, sorted, each once.
    pub(crate) merged: Vec<RepoPath>,
}

impl Plan {
    /// The merged blobs `cs` puts, as uploads to store with it.
    pub(crate) fn uploads(&self, cs: &Changeset) -> Vec<Upload> {
        let mut out: Vec<Upload> = Vec::new();
        for change in &cs.changes {
            if let Change::Put { id, .. } = change
                && let Some(payload) = self.blobs.get(id)
                && !out.iter().any(|u| u.id == *id)
            {
                out.push(Upload {
                    id: *id,
                    bytes: Frame::encode(ObjectKind::Blob, payload).to_bytes(),
                });
            }
        }
        out
    }
}

/// A text input of a line merge: a pending merged blob or a stored one.
type Text<'a> = Cow<'a, [u8]>;

/// Whether an entry kind can be merged by line.
fn mergeable(kind: EntryKind) -> bool {
    matches!(kind, EntryKind::File | EntryKind::Exec)
}

/// A changeset entry as the rebase compares it. Changes are file-level, so
/// a directory counts as no entry: a draft that deletes file `a` matches a
/// target that turned `a` into a directory. A real clash is found when the
/// kept changes are applied.
pub(crate) fn key(e: &Option<TreeEntry>) -> Option<(graft_model::EntryKind, ObjectId)> {
    e.as_ref()
        .filter(|e| e.kind != graft_model::EntryKind::Tree)
        .map(|e| (e.kind, e.id))
}

/// What a change leaves at its path.
fn change_key(c: &Change) -> Option<(graft_model::EntryKind, ObjectId)> {
    match c {
        Change::Put { kind, id, .. } => Some((*kind, *id)),
        Change::Delete { .. } => None,
    }
}

fn conflict(paths: Vec<ConflictPath>, onto: ChangesetId) -> ApiError {
    ApiError::Conflict { paths, onto }
}

impl<M: MetaStore, O: ObjectStore> Engine<M, O> {
    async fn entry(
        &self,
        trees: &Trees<'_, O>,
        root: TreeId,
        path: &RepoPath,
    ) -> Result<Option<TreeEntry>, ApiError> {
        trees.lookup(root, path).await.map_err(errors::source)
    }

    /// The text of blob `id` for a line merge: a merged blob of this plan,
    /// or the stored blob. `None` when it is too large to merge by line,
    /// which the frame header tells without decoding it.
    async fn merge_input<'p>(
        &self,
        repo: RepoId,
        id: ObjectId,
        pending: &'p HashMap<ObjectId, Vec<u8>>,
    ) -> Result<Option<Text<'p>>, ApiError> {
        if let Some(payload) = pending.get(&id) {
            return Ok(Some(Cow::Borrowed(payload)));
        }
        let Some(bytes) = self.objects.get(repo, id).await.map_err(errors::store)? else {
            return Err(ApiError::MissingObjects { ids: vec![id] });
        };
        let frame = Frame::from_bytes(&bytes)
            .ok()
            .filter(|f| f.kind == ObjectKind::Blob)
            .ok_or(ApiError::CorruptObject { id })?;
        if frame.uncompressed_len >= MAX_TEXT_BYTES as u64 {
            return Ok(None);
        }
        let payload = frame
            .decode_verified(id)
            .map_err(|_| ApiError::CorruptObject { id })?;
        Ok(Some(Cow::Owned(payload)))
    }

    /// The line merge of a path both sides changed (decision D16): the
    /// merged entry and its payload, or `None` when it conflicts. All three
    /// entries must be regular or executable files of text; the result
    /// takes the kind from the side that changed it.
    async fn line_merge(
        &self,
        repo: RepoId,
        base: Option<(EntryKind, ObjectId)>,
        ours: Option<(EntryKind, ObjectId)>,
        theirs: Option<(EntryKind, ObjectId)>,
        pending: &HashMap<ObjectId, Vec<u8>>,
    ) -> Result<Option<(EntryKind, ObjectId, Vec<u8>)>, ApiError> {
        let (Some(b), Some(o), Some(t)) = (base, ours, theirs) else {
            return Ok(None);
        };
        if !(mergeable(b.0) && mergeable(o.0) && mergeable(t.0)) {
            return Ok(None);
        }
        // Two kinds only: when theirs changed the kind, ours either kept
        // the base's or made the same change.
        let kind = if b.0 == t.0 { o.0 } else { t.0 };
        let Some(base) = self.merge_input(repo, b.1, pending).await? else {
            return Ok(None);
        };
        let Some(ours) = self.merge_input(repo, o.1, pending).await? else {
            return Ok(None);
        };
        let Some(theirs) = self.merge_input(repo, t.1, pending).await? else {
            return Ok(None);
        };
        match merge3(&base, &ours, &theirs) {
            Merge3::Clean(merged) => {
                let id = ObjectId::hash(ObjectKind::Blob, &merged);
                Ok(Some((kind, id, merged)))
            }
            Merge3::Conflicts(_) | Merge3::NotText | Merge3::TooComplex => Ok(None),
        }
    }

    /// Plans the rebase of `stack` (bottom first; each draft's first parent
    /// is the one below it) onto `onto`. Reads only; fails with `Conflict`
    /// at the first draft that conflicts.
    pub(crate) async fn plan(
        &self,
        repo: RepoId,
        stack: &[(ChangesetId, Changeset)],
        onto: ChangesetId,
        mode: MergeMode,
    ) -> Result<Plan, ApiError> {
        let onto_root = self.root_of(repo, onto).await?;
        self.plan_onto(repo, stack, onto, onto_root, mode).await
    }

    /// [`Engine::plan`] onto a changeset whose root is `onto_root`, which
    /// need not be stored yet (a squashed draft). Each draft's own first
    /// parent must be stored or in `stack`.
    pub(crate) async fn plan_onto(
        &self,
        repo: RepoId,
        stack: &[(ChangesetId, Changeset)],
        onto: ChangesetId,
        onto_root: TreeId,
        mode: MergeMode,
    ) -> Result<Plan, ApiError> {
        let trees = Trees::new(&*self.objects, repo);
        let apply = ApplyOptions {
            case_clash: self.config.portable_paths,
        };
        let mut plan = Plan {
            steps: Vec::with_capacity(stack.len()),
            new: Vec::new(),
            head: onto,
            head_root: onto_root,
            blobs: HashMap::new(),
            merged: Vec::new(),
        };
        let mut merged_all: BTreeSet<RepoPath> = BTreeSet::new();
        // The roots of the drafts below, so a stacked draft's base costs no
        // read.
        let mut roots: HashMap<ChangesetId, TreeId> = HashMap::new();
        for (id, c) in stack {
            let base = c
                .first_parent()
                .ok_or_else(|| errors::invalid(None, "rebase-root"))?;
            if base == plan.head {
                // Its parent did not move: keep it as it is.
                plan.steps.push(Step {
                    from: *id,
                    to: Some(*id),
                });
                plan.head = *id;
                plan.head_root = c.root;
                roots.insert(*id, c.root);
                continue;
            }
            let base_root = match roots.get(&base) {
                Some(root) => *root,
                None => self.root_of(repo, base).await?,
            };
            roots.insert(*id, c.root);

            let mut kept = Vec::with_capacity(c.changes.len());
            let mut conflicts = Vec::new();
            let mut merged = Vec::new();
            for change in &c.changes {
                let path = change.path();
                let b = self.entry(&trees, base_root, path).await?;
                let o = self.entry(&trees, plan.head_root, path).await?;
                let (b, o, ours) = (key(&b), key(&o), change_key(change));
                if b == o {
                    kept.push(change.clone());
                } else if ours == o {
                    // The change is already there: drop it.
                } else if let Some((kind, id, payload)) = match mode {
                    MergeMode::Line => self.line_merge(repo, b, ours, o, &plan.blobs).await?,
                    MergeMode::File => None,
                } {
                    // A merge whose result is already there is dropped.
                    if Some((kind, id)) != o {
                        kept.push(Change::Put {
                            path: path.clone(),
                            kind,
                            id,
                        });
                        plan.blobs.entry(id).or_insert(payload);
                    }
                    merged.push(path.clone());
                } else {
                    conflicts.push(ConflictPath {
                        path: path.to_string(),
                        base: b.map(|(_, id)| id),
                        ours: ours.map(|(_, id)| id),
                        theirs: o.map(|(_, id)| id),
                    });
                }
            }
            if !conflicts.is_empty() {
                return Err(conflict(conflicts, onto));
            }
            merged_all.extend(merged.iter().cloned());
            if kept.is_empty() {
                plan.steps.push(Step {
                    from: *id,
                    to: None,
                });
                continue;
            }
            let applied =
                match apply_changes_with(&trees, model_root(plan.head_root), &kept, &apply).await {
                    Ok(applied) => applied,
                    // One side made a path a directory where the other has a
                    // file (or the reverse).
                    Err(ApplyError::Invalid(v)) if v.rule == Rule::FileAndDirectory => {
                        let Some(path) = v.path else {
                            return Err(errors::violation(v));
                        };
                        let b = self.entry(&trees, base_root, &path).await?;
                        let ours = self.entry(&trees, c.root, &path).await?;
                        let theirs = self.entry(&trees, plan.head_root, &path).await?;
                        return Err(conflict(
                            vec![ConflictPath {
                                path: path.to_string(),
                                base: b.map(|e| e.id),
                                ours: ours.map(|e| e.id),
                                theirs: theirs.map(|e| e.id),
                            }],
                            onto,
                        ));
                    }
                    Err(e) => return Err(errors::apply(e)),
                };
            let root = applied.root.unwrap_or_else(crate::source::empty_tree);
            trees.remember(&applied.new_trees);

            let mut parents = Vec::with_capacity(c.parents.len());
            parents.push(plan.head);
            parents.extend(c.parents.iter().skip(1).copied());
            let mut metadata = c.metadata.clone();
            metadata.insert(REBASED_FROM.to_owned(), id.to_string());
            // The paths this rebase merged, not an earlier one's: a later
            // file-level rebase of a merged result inherits its attestations.
            if merged.is_empty() {
                metadata.remove(MERGED_PATHS);
            } else {
                metadata.insert(MERGED_PATHS.to_owned(), encode_paths(&merged));
            }
            let cs = Changeset {
                parents,
                root,
                changes: kept,
                author: c.author.clone(),
                authored_at: c.authored_at,
                message: c.message.clone(),
                metadata,
            };
            let new_id = cs.id();
            plan.steps.push(Step {
                from: *id,
                to: Some(new_id),
            });
            plan.head = new_id;
            plan.head_root = root;
            plan.new.push(cs);
        }
        plan.merged = merged_all.into_iter().collect();
        Ok(plan)
    }

    /// Stores a plan's new changesets, bottom first, each abandoned until a
    /// workspace adopts it. A changeset already stored (a retry) is kept.
    pub(crate) async fn store_plan(&self, repo: RepoId, plan: &Plan) -> Result<(), ApiError> {
        for cs in &plan.new {
            self.insert_unheld(repo, cs.clone(), plan.uploads(cs))
                .await?;
        }
        Ok(())
    }

    /// The drafts `Rebase` moves for `head`: its first-parent chain down to,
    /// not including, the first landed changeset or `onto`, bottom first.
    /// `head` itself is always moved. At most `max_stack` drafts.
    pub(crate) async fn chain(
        &self,
        repo: RepoId,
        head: ChangesetId,
        onto: ChangesetId,
    ) -> Result<Vec<(ChangesetId, Changeset)>, ApiError> {
        let mut out = Vec::new();
        let mut id = head;
        let (mut cs, _) = self.changeset(repo, id).await?;
        loop {
            if out.len() >= self.config.max_stack as usize {
                return Err(errors::limit("stack", self.config.max_stack.into()));
            }
            let parent = cs.first_parent();
            out.push((id, cs));
            let Some(parent) = parent else {
                break;
            };
            if parent == onto {
                break;
            }
            let (pcs, record) = self.changeset(repo, parent).await?;
            if record.state == RecordState::Landed {
                break;
            }
            id = parent;
            cs = pcs;
        }
        out.reverse();
        Ok(out)
    }

    pub(crate) async fn do_rebase(
        &self,
        cx: &CallContext,
        req: Rebase,
    ) -> Result<ChangesetId, ApiError> {
        require(cx, &req.repo, Need::Read)?;
        let repo = self.repo_id(&req.repo).await?;
        // The target exists and is not collected.
        self.changeset(repo, req.onto).await?;
        let stack = self.chain(repo, req.changeset, req.onto).await?;
        for (_, cs) in &stack {
            require_changes(cx, &req.repo, &cs.changes)?;
        }
        let plan = self.plan(repo, &stack, req.onto, req.merge).await?;
        self.store_plan(repo, &plan).await?;
        Ok(plan.head)
    }

    /// The changeset a workspace syncs onto: `onto` when it is on the
    /// workspace's line, otherwise the line's tip.
    async fn sync_target(
        &self,
        repo: RepoId,
        line: &str,
        onto: Option<ChangesetId>,
    ) -> Result<ChangesetId, ApiError> {
        let tip = self
            .meta
            .get_ref(repo, line)
            .await
            .map_err(errors::store)?
            .ok_or_else(|| errors::not_found(format!("ref {line}")))?
            .target;
        match onto {
            None => Ok(tip),
            Some(at) if at == tip => Ok(at),
            Some(at) => {
                let on_line = self
                    .meta
                    .line_pos(repo, line, at)
                    .await
                    .map_err(errors::store)?;
                if on_line.is_none() {
                    return Err(errors::invalid(None, "onto-not-on-line"));
                }
                Ok(at)
            }
        }
    }

    /// Moves a workspace's base to `onto` (by default its line's tip) and
    /// rebases its stack there, in one CAS on the workspace. Returns the
    /// workspace and the paths whose content differs between the old and
    /// the new top (at most `max_sync_paths`).
    pub(crate) async fn do_sync(
        &self,
        cx: &CallContext,
        wref: WorkspaceRef,
        onto: Option<ChangesetId>,
    ) -> Result<Synced, ApiError> {
        require(cx, &wref.repo, Need::Workspace)?;
        check_name(&wref.name)?;
        let repo = self.repo_id(&wref.repo).await?;
        for _ in 0..CAS_RETRIES {
            let ws = self
                .live_workspace(cx, &wref.repo, repo, &wref.name)
                .await?;
            let target = self.sync_target(repo, &ws.line, onto).await?;
            if target == ws.base {
                return Ok(Synced {
                    workspace: crate::workspace::view(ws),
                    changed_paths: Vec::new(),
                });
            }
            let mut txn = guarded(&ws);
            let mut stack = self
                .stack_content(repo, ws.base, ws.head, &mut txn.writes)
                .await?;
            stack.reverse();
            let mut records = HashMap::with_capacity(stack.len());
            let mut drafts = Vec::with_capacity(stack.len());
            for (id, cs, record) in stack {
                require_changes(cx, &wref.repo, &cs.changes)?;
                records.insert(id, record);
                drafts.push((id, cs));
            }
            // Decision D16: a sync merges by line.
            let plan = self.plan(repo, &drafts, target, MergeMode::Line).await?;

            self.store_plan(repo, &plan).await?;

            // What the client must update: the old top against the new.
            let old_top = ws.head.unwrap_or(ws.base);
            let old_root = match drafts.last() {
                Some((_, cs)) => cs.root,
                None => self.root_of(repo, old_top).await?,
            };
            let trees = Trees::new(&*self.objects, repo);
            let changed = diff_trees(
                &trees,
                model_root(old_root),
                model_root(plan.head_root),
                self.config.max_sync_paths as usize,
            )
            .await
            .map_err(errors::source)?;
            let changed_paths: Vec<RepoPath> =
                changed.changes.iter().map(|c| c.path().clone()).collect();

            // Adopt the new drafts; supersede the ones they replace. A
            // draft that disappeared is superseded by the successor below
            // it, or by the target itself.
            let mut below = target;
            let expires = self.retained_until(self.clock.now().ms);
            for step in &plan.steps {
                let record = records.remove(&step.from).ok_or_else(contended)?;
                match step.to {
                    Some(to) if to == step.from => {}
                    Some(to) => {
                        let (_, new_record) = self.changeset(repo, to).await?;
                        let rule = match new_record.state {
                            RecordState::Abandoned => None,
                            RecordState::Draft if held_by(&new_record, &ws.name) => None,
                            RecordState::Draft => Some("draft-held"),
                            RecordState::Landed => Some("draft-landed"),
                            RecordState::Superseded => Some("draft-superseded"),
                            RecordState::Collected => {
                                return Err(ApiError::Collected { id: to });
                            }
                        };
                        if let Some(rule) = rule {
                            return Err(errors::invalid(None, rule));
                        }
                        if new_record.state == RecordState::Abandoned {
                            transition(
                                to,
                                new_record,
                                RecordState::Draft,
                                Some(holder(&ws.name)),
                                None,
                                None,
                                &mut txn,
                            );
                        }
                        if held_by(&record, &ws.name) {
                            transition(
                                step.from,
                                record,
                                RecordState::Superseded,
                                None,
                                Some(to),
                                expires,
                                &mut txn,
                            );
                        }
                    }
                    None => {
                        if held_by(&record, &ws.name) {
                            transition(
                                step.from,
                                record,
                                RecordState::Superseded,
                                None,
                                Some(below),
                                expires,
                                &mut txn,
                            );
                        }
                    }
                }
                if let Some(to) = step.to {
                    below = to;
                }
            }

            let mut next = ws.clone();
            next.base = target;
            next.head = (plan.head != target).then_some(plan.head);
            txn.writes.push(Write::PutWorkspace(next.clone()));
            txn.writes.push(journal::workspace_synced(
                &ws.name, ws.base, next.base, ws.head, next.head,
            ));
            if let TxnOutcome::Committed(_) =
                self.meta.commit(repo, txn).await.map_err(errors::store)?
            {
                return Ok(Synced {
                    workspace: crate::workspace::view(next),
                    changed_paths,
                });
            }
        }
        Err(contended())
    }
}
