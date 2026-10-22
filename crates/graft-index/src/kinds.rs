//! The derived kinds of `docs/design.md` §4.9, and the value encodings of
//! the kinds stored in the derived table.
//!
//! Stored values are durable formats, pinned by the golden vectors below.
//! Changing one means bumping the kind's `VERSION`, which recomputes lazily.
//!
//! - [`Generation`]: `u64` big-endian.
//! - [`SkipList`]: `u64 depth ‖ u8 n ‖ n × changeset id(32)`, entry `k` the
//!   first-parent ancestor at distance `2^k`.
//! - [`Blame`]: `u32 n ‖ n × (u32 end ‖ origin id(32))`, runs in line order;
//!   each run starts where the one before it ends (the first at 0) and is
//!   not empty. Keyed by [`blame_key`].

use graft_model::{Changeset, ChangesetId, ObjectId, RepoPath};

use crate::{BlameRun, DerivedKind, Mode, ParentDerived, ParentScope, ValueKind};

macro_rules! kind {
    ($(#[$doc:meta])* $name:ident, $str:literal, $parents:expr, $mode:expr) => {
        $(#[$doc])*
        #[derive(Clone, Copy, Debug)]
        pub struct $name;
        impl DerivedKind for $name {
            const NAME: &'static str = $str;
            const VERSION: u32 = 1;
            const PARENTS: ParentScope = $parents;
            const MODE: Mode = $mode;
        }
    };
}

kind!(
    /// `1 + max(parent generations)`.
    Generation, "generation", ParentScope::All, Mode::Eager
);
kind!(
    /// First-parent ancestors at distance 2^k.
    SkipList, "skiplist", ParentScope::First, Mode::Eager
);
kind!(
    /// `(line, pos)` of landed changesets.
    LinePos, "linepos", ParentScope::First, Mode::Eager
);
kind!(
    /// `(line, path-or-prefix, pos)` rows per changed path.
    LinePaths, "linepaths", ParentScope::First, Mode::Eager
);
kind!(
    /// Line → origin per `(changeset, path)`.
    Blame, "blame", ParentScope::First, Mode::Lazy
);

impl ValueKind for Generation {
    type Value = u64;

    fn encode(value: &u64) -> Vec<u8> {
        value.to_be_bytes().to_vec()
    }

    fn decode(bytes: &[u8]) -> Option<u64> {
        Some(u64::from_be_bytes(bytes.try_into().ok()?))
    }
}

impl ParentDerived for Generation {
    fn derive(_cs: &Changeset, parents: &[u64]) -> u64 {
        1 + parents.iter().copied().max().unwrap_or(0)
    }
}

/// A changeset's first-parent skip list.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Skips {
    /// First-parent distance from a root (a root is 0).
    pub depth: u64,
    /// Entry `k` is the first-parent ancestor at distance `2^k`; the list
    /// ends at the first distance past a root.
    pub ancestors: Vec<ChangesetId>,
}

impl ValueKind for SkipList {
    type Value = Skips;

    fn encode(value: &Skips) -> Vec<u8> {
        let mut out = Vec::with_capacity(9 + 32 * value.ancestors.len());
        out.extend_from_slice(&value.depth.to_be_bytes());
        // At most 64 entries: distances are below 2^64.
        out.push(value.ancestors.len() as u8);
        for id in &value.ancestors {
            out.extend_from_slice(&id.0.0);
        }
        out
    }

    fn decode(bytes: &[u8]) -> Option<Skips> {
        let (depth, rest) = bytes.split_first_chunk::<8>()?;
        let (&n, rest) = rest.split_first()?;
        let (ids, tail) = rest.as_chunks::<32>();
        if ids.len() != usize::from(n) || !tail.is_empty() {
            return None;
        }
        Some(Skips {
            depth: u64::from_be_bytes(*depth),
            ancestors: ids.iter().map(|b| ChangesetId(ObjectId(*b))).collect(),
        })
    }
}

/// The derived-table key of a blame value: the changeset's 32 id bytes,
/// then the path's bytes.
pub fn blame_key(id: ChangesetId, path: &RepoPath) -> Vec<u8> {
    let mut key = id.0.0.to_vec();
    key.extend_from_slice(path.as_str().as_bytes());
    key
}

impl ValueKind for Blame {
    type Value = Vec<BlameRun>;

    fn encode(value: &Vec<BlameRun>) -> Vec<u8> {
        let mut out = Vec::with_capacity(4 + 36 * value.len());
        // Runs are lines of a blob (at most 64 MiB), so `n` fits.
        out.extend_from_slice(&(value.len() as u32).to_be_bytes());
        for run in value {
            out.extend_from_slice(&run.end.to_be_bytes());
            out.extend_from_slice(&run.origin.0.0);
        }
        out
    }

    fn decode(bytes: &[u8]) -> Option<Vec<BlameRun>> {
        let (n, rest) = bytes.split_first_chunk::<4>()?;
        let (runs, tail) = rest.as_chunks::<36>();
        if runs.len() != u32::from_be_bytes(*n) as usize || !tail.is_empty() {
            return None;
        }
        let mut start = 0;
        let mut out = Vec::with_capacity(runs.len());
        for run in runs {
            let (end, origin) = run.split_first_chunk::<4>()?;
            let end = u32::from_be_bytes(*end);
            if end <= start {
                return None;
            }
            out.push(BlameRun {
                start,
                end,
                origin: ChangesetId(ObjectId(origin.try_into().ok()?)),
            });
            start = end;
        }
        Some(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn id(b: u8) -> ChangesetId {
        ChangesetId(ObjectId([b; 32]))
    }

    #[test]
    fn generation_golden_vector() {
        assert_eq!(Generation::encode(&0x0102), [0, 0, 0, 0, 0, 0, 1, 2]);
        assert_eq!(Generation::decode(&[0, 0, 0, 0, 0, 0, 1, 2]), Some(0x0102));
        assert_eq!(Generation::decode(&[1, 2]), None);
    }

    #[test]
    fn skip_list_golden_vector() {
        let skips = Skips {
            depth: 3,
            ancestors: vec![id(0xaa), id(0xbb)],
        };
        let mut want = vec![0, 0, 0, 0, 0, 0, 0, 3, 2];
        want.extend_from_slice(&[0xaa; 32]);
        want.extend_from_slice(&[0xbb; 32]);
        assert_eq!(SkipList::encode(&skips), want);
        assert_eq!(SkipList::decode(&want), Some(skips));
    }

    #[test]
    fn skip_list_rejects_bad_lengths() {
        let mut bytes = SkipList::encode(&Skips {
            depth: 1,
            ancestors: vec![id(1)],
        });
        assert!(SkipList::decode(&bytes[..bytes.len() - 1]).is_none());
        bytes.push(0);
        assert!(SkipList::decode(&bytes).is_none());
        assert!(SkipList::decode(&[0; 7]).is_none());
    }

    #[test]
    fn blame_golden_vector() {
        let runs = vec![
            BlameRun {
                start: 0,
                end: 2,
                origin: id(0xaa),
            },
            BlameRun {
                start: 2,
                end: 0x0105,
                origin: id(0xbb),
            },
        ];
        let mut want = vec![0, 0, 0, 2, 0, 0, 0, 2];
        want.extend_from_slice(&[0xaa; 32]);
        want.extend_from_slice(&[0, 0, 1, 5]);
        want.extend_from_slice(&[0xbb; 32]);
        assert_eq!(Blame::encode(&runs), want);
        assert_eq!(Blame::decode(&want), Some(runs));
        assert_eq!(Blame::decode(&[0, 0, 0, 0]), Some(Vec::new()));
    }

    #[test]
    fn blame_rejects_bad_runs() {
        let one = |end: u32| {
            let mut b = vec![0, 0, 0, 1];
            b.extend_from_slice(&end.to_be_bytes());
            b.extend_from_slice(&[1; 32]);
            b
        };
        assert!(Blame::decode(&one(1)).is_some());
        // An empty run.
        assert!(Blame::decode(&one(0)).is_none());
        // A count that disagrees with the body.
        let mut bad = one(1);
        bad[3] = 2;
        assert!(Blame::decode(&bad).is_none());
        assert!(Blame::decode(&one(1)[..39]).is_none());
        assert!(Blame::decode(&[0, 0]).is_none());
    }

    #[test]
    fn blame_key_is_id_then_path() {
        let key = blame_key(id(3), &RepoPath::new("a/b.rs").unwrap());
        assert_eq!(&key[..32], &[3; 32]);
        assert_eq!(&key[32..], b"a/b.rs");
    }
}
