//! `Blame` (`docs/design.md` §7.7): a lazy kind cached per
//! `(changeset, path)`.
//!
//! The versions of a file are the changesets on its first-parent history
//! that changed it, which the line path index lists. Blame walks them
//! newest first to the nearest cached value (or the file's creation), then
//! diffs each version against the one before it (histogram diff) to carry
//! line origins forward. It reads one changeset, one cached value and one
//! blob per version: the cost is the versions of that file, times its size,
//! not the length of history.

use std::iter;

use graft_model::{Change, Changeset, ChangesetId, EntryKind, ObjectId, ObjectKind, RepoPath};
use graft_proto::Frame;
use graft_store::{MAX_PAGE, MetaStore, MetaTxn, ObjectStore, RepoId, StoreError, StoreResult};
use imara_diff::{Algorithm, Diff, InternedInput};

use crate::derive::row;
use crate::kinds::{Blame, blame_key};
use crate::{BlameRun, Index, LogQuery};

/// What a changeset on a path's history did to the file there.
enum Version {
    /// Wrote its content over the file before it.
    Edit(ObjectId),
    /// Wrote its content where no file was before (the path was a
    /// directory, or absent).
    Create(ObjectId),
    /// Left no file: deleted it, made it a directory or a submodule.
    Gone,
}

/// What `cs` (on `path`'s history, so it touched `path` or beneath it) did
/// to the file at `path`.
fn version(cs: &Changeset, path: &RepoPath) -> Version {
    let exact = cs.changes.iter().find(|c| c.path() == path);
    let beneath = cs
        .changes
        .iter()
        .any(|c| c.path() != path && c.path().starts_with(path));
    match exact {
        Some(Change::Put { kind, id, .. }) if *kind != EntryKind::Submodule => {
            // Changes beneath it mean it was a directory just before.
            if beneath {
                Version::Create(*id)
            } else {
                Version::Edit(*id)
            }
        }
        _ => Version::Gone,
    }
}

/// `new`'s line origins: those of the lines it keeps from `old` (whose
/// origins are `origins`), and `id` for the rest.
fn carry(old: &[u8], new: &[u8], origins: &[ChangesetId], id: ChangesetId) -> Vec<ChangesetId> {
    let input = InternedInput::new(old, new);
    let mut diff = Diff::compute(Algorithm::Histogram, &input);
    diff.postprocess_lines(&input);
    let mut out = Vec::with_capacity(input.after.len());
    let mut kept = 0;
    for hunk in diff.hunks() {
        out.extend_from_slice(&origins[kept..hunk.before.start as usize]);
        out.extend(iter::repeat_n(id, hunk.after.len()));
        kept = hunk.before.end as usize;
    }
    out.extend_from_slice(&origins[kept..]);
    out
}

/// Per-line origins as runs.
fn runs(origins: &[ChangesetId]) -> Vec<BlameRun> {
    let mut out: Vec<BlameRun> = Vec::new();
    for (line, &origin) in (0u32..).zip(origins) {
        match out.last_mut() {
            Some(run) if run.origin == origin => run.end = line + 1,
            _ => out.push(BlameRun {
                start: line,
                end: line + 1,
                origin,
            }),
        }
    }
    out
}

/// Runs as per-line origins.
fn lines(runs: &[BlameRun]) -> Vec<ChangesetId> {
    runs.iter()
        .flat_map(|r| iter::repeat_n(r.origin, (r.end - r.start) as usize))
        .collect()
}

impl<M: MetaStore, O: ObjectStore> Index<M, O> {
    /// A blob's bytes, verified against its id.
    async fn blob(&self, repo: RepoId, id: ObjectId) -> StoreResult<Vec<u8>> {
        let blob = id.to_hex();
        let bytes = self
            .objects
            .get(repo, id)
            .await?
            .ok_or_else(|| StoreError::Corrupt(format!("blob {blob} is missing")))?;
        Frame::from_bytes(&bytes)
            .and_then(|frame| {
                if frame.kind == ObjectKind::Blob {
                    frame.decode_verified(id)
                } else {
                    Err(graft_proto::FrameError::Unknown(frame.kind.letter() as u8))
                }
            })
            .map_err(|e| StoreError::Corrupt(format!("blob {blob}: {e:?}")))
    }

    /// Line blame of the file at `path` in `at`'s snapshot: runs covering
    /// every line, each with the changeset on `at`'s first-parent history
    /// that last changed it. The value is cached for the version of the
    /// file `at` sees. `Invalid` when `at` has no file at `path`.
    pub async fn blame(
        &self,
        repo: RepoId,
        at: ChangesetId,
        path: &RepoPath,
    ) -> StoreResult<Vec<BlameRun>> {
        // Versions newer than the base, newest first.
        let mut pending: Vec<(ChangesetId, ObjectId)> = Vec::new();
        // The nearest cached value, with its version's blob.
        let mut base: Option<(Vec<BlameRun>, ObjectId)> = None;
        let mut from = Some(at);
        // Pages grow, so the rows read stay within twice the versions used.
        let mut limit = 16;
        'walk: while let Some(start) = from.take() {
            let page = self
                .history(
                    repo,
                    &LogQuery {
                        from: start,
                        until: None,
                        paths: vec![path.clone()],
                        limit,
                        after_pos: None,
                    },
                )
                .await?;
            let full = page.len() == limit as usize;
            limit = (limit * 2).min(MAX_PAGE);
            for (id, _) in page {
                let cached = self.get::<Blame>(repo, &blame_key(id, path)).await?;
                if pending.is_empty()
                    && let Some(runs) = cached
                {
                    return Ok(runs);
                }
                let cs = self.load(repo, id).await?;
                match (version(&cs, path), cached) {
                    (Version::Edit(blob) | Version::Create(blob), Some(runs)) => {
                        base = Some((runs, blob));
                        break 'walk;
                    }
                    (Version::Edit(blob), None) => pending.push((id, blob)),
                    (Version::Create(blob), None) => {
                        pending.push((id, blob));
                        break 'walk;
                    }
                    (Version::Gone, _) => break 'walk,
                }
                if full {
                    from = cs.first_parent();
                }
            }
        }
        let Some(&(newest, _)) = pending.first() else {
            return Err(StoreError::Invalid(format!("no file at {path} in {at}")));
        };

        let (mut origins, mut content) = match base {
            Some((runs, blob)) => (lines(&runs), self.blob(repo, blob).await?),
            None => (Vec::new(), Vec::new()),
        };
        for (id, blob) in pending.into_iter().rev() {
            let next = self.blob(repo, blob).await?;
            origins = carry(&content, &next, &origins, id);
            content = next;
        }
        let value = runs(&origins);

        // Cache fill, best effort: a reader that cannot write still gets
        // its answer, and the next one computes it again.
        let txn = MetaTxn {
            preconditions: Vec::new(),
            writes: vec![row::<Blame>(blame_key(newest, path), &value)],
        };
        let _ = self.meta.commit(repo, txn).await;
        Ok(value)
    }
}

#[cfg(test)]
mod tests {
    use graft_model::ObjectId;

    use super::*;

    fn id(b: u8) -> ChangesetId {
        ChangesetId(ObjectId([b; 32]))
    }

    #[test]
    fn carry_keeps_unchanged_lines_and_blames_the_rest() {
        let old = b"a\nb\nc\n";
        let new = b"a\nB\nc\nd";
        let got = carry(old, new, &[id(1), id(2), id(3)], id(9));
        assert_eq!(got, vec![id(1), id(9), id(3), id(9)]);
        assert_eq!(carry(b"", b"x\ny\n", &[], id(4)), vec![id(4), id(4)]);
        assert_eq!(carry(b"x\n", b"", &[id(4)], id(5)), Vec::new());
    }

    #[test]
    fn runs_round_trip_per_line_origins() {
        let origins = vec![id(1), id(1), id(2), id(1)];
        let r = runs(&origins);
        assert_eq!(r.len(), 3);
        assert_eq!((r[0].start, r[0].end), (0, 2));
        assert_eq!((r[2].start, r[2].end), (3, 4));
        assert_eq!(lines(&r), origins);
        assert!(runs(&[]).is_empty());
    }
}
