//! Merges between lines (D29, `docs/design.md` §8.9).
//!
//! A merge lands on the target line as an ordinary one-parent changeset;
//! what makes it a merge is a **merge point**, one `line_merges` row written
//! in the landing's CAS transaction with its `LineMerged` journal entry.
//! `MergeBase` between two lines reads the merge points (and fork rows)
//! before it falls back to the ancestry walk.
//!
//! Containment (§8.9): a line `L`, up to position `b`, contains a point
//! `(M, p)` (changeset `c`) when
//!
//! 1. and 2. `c` is on `L`'s line index at or below `b`, following forks
//!    (one `line_pos` read);
//! 2. (the rest) `L` forked from `M'` at `q` and `M'` up to `min(b, q)`
//!    contains it through its own merge rows;
//! 3. `L` has a merge row at or below `b` from `S` at `s`, and `S` up to `s`
//!    contains it;
//! 4. `(M, p)` is itself a merge landing from `(S, s)`, and `L` contains
//!    both `(M, p - 1)` and `(S, s)`.
//!
//! Each rule is an indexed read "at or below a position". A search follows
//! at most `merge_hops` lines deep and spends at most `merge_reads` reads;
//! a capped search answers "not contained", and `MergeBase` then falls
//! back to the ancestry walk, saying so (`via`).
//!
//! `LineMerge` builds the merge as `Revert` builds an inverse: a changeset
//! on the base whose root is the source's, rebased onto the target's tip
//! with line merge (D16). That result is stored unheld and landed with
//! `Land { merged_from }`, so the CAS, the retries, the idempotence and the
//! merge point row all come from `Land`.

use std::collections::HashMap;
use std::future::Future;
use std::pin::Pin;

use graft_model::{Changeset, ChangesetId, Metadata, diff_trees};
use graft_proto::types::{
    Land, Landed, LineMerge, LineMergeOutcome, LineMergePage, LineMergePoint, LineMerges,
    MergeBaseResult, MergeBaseVia, MergedFrom, Pair,
};
use graft_proto::{ApiError, CallContext};
use graft_store::{MetaStore, ObjectStore, RepoId, Write};

use crate::access::{Need, require};
use crate::journal;
use crate::rebase::REBASED_FROM;
use crate::refs::check_ref_name;
use crate::revert::caller;
use crate::source::{Trees, model_root};
use crate::{Engine, errors};

/// The metadata key naming what a merge landing merged: `<src>@<changeset>`.
pub(crate) const MERGED_FROM: &str = "graft.merged-from";

/// Merge rows read per step of a search.
const ROWS_PER_READ: u32 = 8;

/// Lines read per changeset when choosing the lines `MergeBase` compares.
const HOLDERS: u32 = 4;

/// The source of a merge landing, checked: `changeset` is at `pos` on
/// `line`'s line index.
#[derive(Clone, Debug)]
pub(crate) struct MergeSource {
    line: String,
    changeset: ChangesetId,
    pos: u64,
}

impl MergeSource {
    /// The merge point row and its journal entry, for the landing at
    /// `dst_pos` on `dst`; both go in the land's CAS transaction.
    pub(crate) fn writes(&self, dst: &str, dst_pos: u64) -> [Write; 2] {
        [
            Write::PutLineMerge {
                dst: dst.to_owned(),
                dst_pos,
                src: self.line.clone(),
                src_changeset: self.changeset,
                src_pos: self.pos,
            },
            journal::line_merged(&self.line, dst, self.changeset, self.pos, dst_pos),
        ]
    }
}

/// A changeset at a position on a line.
#[derive(Clone, Debug)]
struct Point {
    line: String,
    pos: u64,
    id: ChangesetId,
}

/// The budget and memory of one containment search.
struct Search {
    hops: u32,
    reads: u32,
    capped: bool,
    memo: HashMap<(String, u64, ChangesetId), bool>,
}

impl Search {
    /// Takes one read from the budget; false (and capped) when spent.
    fn read(&mut self) -> bool {
        if self.reads == 0 {
            self.capped = true;
            return false;
        }
        self.reads -= 1;
        true
    }
}

type Boxed<'a, T> = Pin<Box<dyn Future<Output = Result<T, ApiError>> + Send + 'a>>;

/// A merge base candidate: its rank (the target position it stands for,
/// then a tie-break), the changeset, and how it was found.
type Candidate = ((u64, u8), ChangesetId, MergeBaseVia);

fn better(best: &mut Option<Candidate>, c: Candidate) {
    if best.as_ref().is_none_or(|b| c.0 > b.0) {
        *best = Some(c);
    }
}

fn merge_message(src: &str, dst: &str, s: ChangesetId) -> String {
    format!("Merge {src} into {dst}\n\nMerges changeset {s}.\n")
}

impl<M: MetaStore, O: ObjectStore> Engine<M, O> {
    /// Checks a land's `merged_from`: a line other than `dst` the caller
    /// may read, holding the changeset on its line index.
    pub(crate) async fn merge_source(
        &self,
        cx: &CallContext,
        repo: RepoId,
        repo_name: &str,
        dst: &str,
        from: &MergedFrom,
    ) -> Result<MergeSource, ApiError> {
        check_ref_name(&from.line)?;
        require(cx, repo_name, Need::Read)?;
        if from.line == dst {
            return Err(errors::invalid(None, "merge-same-line"));
        }
        self.line_ref(repo, &from.line).await?;
        let pos = self
            .meta
            .line_pos(repo, &from.line, from.changeset)
            .await
            .map_err(errors::store)?
            .ok_or_else(|| errors::invalid(None, "merge-source-not-on-line"))?;
        Ok(MergeSource {
            line: from.line.clone(),
            changeset: from.changeset,
            pos,
        })
    }

    fn search(&self) -> Search {
        Search {
            hops: self.config.merge_hops,
            reads: self.config.merge_reads,
            capped: false,
            memo: HashMap::new(),
        }
    }

    /// The changeset at `pos` on the line whose tip `tip` is at `tip_pos`:
    /// a skip-list jump down the first parents.
    async fn at_pos(
        &self,
        repo: RepoId,
        tip: ChangesetId,
        tip_pos: u64,
        pos: u64,
    ) -> Result<Option<ChangesetId>, ApiError> {
        let Some(distance) = tip_pos.checked_sub(pos) else {
            return Ok(None);
        };
        // Skip lists derived on the way are not kept: this is a read.
        let mut scratch = Vec::new();
        self.ancestor_at(repo, tip, distance, &mut scratch).await
    }

    /// Whether `line` up to `bound` contains `p`, within the budget.
    fn contains<'a>(
        &'a self,
        repo: RepoId,
        s: &'a mut Search,
        line: &'a str,
        bound: u64,
        p: &'a Point,
        depth: u32,
    ) -> Boxed<'a, bool> {
        Box::pin(async move {
            let key = (line.to_owned(), bound, p.id);
            if let Some(&hit) = s.memo.get(&key) {
                return Ok(hit);
            }
            let was_capped = s.capped;
            let got = self
                .contains_by_rules(repo, s, line, bound, p, depth)
                .await?;
            // A "no" from a capped search is not an answer to remember.
            if got || !s.capped || was_capped {
                s.memo.insert(key, got);
            }
            Ok(got)
        })
    }

    async fn contains_by_rules(
        &self,
        repo: RepoId,
        s: &mut Search,
        line: &str,
        bound: u64,
        p: &Point,
        depth: u32,
    ) -> Result<bool, ApiError> {
        // Rules 1 and 2 for a position on the line or a line it forked
        // from: one index read.
        if !s.read() {
            return Ok(false);
        }
        let pos = self
            .meta
            .line_pos(repo, line, p.id)
            .await
            .map_err(errors::store)?;
        match pos {
            Some(q) if q <= bound => return Ok(true),
            // On the line above the bound: landed after the bound on the
            // line's own first-parent chain, so nothing at or below the
            // bound brought it. Saves the merge rows below for a search
            // that cannot succeed.
            Some(_) => return Ok(false),
            None => {}
        }
        if depth >= s.hops {
            s.capped = true;
            return Ok(false);
        }

        // Rule 4: `p` is a merge landing whose two inputs `line` holds.
        if p.pos > 0 && s.read() {
            let rows = self
                .meta
                .line_merges(repo, &p.line, None, p.pos, 1)
                .await
                .map_err(errors::store)?;
            if let Some(row) = rows.into_iter().find(|r| r.dst_pos == p.pos) {
                let from = Point {
                    line: row.src,
                    pos: row.src_pos,
                    id: row.src_changeset,
                };
                if self
                    .contains(repo, s, line, bound, &from, depth + 1)
                    .await?
                    && s.read()
                {
                    let (cs, _) = self.changeset(repo, p.id).await?;
                    if let Some(parent) = cs.first_parent() {
                        let below = Point {
                            line: p.line.clone(),
                            pos: p.pos - 1,
                            id: parent,
                        };
                        if self
                            .contains(repo, s, line, bound, &below, depth + 1)
                            .await?
                        {
                            return Ok(true);
                        }
                    }
                }
            }
        }

        // Rule 3: a merge into `line` at or below `bound` brought it. The
        // rows are paged within the read budget, so an older merge is not
        // missed silently: running out of reads marks the search capped.
        let mut below = Some(bound);
        while let Some(at) = below {
            if !s.read() {
                break;
            }
            let rows = self
                .meta
                .line_merges(repo, line, None, at, ROWS_PER_READ)
                .await
                .map_err(errors::store)?;
            below = next_page(&rows);
            for row in rows {
                if self
                    .contains(repo, s, &row.src, row.src_pos, p, depth + 1)
                    .await?
                {
                    return Ok(true);
                }
            }
        }

        // Rule 2 through merges: the parent line below the fork point.
        if s.read()
            && let Some((parent, q)) = self
                .meta
                .line_fork(repo, line)
                .await
                .map_err(errors::store)?
            && self
                .contains(repo, s, &parent, bound.min(q), p, depth + 1)
                .await?
        {
            return Ok(true);
        }
        Ok(false)
    }

    /// `MergeBase` of `src` (merged) and `dst` (merged into) over merge
    /// points and fork rows: the candidate standing for the highest
    /// position on `dst` that both lines contain. `None` when the bounded
    /// search finds none; the caller falls back to the ancestry walk.
    async fn point_base(
        &self,
        repo: RepoId,
        src: &Point,
        dst: &Point,
    ) -> Result<Option<(ChangesetId, MergeBaseVia)>, ApiError> {
        let mut s = self.search();
        // The target already holds the source: the source is the base.
        if self
            .contains(repo, &mut s, &dst.line, dst.pos, src, 0)
            .await?
        {
            return Ok(Some((src.id, MergeBaseVia::MergePoint)));
        }
        let mut best: Option<Candidate> = None;

        // The target's merges, newest first: the landing itself when the
        // source holds it (rule 4), else what it merged. Paged within the
        // read budget.
        let mut below = Some(dst.pos);
        'rows: while let Some(at) = below {
            if !s.read() {
                break;
            }
            let rows = self
                .meta
                .line_merges(repo, &dst.line, None, at, ROWS_PER_READ)
                .await
                .map_err(errors::store)?;
            below = next_page(&rows);
            for row in rows {
                if s.reads == 0 {
                    s.capped = true;
                    break 'rows;
                }
                if let Some(id) = self.at_pos(repo, dst.id, dst.pos, row.dst_pos).await? {
                    let landing = Point {
                        line: dst.line.clone(),
                        pos: row.dst_pos,
                        id,
                    };
                    if self
                        .contains(repo, &mut s, &src.line, src.pos, &landing, 0)
                        .await?
                    {
                        better(&mut best, ((row.dst_pos, 1), id, MergeBaseVia::MergePoint));
                        break 'rows;
                    }
                }
                let merged = Point {
                    line: row.src,
                    pos: row.src_pos,
                    id: row.src_changeset,
                };
                if self
                    .contains(repo, &mut s, &src.line, src.pos, &merged, 0)
                    .await?
                {
                    better(
                        &mut best,
                        ((row.dst_pos, 0), merged.id, MergeBaseVia::MergePoint),
                    );
                    break 'rows;
                }
            }
        }

        // The source's latest merge from the target (a back-merge).
        if s.read()
            && let Some(row) = self
                .meta
                .line_merges(repo, &src.line, Some(&dst.line), src.pos, 1)
                .await
                .map_err(errors::store)?
                .into_iter()
                .next()
            && row.src_pos <= dst.pos
        {
            better(
                &mut best,
                (
                    (row.src_pos, 1),
                    row.src_changeset,
                    MergeBaseVia::MergePoint,
                ),
            );
        }

        // A fork of one line from the other.
        if s.read()
            && let Some((parent, q)) = self
                .meta
                .line_fork(repo, &src.line)
                .await
                .map_err(errors::store)?
            && parent == dst.line
            && let Some(id) = self.at_pos(repo, dst.id, dst.pos, q).await?
        {
            better(&mut best, ((q, 1), id, MergeBaseVia::Fork));
        }
        if s.read()
            && let Some((parent, q)) = self
                .meta
                .line_fork(repo, &dst.line)
                .await
                .map_err(errors::store)?
            && parent == src.line
            && let Some(id) = self.at_pos(repo, src.id, src.pos, q).await?
        {
            better(&mut best, ((q, 1), id, MergeBaseVia::Fork));
        }
        // A capped search may have missed a higher candidate: its best so
        // far is not the answer, and the caller falls back to the ancestry
        // walk instead.
        if s.capped {
            return Ok(None);
        }
        Ok(best.map(|(_, id, via)| (id, via)))
    }

    /// `MergeBase` between the two points, merge points first, then the
    /// ancestry walk.
    async fn line_merge_base(
        &self,
        repo: RepoId,
        src: &Point,
        dst: &Point,
    ) -> Result<MergeBaseResult, ApiError> {
        if let Some((base, via)) = self.point_base(repo, src, dst).await? {
            return Ok(MergeBaseResult {
                base: Some(base),
                via,
            });
        }
        let base = self
            .index
            .merge_base(repo, src.id, dst.id)
            .await
            .map_err(errors::store)?;
        Ok(MergeBaseResult {
            base,
            via: MergeBaseVia::Ancestry,
        })
    }

    /// The points of `id` on the lines that index it, in line order, up
    /// to [`HOLDERS`].
    async fn points_of(&self, repo: RepoId, id: ChangesetId) -> Result<Vec<Point>, ApiError> {
        let held = self
            .meta
            .line_holders(repo, id, HOLDERS)
            .await
            .map_err(errors::store)?;
        Ok(held
            .into_iter()
            .map(|(line, pos)| Point { line, pos, id })
            .collect())
    }

    pub(crate) async fn do_merge_base_via(
        &self,
        cx: &CallContext,
        req: Pair,
    ) -> Result<MergeBaseResult, ApiError> {
        require(cx, &req.repo, Need::Read)?;
        let repo = self.repo_id(&req.repo).await?;
        self.changeset(repo, req.a).await?;
        self.changeset(repo, req.b).await?;
        let a = self.points_of(repo, req.a).await?;
        let b = self.points_of(repo, req.b).await?;
        if let Some((a, b)) = distinct_lines(&a, &b) {
            return self.line_merge_base(repo, a, b).await;
        }
        let base = self.do_merge_base(cx, req).await?;
        Ok(MergeBaseResult {
            base,
            via: MergeBaseVia::Ancestry,
        })
    }

    pub(crate) async fn do_line_merge(
        &self,
        cx: &CallContext,
        req: LineMerge,
    ) -> Result<LineMergeOutcome, ApiError> {
        check_ref_name(&req.src)?;
        check_ref_name(&req.dst)?;
        require(cx, &req.repo, Need::Read)?;
        require(cx, &req.repo, Need::Land(&req.dst))?;
        if req.src == req.dst {
            return Err(errors::invalid(None, "merge-same-line"));
        }
        let repo = self.repo_id(&req.repo).await?;
        let s = match req.src_at {
            Some(id) => id,
            None => self.line_ref(repo, &req.src).await?.target,
        };
        let source = self
            .merge_source(
                cx,
                repo,
                &req.repo,
                &req.dst,
                &MergedFrom {
                    line: req.src.clone(),
                    changeset: s,
                },
            )
            .await?;
        let tip = self.line_ref(repo, &req.dst).await?.target;
        let tip_pos = self
            .meta
            .line_pos(repo, &req.dst, tip)
            .await
            .map_err(errors::store)?
            .ok_or_else(|| errors::invalid(None, "tip-not-indexed"))?;
        let src = Point {
            line: req.src.clone(),
            pos: source.pos,
            id: s,
        };
        let dst = Point {
            line: req.dst.clone(),
            pos: tip_pos,
            id: tip,
        };
        let found = self.line_merge_base(repo, &src, &dst).await?;
        let Some(base) = found.base else {
            return Err(errors::invalid(None, "merge-unrelated"));
        };
        if base == s {
            return Ok(LineMergeOutcome::UpToDate { tip });
        }

        // The source's change since the base, as one changeset on the base.
        let (scs, _) = self.changeset(repo, s).await?;
        let base_root = self.root_of(repo, base).await?;
        let trees = Trees::new(&*self.objects, repo);
        let page = diff_trees(
            &trees,
            model_root(base_root),
            model_root(scs.root),
            self.config.max_changes as usize,
        )
        .await
        .map_err(errors::source)?;
        if page.next.is_some() {
            return Err(errors::limit("changes", self.config.max_changes.into()));
        }
        if page.changes.is_empty() {
            return Ok(LineMergeOutcome::UpToDate { tip });
        }
        let mut metadata = Metadata::new();
        metadata.insert(MERGED_FROM.to_owned(), format!("{}@{s}", req.src));
        let change = Changeset {
            parents: vec![base],
            root: scs.root,
            changes: page.changes,
            author: caller(cx),
            // No server time in an id: a retry builds the same merge.
            authored_at: scs.authored_at,
            message: req
                .message
                .clone()
                .unwrap_or_else(|| merge_message(&req.src, &req.dst, s)),
            metadata,
        };
        let change_id = change.id();
        let config = self.line_config(&req.dst);
        let as_merge_conflict = |e| merge_conflict(e, base, s);
        let plan = self
            .plan(
                repo,
                &[(change_id, change.clone())],
                tip,
                req.merge,
                &config,
            )
            .await
            .map_err(as_merge_conflict)?;
        let (cs, uploads) = match plan.new.last() {
            Some(cs) => {
                let uploads = plan.uploads(cs);
                let mut cs = cs.clone();
                // The change on the base is never stored.
                cs.metadata.remove(REBASED_FROM);
                (cs, uploads)
            }
            // The base is the tip: the change goes on as it is.
            None if plan.head == change_id => (change, Vec::new()),
            // Every change is already on the target.
            None => return Ok(LineMergeOutcome::UpToDate { tip }),
        };
        let created = self.insert_unheld(repo, cs, uploads).await?;
        let landed = self
            .do_land(
                cx,
                Land {
                    repo: req.repo.clone(),
                    head: created.id,
                    line: req.dst.clone(),
                    squash: false,
                    merge: req.merge,
                    deadline_ms: req.deadline_ms,
                    merged_from: Some(MergedFrom {
                        line: req.src.clone(),
                        changeset: s,
                    }),
                },
            )
            .await
            .map_err(as_merge_conflict)?;
        match landed {
            Landed::Landed {
                landed,
                line_version,
                pos,
                merged_paths,
                ..
            } => {
                let mut paths = plan.merged;
                paths.extend(merged_paths);
                paths.sort();
                paths.dedup();
                Ok(LineMergeOutcome::Merged {
                    landed,
                    line_version,
                    pos,
                    merged_paths: paths,
                    base,
                    via: found.via,
                })
            }
            _ => Err(errors::invalid(None, "merge-queued")),
        }
    }

    pub(crate) async fn do_line_merges(
        &self,
        cx: &CallContext,
        req: LineMerges,
    ) -> Result<LineMergePage, ApiError> {
        check_ref_name(&req.dst)?;
        if let Some(src) = &req.src {
            check_ref_name(src)?;
        }
        require(cx, &req.repo, Need::Read)?;
        let repo = self.repo_id(&req.repo).await?;
        let tip = self.line_ref(repo, &req.dst).await?.target;
        let tip_pos = self
            .meta
            .line_pos(repo, &req.dst, tip)
            .await
            .map_err(errors::store)?
            .ok_or_else(|| errors::invalid(None, "tip-not-indexed"))?;
        let at_or_below = match &req.page.token {
            Some(t) => t
                .parse::<u64>()
                .map_err(|_| errors::invalid(None, "page-token"))?,
            None => tip_pos,
        };
        let limit = self.page_size(req.page.limit);
        let rows = self
            .meta
            .line_merges(repo, &req.dst, req.src.as_deref(), at_or_below, limit)
            .await
            .map_err(errors::store)?;
        let mut merges = Vec::with_capacity(rows.len());
        for row in &rows {
            let dst_changeset = self
                .at_pos(repo, tip, tip_pos, row.dst_pos)
                .await?
                .ok_or_else(|| errors::invalid(None, "merge-not-indexed"))?;
            merges.push(LineMergePoint {
                src: row.src.clone(),
                src_changeset: row.src_changeset,
                src_pos: row.src_pos,
                dst_changeset,
                dst_pos: row.dst_pos,
            });
        }
        let next = match rows.last() {
            Some(last) if rows.len() == limit as usize && last.dst_pos > 0 => {
                Some((last.dst_pos - 1).to_string())
            }
            _ => None,
        };
        Ok(LineMergePage { merges, next })
    }
}

/// The next page bound after a full page of merge rows, newest first.
fn next_page(rows: &[graft_store::LineMergeRow]) -> Option<u64> {
    match rows.last() {
        Some(last) if rows.len() == ROWS_PER_READ as usize => last.dst_pos.checked_sub(1),
        _ => None,
    }
}

/// The first pair, in line order, of points on two different lines.
fn distinct_lines<'a>(a: &'a [Point], b: &'a [Point]) -> Option<(&'a Point, &'a Point)> {
    a.iter()
        .find_map(|a| b.iter().find(|b| b.line != a.line).map(|b| (a, b)))
}

/// A merge's conflict carries the base and the source as well as the tip
/// (§8.9), so the client can write markers in one fetch.
fn merge_conflict(e: ApiError, base: ChangesetId, source: ChangesetId) -> ApiError {
    match e {
        ApiError::Conflict { paths, onto } => {
            ApiError::MergeConflict(Box::new(graft_proto::error::MergeConflict {
                paths,
                onto,
                base,
                source,
            }))
        }
        e => e,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use graft_model::ObjectId;

    #[test]
    fn the_default_message_names_both_lines_and_the_source() {
        let id = ChangesetId(ObjectId([7; 32]));
        assert_eq!(
            merge_message("develop", "main", id),
            format!("Merge develop into main\n\nMerges changeset {id}.\n")
        );
    }

    #[test]
    fn the_higher_target_position_wins() {
        let a = ChangesetId(ObjectId([1; 32]));
        let b = ChangesetId(ObjectId([2; 32]));
        let mut best = None;
        better(&mut best, ((3, 0), a, MergeBaseVia::MergePoint));
        better(&mut best, ((2, 1), b, MergeBaseVia::Fork));
        assert_eq!(best.map(|c| c.1), Some(a));
        better(&mut best, ((3, 1), b, MergeBaseVia::Fork));
        assert_eq!(best.map(|c| c.1), Some(b));
    }
}
