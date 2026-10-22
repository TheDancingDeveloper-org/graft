//! First-parent history: `Log` (`docs/design.md` §7.5).
//!
//! A query walks first parents from `from` until it reaches a changeset on a
//! line (the stretch of drafts above it, at most the stack depth), then
//! answers the rest from the line's index: a `line_paths` range scan with
//! paths, or a first-parent walk from the right position (reached by skip
//! list) without. Either way it reads O(results) rows, plus O(log depth)
//! skip lists to start a page part-way down the line.

use std::collections::BTreeMap;

use graft_model::{Changeset, ChangesetId, RepoPath};
use graft_store::{MAX_PAGE, MetaStore, ObjectStore, RefKind, RepoId, StoreError, StoreResult};

use crate::{Index, LogQuery};

/// A changeset's place on a line: an index into the line names, and its
/// position there.
pub(crate) type Place = (usize, u64);

/// Whether `cs` changes anything at or beneath one of `paths` (every
/// changeset matches no filter).
pub(crate) fn touches(cs: &Changeset, paths: &[RepoPath]) -> bool {
    paths.is_empty()
        || cs
            .changes
            .iter()
            .any(|c| paths.iter().any(|p| c.path().starts_with(p)))
}

impl<M: MetaStore, O: ObjectStore> Index<M, O> {
    /// First-parent history, newest first: at most `q.limit` changesets on
    /// `from`'s first-parent chain (`from` included) that touch one of
    /// `q.paths`, stopping at `q.until`. See [`LogQuery`] for paging.
    pub async fn log(&self, repo: RepoId, q: LogQuery) -> StoreResult<Vec<ChangesetId>> {
        let entries = self.history(repo, &q).await?;
        Ok(entries.into_iter().map(|(id, _)| id).collect())
    }

    /// [`Index::log`] with each result's line position (`None` above the
    /// line).
    pub(crate) async fn history(
        &self,
        repo: RepoId,
        q: &LogQuery,
    ) -> StoreResult<Vec<(ChangesetId, Option<u64>)>> {
        let limit = graft_store::page_limit(q.limit);
        let lines = self.lines(repo).await?;
        let mut out = Vec::new();

        // The drafts above the line.
        let mut cur = Some(q.from);
        let mut anchor = None;
        while let Some(id) = cur {
            if Some(id) == q.until {
                return Ok(out);
            }
            if let Some(place) = self.place(repo, &lines, id).await? {
                anchor = Some((id, place));
                break;
            }
            let cs = self.load(repo, id).await?;
            if q.after_pos.is_none() && touches(&cs, &q.paths) {
                out.push((id, None));
                if out.len() == limit {
                    return Ok(out);
                }
            }
            cur = cs.first_parent();
        }
        let Some((tip, (line, tip_pos))) = anchor else {
            return Ok(out);
        };
        let line = &lines[line];

        // The line: positions in `(floor, before)`.
        let floor = match q.until {
            Some(until) => self.meta.line_pos(repo, line, until).await?,
            None => None,
        };
        let before = q.after_pos.map_or(tip_pos + 1, |a| a.min(tip_pos + 1));
        let need = limit - out.len();
        if floor.is_some_and(|f| f + 1 >= before) {
            return Ok(out);
        }
        if q.paths.is_empty() {
            self.chain(repo, tip, tip_pos, before, floor, need, &mut out)
                .await?;
        } else {
            self.scan(repo, line, &q.paths, before, floor, need, &mut out)
                .await?;
        }
        Ok(out)
    }

    /// Pushes up to `need` index rows of `paths` on `line` at positions in
    /// `(floor, before)`, newest first. Reads at most `need` rows per path.
    #[allow(clippy::too_many_arguments)]
    async fn scan(
        &self,
        repo: RepoId,
        line: &str,
        paths: &[RepoPath],
        before: u64,
        floor: Option<u64>,
        need: usize,
        out: &mut Vec<(ChangesetId, Option<u64>)>,
    ) -> StoreResult<()> {
        // Positions are unique on a line; a changeset matching two filters
        // (or a path and its directory) is one result.
        let mut rows = BTreeMap::new();
        for path in paths {
            let page = self
                .meta
                .scan_line_path(repo, line, path, before, need as u32)
                .await?;
            for (pos, id) in page {
                if floor.is_none_or(|f| pos > f) {
                    rows.insert(pos, id);
                }
            }
        }
        out.extend(
            rows.into_iter()
                .rev()
                .take(need)
                .map(|(pos, id)| (id, Some(pos))),
        );
        Ok(())
    }

    /// Pushes up to `need` first-parent ancestors of `tip` (at `tip_pos`)
    /// at positions in `(floor, before)`, newest first. One changeset read
    /// per result after the first, which a skip-list jump finds.
    #[allow(clippy::too_many_arguments)]
    async fn chain(
        &self,
        repo: RepoId,
        tip: ChangesetId,
        tip_pos: u64,
        before: u64,
        floor: Option<u64>,
        need: usize,
        out: &mut Vec<(ChangesetId, Option<u64>)>,
    ) -> StoreResult<()> {
        let Some(mut pos) = before.checked_sub(1) else {
            return Ok(());
        };
        let mut id = self.first_parent_ancestor(repo, tip, tip_pos - pos).await?;
        for n in 0..need {
            if floor.is_some_and(|f| pos <= f) {
                break;
            }
            out.push((id, Some(pos)));
            if n + 1 == need || pos == 0 {
                break;
            }
            match self.load(repo, id).await?.first_parent() {
                Some(parent) => id = parent,
                None => break,
            }
            pos -= 1;
        }
        Ok(())
    }

    /// The first-parent ancestor `distance` steps below `id`, by skip list:
    /// O(log distance) lists.
    pub(crate) async fn first_parent_ancestor(
        &self,
        repo: RepoId,
        mut id: ChangesetId,
        mut distance: u64,
    ) -> StoreResult<ChangesetId> {
        while distance > 0 {
            let skips = self.skip_list(repo, id).await?.value;
            let k = (distance.ilog2() as usize).min(skips.ancestors.len().saturating_sub(1));
            let next = *skips.ancestors.get(k).ok_or_else(|| {
                StoreError::Invalid(format!(
                    "{id} has no first-parent ancestor {distance} below"
                ))
            })?;
            id = next;
            distance -= 1 << k;
        }
        Ok(id)
    }

    /// The repository's lines, by name: its line refs (a handful, §4.6). A
    /// line's index is keyed by its ref name.
    pub(crate) async fn lines(&self, repo: RepoId) -> StoreResult<Vec<String>> {
        let refs = self.meta.list_refs(repo, "", MAX_PAGE).await?;
        Ok(refs
            .into_iter()
            .filter(|r| r.kind == RefKind::Line)
            .map(|r| r.name)
            .collect())
    }

    /// `id`'s place on the first of `lines` that holds it.
    pub(crate) async fn place(
        &self,
        repo: RepoId,
        lines: &[String],
        id: ChangesetId,
    ) -> StoreResult<Option<Place>> {
        for (i, line) in lines.iter().enumerate() {
            if let Some(pos) = self.meta.line_pos(repo, line, id).await? {
                return Ok(Some((i, pos)));
            }
        }
        Ok(None)
    }
}
