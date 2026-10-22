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
//!
//! Each diff has a work budget. Histogram diff is near linear when lines
//! are distinct, but on files made of a few lines repeated many times
//! (generated code, logs, CSV) it degrades towards quadratic. Before
//! diffing, [`carry`] trims the common prefix and suffix and estimates the
//! work left (see [`work`]): the pairs of repeated lines it might compare,
//! divided by the rare lines the diff can anchor on to split the file.
//! Past [`WORK_PER_LINE`] times the lines left, it attributes
//! coarsely: the trimmed middle is blamed on the newer version, as if it
//! rewrote it. The choice depends only on the two contents, so the answer
//! is deterministic. Between versions blame yields to the executor.

use std::collections::HashMap;
use std::future::Future;
use std::iter;
use std::pin::Pin;
use std::task::{Context, Poll};

use graft_model::{Change, Changeset, ChangesetId, EntryKind, ObjectId, ObjectKind, RepoPath};
use graft_proto::Frame;
use graft_store::{MAX_PAGE, MetaStore, MetaTxn, ObjectStore, RepoId, StoreError, StoreResult};
use imara_diff::{Algorithm, Diff, InternedInput, Token};

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

/// Lines that occur at most this often on a side are cheap for histogram
/// diff (its own chain limit is 63); only more repeated lines count
/// towards the work estimate.
const REPEAT_THRESHOLD: u32 = 64;

/// Work budget per line of the trimmed middle (both sides). A 20 000 line
/// edit may estimate up to about 40 million repeated-line pairs before it
/// is attributed coarsely.
const WORK_PER_LINE: u64 = 1024;

/// How [`carry`] attributed a version.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Attribution {
    /// Histogram diff.
    Diff,
    /// Over budget: the middle between the common prefix and suffix is
    /// blamed on the newer version.
    Coarse,
}

/// Estimated work for histogram diff of `old` against `new`. For each line
/// occurring more than [`REPEAT_THRESHOLD`] times on either side, its
/// count in `old` times its count in `new`; the sum is divided by one plus
/// the anchors, the lines of `new` that occur on both sides at most
/// [`REPEAT_THRESHOLD`] times. Anchors split the diff into regions that
/// each see only their share of the repeats, so ordinary source (with many
/// blank lines and braces, but distinct lines between them) stays cheap,
/// while a file of a few lines repeated has no anchors and pays in full.
fn work(old: &[Token], new: &[Token]) -> u64 {
    let mut counts: HashMap<Token, (u64, u64)> = HashMap::new();
    for t in old {
        counts.entry(*t).or_default().0 += 1;
    }
    for t in new {
        counts.entry(*t).or_default().1 += 1;
    }
    let limit = u64::from(REPEAT_THRESHOLD);
    let (mut pairs, mut anchors) = (0u64, 0u64);
    for &(a, b) in counts.values() {
        if a > limit || b > limit {
            pairs = pairs.saturating_add(a * b);
        } else if a > 0 && b > 0 {
            anchors += b;
        }
    }
    pairs / (anchors + 1)
}

/// `new`'s line origins: those of the lines it keeps from `old` (whose
/// origins are `origins`), and `id` for the rest. Past the work budget
/// (see the module docs) every line between the common prefix and suffix
/// gets `id`.
fn carry(old: &[u8], new: &[u8], origins: &[ChangesetId], id: ChangesetId) -> Vec<ChangesetId> {
    carry_with(old, new, origins, id).0
}

fn carry_with(
    old: &[u8],
    new: &[u8],
    origins: &[ChangesetId],
    id: ChangesetId,
) -> (Vec<ChangesetId>, Attribution) {
    let input = InternedInput::new(old, new);
    let (before, after) = (&input.before[..], &input.after[..]);
    let prefix = before.iter().zip(after).take_while(|(a, b)| a == b).count();
    let suffix = before[prefix..]
        .iter()
        .rev()
        .zip(after[prefix..].iter().rev())
        .take_while(|(a, b)| a == b)
        .count();
    let (mid_old, mid_new) = (
        &before[prefix..before.len() - suffix],
        &after[prefix..after.len() - suffix],
    );
    let budget = WORK_PER_LINE * (mid_old.len() + mid_new.len()) as u64;
    if work(mid_old, mid_new) > budget {
        let mut out = Vec::with_capacity(after.len());
        out.extend_from_slice(&origins[..prefix]);
        out.extend(iter::repeat_n(id, mid_new.len()));
        out.extend_from_slice(&origins[before.len() - suffix..]);
        return (out, Attribution::Coarse);
    }
    let mut diff = Diff::compute(Algorithm::Histogram, &input);
    diff.postprocess_lines(&input);
    let mut out = Vec::with_capacity(after.len());
    let mut kept = 0;
    for hunk in diff.hunks() {
        out.extend_from_slice(&origins[kept..hunk.before.start as usize]);
        out.extend(iter::repeat_n(id, hunk.after.len()));
        kept = hunk.before.end as usize;
    }
    out.extend_from_slice(&origins[kept..]);
    (out, Attribution::Diff)
}

/// Returns `Pending` once, so a long blame lets other tasks run between
/// versions on any executor.
struct YieldNow(bool);

impl Future for YieldNow {
    type Output = ();
    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<()> {
        if self.0 {
            Poll::Ready(())
        } else {
            self.0 = true;
            cx.waker().wake_by_ref();
            Poll::Pending
        }
    }
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
            YieldNow(false).await;
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

    /// About five distinct lines, `n` of them, shifted by `v`.
    fn repetitive(n: usize, v: usize) -> Vec<u8> {
        let mut out = Vec::new();
        for i in 0..n {
            out.extend_from_slice(format!("line {}\n", (i * 7 + v) % 5).as_bytes());
        }
        out
    }

    fn timed_blame(n: usize, versions: usize, bound: std::time::Duration) {
        let start = std::time::Instant::now();
        let mut content = Vec::new();
        let mut origins = Vec::new();
        let mut coarse = 0;
        for v in 0..versions {
            let next = repetitive(n, v);
            let (o, how) = carry_with(&content, &next, &origins, id(v as u8));
            coarse += usize::from(how == Attribution::Coarse);
            assert_eq!(o.len(), n);
            origins = o;
            content = next;
        }
        // Every version after the first is over budget.
        assert_eq!(coarse, versions - 1);
        let took = start.elapsed();
        assert!(
            took < bound,
            "{n} lines x {versions} versions took {took:?}"
        );
    }

    /// The review's probes: 80k lines x 10 versions and 20k lines x 100
    /// versions took 18.8s and 12.9s in release before the budget; now
    /// about 2s together in a debug build.
    #[test]
    fn repetitive_probes_are_bounded() {
        timed_blame(80_000, 10, std::time::Duration::from_secs(30));
        timed_blame(20_000, 100, std::time::Duration::from_secs(60));
    }

    /// Just under the budget, histogram diff still runs; the worst case
    /// stays well bounded.
    #[test]
    fn under_budget_worst_case_is_bounded() {
        let (old, new) = (repetitive(9_000, 0), repetitive(9_000, 1));
        let start = std::time::Instant::now();
        let (_, how) = carry_with(&old, &new, &vec![id(1); 9_000], id(2));
        assert_eq!(how, Attribution::Diff);
        assert!(start.elapsed() < std::time::Duration::from_secs(10));
    }

    #[test]
    fn coarse_blames_the_middle_on_the_newer_version() {
        let mut old = b"head\n".to_vec();
        old.extend(repetitive(20_000, 0));
        old.extend_from_slice(b"tail\n");
        let mut new = b"head\n".to_vec();
        new.extend(repetitive(20_000, 1));
        new.extend_from_slice(b"tail\n");
        let origins = vec![id(1); 20_002];
        let (got, how) = carry_with(&old, &new, &origins, id(2));
        assert_eq!(how, Attribution::Coarse);
        assert_eq!(got[0], id(1));
        assert_eq!(got[20_001], id(1));
        assert!(got[1..20_001].iter().all(|&o| o == id(2)));
    }

    #[test]
    fn distinct_lines_stay_under_budget() {
        let old: String = (0..20_000).map(|i| format!("{i}\n\n}}\n")).collect();
        let new = old
            .replacen("100\n", "x\n", 1)
            .replacen("19000\n", "y\n", 1);
        let origins = vec![id(1); 60_000];
        let (got, how) = carry_with(old.as_bytes(), new.as_bytes(), &origins, id(2));
        assert_eq!(how, Attribution::Diff);
        assert_eq!(got.iter().filter(|&&o| o == id(2)).count(), 2);
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
