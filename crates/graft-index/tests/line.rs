//! Line positions and line path index rows.

mod common;

use std::collections::{BTreeMap, BTreeSet};

use common::{Fixture, block_on, changeset, path, put};
use graft_index::path_prefixes;
use graft_model::{Change, Changeset, ChangesetId, RepoPath};
use graft_store::{MetaStore, StoreError, Write};
use proptest::prelude::*;

/// Lands `batch` on `line` and commits the rows.
fn land(fx: &Fixture, line: &str, batch: &[Changeset]) -> Vec<Write> {
    for cs in batch {
        fx.store_only(cs);
    }
    let rows = block_on(fx.index.on_land(fx.repo, line, batch)).unwrap();
    fx.commit(rows.clone());
    rows
}

fn scan(fx: &Fixture, line: &str, p: &RepoPath) -> Vec<(u64, ChangesetId)> {
    block_on(
        fx.index
            .meta()
            .scan_line_path(fx.repo, line, p, u64::MAX, 1000),
    )
    .unwrap()
}

/// Every prefix of `p`, the path itself included.
fn brute_prefixes(p: &str) -> Vec<String> {
    let parts: Vec<_> = p.split('/').collect();
    (1..=parts.len()).map(|n| parts[..n].join("/")).collect()
}

fn file_path() -> impl Strategy<Value = String> {
    prop::collection::vec(prop::sample::select(vec!["a", "b", "c"]), 1..5)
        .prop_map(|parts| parts.join("/") + ".f")
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(64))]

    /// Landing a random chain in random batches gives each changeset its
    /// index on the line, and the path index answers every path and
    /// directory with exactly the changesets that touched it, newest first.
    #[test]
    fn positions_and_path_rows_match_brute_force(
        touched in prop::collection::vec(prop::collection::vec(file_path(), 1..6), 1..30),
        cuts in prop::collection::vec(1usize..5, 1..30),
    ) {
        let fx = Fixture::new();
        let mut css: Vec<Changeset> = Vec::new();
        for (i, paths) in touched.iter().enumerate() {
            let parents = css.last().map(Changeset::id).into_iter().collect();
            let changes = paths.iter().map(|p| put(p)).collect();
            css.push(changeset(parents, i as u64, changes));
        }
        let mut start = 0;
        for cut in cuts.iter().cycle() {
            if start == css.len() {
                break;
            }
            let end = (start + cut).min(css.len());
            land(&fx, "main", &css[start..end]);
            start = end;
        }

        let mut want: BTreeMap<String, Vec<(u64, ChangesetId)>> = BTreeMap::new();
        for (pos, (cs, paths)) in css.iter().zip(&touched).enumerate() {
            let id = cs.id();
            let got = block_on(fx.index.meta().line_pos(fx.repo, "main", id)).unwrap();
            prop_assert_eq!(got, Some(pos as u64));
            let prefixes: BTreeSet<String> = paths.iter().flat_map(|p| brute_prefixes(p)).collect();
            for p in prefixes {
                want.entry(p).or_default().push((pos as u64, id));
            }
        }
        for (p, mut rows) in want {
            rows.reverse();
            prop_assert_eq!(scan(&fx, "main", &path(&p)), rows);
        }
    }
}

#[test]
fn path_rows_are_bounded_by_changes_times_depth() {
    // 50 files, each 6 deep, in disjoint directories: exactly 50 × 6 rows.
    let disjoint: Vec<Change> = (0..50)
        .map(|i| put(&format!("d{i}/e/f/g/h/file.rs")))
        .collect();
    let cs = changeset(Vec::new(), 0, disjoint);
    assert_eq!(path_prefixes(&cs).len(), 50 * 6);

    // 50 files in one directory share its five ancestors: 50 + 5 rows.
    let shared: Vec<Change> = (0..50).map(|i| put(&format!("a/b/c/d/e/f{i}"))).collect();
    let cs = changeset(Vec::new(), 1, shared);
    assert_eq!(path_prefixes(&cs).len(), 50 + 5);

    // Rows depend only on the changeset, not on the history before it.
    let fx = Fixture::new();
    let mut tip: Option<ChangesetId> = None;
    let mut history = Vec::new();
    for i in 0..300 {
        let cs = changeset(tip.into_iter().collect(), 100 + i, vec![put("x/y/z.rs")]);
        tip = Some(cs.id());
        history.push(cs);
    }
    land(&fx, "main", &history);
    let next = changeset(
        tip.into_iter().collect(),
        9999,
        vec![put("p/q/r.rs"), put("p/s.rs")],
    );
    fx.store_only(&next);
    fx.reads.reset();
    let rows = block_on(
        fx.index
            .on_land(fx.repo, "main", std::slice::from_ref(&next)),
    )
    .unwrap();
    // The position, then p, p/q, p/q/r.rs, p/s.rs.
    assert_eq!(rows.len(), 1 + 4);
    assert_eq!(fx.reads.line_pos(), 1);
    assert_eq!(fx.reads.changeset(), 0);
    assert_eq!(fx.reads.derived(), 0);
    assert_eq!(fx.reads.other(), 0);
    assert_eq!(
        rows[0],
        Write::PutLinePos {
            line: "main".into(),
            changeset: next.id(),
            pos: 300,
        }
    );
}

#[test]
fn replacing_a_file_with_a_directory_indexes_each_path_once() {
    let cs = changeset(
        Vec::new(),
        0,
        vec![
            Change::Delete { path: path("a/b") },
            put("a/b/c"),
            put("a/d"),
        ],
    );
    let got: Vec<_> = path_prefixes(&cs)
        .iter()
        .map(|p| p.as_str().to_owned())
        .collect();
    assert_eq!(got, ["a", "a/b", "a/b/c", "a/d"]);
}

#[test]
fn landing_on_a_parent_that_is_not_on_the_line_is_refused() {
    let fx = Fixture::new();
    let root = changeset(Vec::new(), 0, vec![put("f")]);
    fx.store_only(&root);
    let child = changeset(vec![root.id()], 1, vec![put("g")]);
    let err = block_on(fx.index.on_land(fx.repo, "main", &[child])).unwrap_err();
    assert!(matches!(err, StoreError::Invalid(_)), "{err:?}");
}

#[test]
fn landing_nothing_writes_nothing() {
    let fx = Fixture::new();
    assert!(
        block_on(fx.index.on_land(fx.repo, "main", &[]))
            .unwrap()
            .is_empty()
    );
}

#[test]
fn lines_are_indexed_separately() {
    let fx = Fixture::new();
    let root = changeset(Vec::new(), 0, vec![put("f")]);
    land(&fx, "main", std::slice::from_ref(&root));
    land(&fx, "release", std::slice::from_ref(&root));
    let next = changeset(vec![root.id()], 1, vec![put("f")]);
    land(&fx, "main", std::slice::from_ref(&next));
    assert_eq!(scan(&fx, "main", &path("f")).len(), 2);
    assert_eq!(scan(&fx, "release", &path("f")), vec![(0, root.id())]);
}
