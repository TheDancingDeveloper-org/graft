//! `Stack` operations on a workspace's drafts (`docs/design.md` §7.2, §8.5).
//!
//! Each operation edits the stack (the drafts between the workspace's base
//! and head, bottom first) on the server, then moves the head by CAS:
//!
//! - `reorder` replays the drafts in the given order, each draft's changes
//!   relative to its own first parent, as a rebase does;
//! - `squash` turns adjacent drafts into one: the bottom's parent, author
//!   and time, the top's snapshot, their messages joined;
//! - `split` turns one draft into one draft per path group, in group order.
//!   Each change goes to the first group with a path that is it or a
//!   directory above it; every change needs a group and every group a
//!   change;
//! - `drop` removes a draft.
//!
//! The drafts above an edit are rebased onto it with line merge (decision
//! D16), which is a plain re-parenting when the edit kept the snapshot
//! below them (`squash`, `split`). A conflict fails the operation and writes
//! nothing to the workspace. New drafts carry none of the server's
//! `graft.` metadata, which describes how their predecessors were made.
//!
//! New changesets are stored abandoned first, then one transaction guarded
//! by the workspace adopts them (reviving one that an earlier edit
//! abandoned or superseded), marks each replaced draft superseded by its
//! successor (a dropped draft abandoned), and moves the head. A crash in
//! between leaves only collectable orphans. A retried operation that already
//! committed names drafts no longer in the stack, and fails with
//! `not-in-stack`.
//!
//! Cost: the drafts above the edit are rebased (O(changes × depth) tree
//! reads); a squash diffs two snapshots, recursing only where they differ.

use std::collections::{HashMap, HashSet};

use graft_model::{
    ApplyOptions, Change, Changeset, ChangesetId, Metadata, RepoPath, TreeId, apply_changes_with,
    diff_trees,
};
use graft_proto::types::{MergeMode, StackOp, WorkspaceRef, WorkspaceView};
use graft_proto::{ApiError, CallContext};
use graft_store::{MetaStore, ObjectStore, RecordState, RepoId, TxnOutcome, Write};

use crate::access::{Need, require, require_changes};
use crate::objects::Upload;
use crate::source::{Trees, model_root};
use crate::workspace::{
    CAS_RETRIES, check_name, contended, guarded, held_by, holder, set_draft_paths, transition,
};
use crate::{Engine, errors, journal};

/// What became of a draft.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Fate {
    /// Unchanged, still in the stack.
    Kept,
    /// Replaced by this changeset.
    By(ChangesetId),
    /// Removed from the stack.
    Dropped,
}

/// A planned stack edit: nothing is written yet.
struct Edit {
    /// The changesets to store, bottom first, with the blobs they bring.
    new: Vec<(Changeset, Vec<Upload>)>,
    /// What became of each draft, bottom first.
    fates: Vec<(ChangesetId, Fate)>,
    /// The new top: the base when the stack is empty.
    head: ChangesetId,
}

impl Edit {
    fn fate(&mut self, from: ChangesetId, to: ChangesetId) {
        let fate = if from == to { Fate::Kept } else { Fate::By(to) };
        self.fates.push((from, fate));
    }
}

fn stack_rule(rule: &'static str) -> ApiError {
    errors::invalid(None, rule)
}

/// Metadata without the server's `graft.` keys.
fn own_metadata(metadata: &Metadata) -> Metadata {
    metadata
        .iter()
        .filter(|(k, _)| !k.starts_with("graft."))
        .map(|(k, v)| (k.clone(), v.clone()))
        .collect()
}

/// The index of draft `id` in `drafts`.
fn position(drafts: &[(ChangesetId, Changeset)], id: ChangesetId) -> Result<usize, ApiError> {
    drafts
        .iter()
        .position(|(d, _)| *d == id)
        .ok_or_else(|| stack_rule("not-in-stack"))
}

/// The group a split puts `path` in: the first with a path that is it or a
/// directory above it.
fn group_of(groups: &[Vec<RepoPath>], path: &RepoPath) -> Option<usize> {
    groups
        .iter()
        .position(|g| g.iter().any(|prefix| path.starts_with(prefix)))
}

impl<M: MetaStore, O: ObjectStore> Engine<M, O> {
    /// The snapshot below draft `i`: the draft under it, or the base.
    async fn root_below(
        &self,
        repo: RepoId,
        base: ChangesetId,
        drafts: &[(ChangesetId, Changeset)],
        i: usize,
    ) -> Result<(ChangesetId, TreeId), ApiError> {
        match i.checked_sub(1) {
            Some(below) => Ok((drafts[below].0, drafts[below].1.root)),
            None => Ok((base, self.root_of(repo, base).await?)),
        }
    }

    /// Rebases `above` onto `onto` (whose root is `onto_root`) into `edit`.
    async fn rebase_above(
        &self,
        repo: RepoId,
        above: &[(ChangesetId, Changeset)],
        onto: ChangesetId,
        onto_root: TreeId,
        edit: &mut Edit,
    ) -> Result<(), ApiError> {
        let plan = self
            .plan_onto(repo, above, onto, onto_root, MergeMode::Line)
            .await?;
        let mut below = onto;
        for step in &plan.steps {
            let to = step.to.unwrap_or(below);
            edit.fate(step.from, to);
            below = to;
        }
        for cs in &plan.new {
            edit.new.push((cs.clone(), plan.uploads(cs)));
        }
        edit.head = plan.head;
        Ok(())
    }

    /// Plans `op` on the stack `drafts` (bottom first) over `base`.
    async fn edit(
        &self,
        repo: RepoId,
        base: ChangesetId,
        drafts: &[(ChangesetId, Changeset)],
        op: &StackOp,
    ) -> Result<Edit, ApiError> {
        let mut edit = Edit {
            new: Vec::new(),
            fates: Vec::with_capacity(drafts.len()),
            head: base,
        };
        let keep_below = |edit: &mut Edit, i: usize| {
            edit.fates
                .extend(drafts[..i].iter().map(|(id, _)| (*id, Fate::Kept)));
        };
        match op {
            StackOp::Reorder(order) => {
                let by_id: HashMap<ChangesetId, &Changeset> =
                    drafts.iter().map(|(id, cs)| (*id, cs)).collect();
                let mut seen = HashSet::new();
                let mut reordered = Vec::with_capacity(order.len());
                for id in order {
                    let cs = by_id.get(id).ok_or_else(|| stack_rule("not-in-stack"))?;
                    if !seen.insert(*id) {
                        return Err(stack_rule("stack-order"));
                    }
                    reordered.push((*id, (*cs).clone()));
                }
                if reordered.len() != drafts.len() {
                    return Err(stack_rule("stack-order"));
                }
                let base_root = self.root_of(repo, base).await?;
                self.rebase_above(repo, &reordered, base, base_root, &mut edit)
                    .await?;
            }
            StackOp::Squash(ids) => {
                let first = *ids.first().ok_or_else(|| stack_rule("stack-squash"))?;
                let i = position(drafts, first)?;
                let j = i + ids.len() - 1;
                let adjacent = drafts
                    .get(i..=j)
                    .is_some_and(|run| run.iter().zip(ids).all(|((d, _), id)| d == id));
                if !adjacent {
                    return Err(stack_rule("stack-squash"));
                }
                keep_below(&mut edit, i);
                let (parent, parent_root) = self.root_below(repo, base, drafts, i).await?;
                let (bottom, top) = (&drafts[i].1, &drafts[j].1);
                let trees = Trees::new(&*self.objects, repo);
                let max = self.config.max_changes as usize;
                let diff = diff_trees(&trees, model_root(parent_root), model_root(top.root), max)
                    .await
                    .map_err(errors::source)?;
                if diff.next.is_some() {
                    return Err(errors::limit("changes", self.config.max_changes.into()));
                }
                let (onto, onto_root) = if diff.changes.is_empty() {
                    // The drafts cancel out: the run disappears.
                    for (id, _) in &drafts[i..=j] {
                        edit.fate(*id, parent);
                    }
                    (parent, parent_root)
                } else {
                    let mut metadata = Metadata::new();
                    let mut messages = Vec::new();
                    for (_, cs) in &drafts[i..=j] {
                        metadata.extend(own_metadata(&cs.metadata));
                        let message = cs.message.trim_end();
                        if !message.is_empty() {
                            messages.push(message);
                        }
                    }
                    let squashed = Changeset {
                        parents: vec![parent],
                        root: top.root,
                        changes: diff.changes,
                        author: bottom.author.clone(),
                        authored_at: bottom.authored_at,
                        message: messages.join("\n\n"),
                        metadata,
                    };
                    let id = squashed.id();
                    for (old, _) in &drafts[i..=j] {
                        edit.fate(*old, id);
                    }
                    edit.new.push((squashed, Vec::new()));
                    (id, top.root)
                };
                edit.head = onto;
                self.rebase_above(repo, &drafts[j + 1..], onto, onto_root, &mut edit)
                    .await?;
            }
            StackOp::Split { draft, groups } => {
                let i = position(drafts, *draft)?;
                if groups.is_empty() {
                    return Err(stack_rule("stack-split"));
                }
                let total = drafts.len() - 1 + groups.len();
                if total > self.config.max_stack as usize {
                    return Err(errors::limit("stack", self.config.max_stack.into()));
                }
                let c = &drafts[i].1;
                let mut parts: Vec<Vec<Change>> = vec![Vec::new(); groups.len()];
                for change in &c.changes {
                    let g = group_of(groups, change.path())
                        .ok_or_else(|| stack_rule("stack-split-uncovered"))?;
                    parts[g].push(change.clone());
                }
                if parts.iter().any(Vec::is_empty) {
                    return Err(stack_rule("stack-split-empty"));
                }
                keep_below(&mut edit, i);
                let (mut prev, mut prev_root) = self.root_below(repo, base, drafts, i).await?;
                let trees = Trees::new(&*self.objects, repo);
                let apply = ApplyOptions {
                    case_clash: self.config.portable_paths,
                };
                for changes in parts {
                    let applied =
                        apply_changes_with(&trees, model_root(prev_root), &changes, &apply)
                            .await
                            .map_err(errors::apply)?;
                    trees.remember(&applied.new_trees);
                    let root = applied.root.unwrap_or_else(crate::source::empty_tree);
                    let part = Changeset {
                        parents: vec![prev],
                        root,
                        changes,
                        author: c.author.clone(),
                        authored_at: c.authored_at,
                        message: c.message.clone(),
                        metadata: own_metadata(&c.metadata),
                    };
                    prev = part.id();
                    prev_root = root;
                    edit.new.push((part, Vec::new()));
                }
                if prev_root != c.root {
                    return Err(stack_rule("stack-split-root"));
                }
                edit.fate(*draft, prev);
                edit.head = prev;
                self.rebase_above(repo, &drafts[i + 1..], prev, prev_root, &mut edit)
                    .await?;
            }
            StackOp::Drop(id) => {
                let i = position(drafts, *id)?;
                keep_below(&mut edit, i);
                edit.fates.push((*id, Fate::Dropped));
                let (below, below_root) = self.root_below(repo, base, drafts, i).await?;
                edit.head = below;
                self.rebase_above(repo, &drafts[i + 1..], below, below_root, &mut edit)
                    .await?;
            }
            _ => return Err(stack_rule("stack-op")),
        }
        Ok(edit)
    }

    pub(crate) async fn do_stack(
        &self,
        cx: &CallContext,
        wref: WorkspaceRef,
        op: StackOp,
    ) -> Result<WorkspaceView, ApiError> {
        require(cx, &wref.repo, Need::Workspace)?;
        check_name(&wref.name)?;
        let repo = self.repo_id(&wref.repo).await?;
        for _ in 0..CAS_RETRIES {
            let ws = self
                .live_workspace(cx, &wref.repo, repo, &wref.name)
                .await?;
            let mut txn = guarded(&ws);
            let mut stack = self
                .stack_content(repo, ws.base, ws.head, &mut txn.writes)
                .await?;
            stack.reverse();
            let mut records = HashMap::with_capacity(stack.len());
            let mut drafts = Vec::with_capacity(stack.len());
            for (id, cs, record) in stack {
                records.insert(id, record);
                drafts.push((id, cs));
            }
            let edit = self.edit(repo, ws.base, &drafts, &op).await?;
            for (cs, _) in &edit.new {
                require_changes(cx, &wref.repo, &cs.changes)?;
            }
            // The new stack: the drafts kept as they were, and the new ones.
            let kept: HashSet<ChangesetId> = edit
                .fates
                .iter()
                .filter(|(_, fate)| matches!(fate, Fate::Kept))
                .map(|(id, _)| *id)
                .collect();
            let paths = set_draft_paths(
                &ws,
                drafts
                    .iter()
                    .filter(|(id, _)| kept.contains(id))
                    .map(|(_, cs)| cs)
                    .chain(edit.new.iter().map(|(cs, _)| cs)),
            );
            let mut adopt = Vec::with_capacity(edit.new.len());
            for (cs, uploads) in edit.new {
                adopt.push(self.insert_unheld(repo, cs, uploads).await?.id);
            }

            // Adopt the new drafts; supersede (or abandon) the ones they
            // replace. The server chose their content, so one can be an
            // earlier draft rebuilt exactly (a reorder that undoes itself,
            // then a squash that strips `graft.` metadata): it is revived,
            // superseded or not.
            for id in adopt {
                let (_, record) = self.changeset(repo, id).await?;
                let rule = match record.state {
                    RecordState::Abandoned | RecordState::Superseded => None,
                    RecordState::Draft if held_by(&record, &ws.name) => continue,
                    RecordState::Draft => Some("draft-held"),
                    RecordState::Landed => Some("draft-landed"),
                    RecordState::Collected => return Err(ApiError::Collected { id }),
                };
                if let Some(rule) = rule {
                    return Err(errors::invalid(None, rule));
                }
                transition(
                    id,
                    record,
                    RecordState::Draft,
                    Some(holder(&ws.name)),
                    None,
                    None,
                    &mut txn,
                );
            }
            let expires = self.retained_until(self.clock.now().ms);
            for (id, fate) in &edit.fates {
                let record = records.remove(id).ok_or_else(contended)?;
                if !held_by(&record, &ws.name) {
                    continue;
                }
                match fate {
                    Fate::Kept => {}
                    Fate::By(to) => transition(
                        *id,
                        record,
                        RecordState::Superseded,
                        None,
                        Some(*to),
                        expires,
                        &mut txn,
                    ),
                    Fate::Dropped => transition(
                        *id,
                        record,
                        RecordState::Abandoned,
                        None,
                        None,
                        expires,
                        &mut txn,
                    ),
                }
            }

            let mut next = ws.clone();
            next.head = (edit.head != ws.base).then_some(edit.head);
            if next.head != ws.head {
                txn.writes.push(Write::PutWorkspace(next.clone()));
                txn.writes.push(paths);
                txn.writes
                    .push(journal::workspace_head_moved(&ws.name, ws.head, next.head));
            }
            if let TxnOutcome::Committed(_) =
                self.meta.commit(repo, txn).await.map_err(errors::store)?
            {
                return Ok(crate::workspace::view(next));
            }
        }
        Err(contended())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn p(s: &str) -> RepoPath {
        RepoPath::new(s).expect("path")
    }

    #[test]
    fn split_groups_take_the_first_covering_prefix() {
        let groups = vec![vec![p("src/a")], vec![p("src"), p("docs")]];
        assert_eq!(group_of(&groups, &p("src/a/x.rs")), Some(0));
        assert_eq!(group_of(&groups, &p("src/b.rs")), Some(1));
        assert_eq!(group_of(&groups, &p("docs/x.md")), Some(1));
        assert_eq!(group_of(&groups, &p("srcs/x")), None);
        assert_eq!(group_of(&groups, &p("README")), None);
    }

    #[test]
    fn new_drafts_drop_the_servers_metadata() {
        let mut m = Metadata::new();
        m.insert("graft.rebased-from".to_owned(), "x".to_owned());
        m.insert("graft.merged-paths".to_owned(), "[]".to_owned());
        m.insert("vogt.session".to_owned(), "ses_1".to_owned());
        let own = own_metadata(&m);
        assert_eq!(own.len(), 1);
        assert_eq!(own["vogt.session"], "ses_1");
    }
}
