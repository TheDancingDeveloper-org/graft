//! apply_changes and diff_trees: costs follow the change, and results match
//! a flat `BTreeMap` model.

mod common;

use common::{MemSource, Model, blob, block_on, build, del, flatten, model_diff, path, put};
use graft_model::tree_ops::{DiffPage, diff_trees_after};
use graft_model::{Change, EntryKind, ObjectId, RepoPath, Tree, apply_changes, diff_trees};
use proptest::prelude::*;

/// 10 directories × 10 subdirectories × 100 files = 10,000 files.
fn big_model() -> Model {
    let mut m = Model::new();
    for d in 0..10 {
        for s in 0..10 {
            for f in 0..100 {
                let p = format!("d{d}/s{s}/f{f}.rs");
                m.insert(path(&p), (EntryKind::File, blob(&p)));
            }
        }
    }
    m
}

#[test]
fn one_file_change_in_10k_files_writes_only_the_path() {
    let source = MemSource::default();
    let model = big_model();
    assert_eq!(model.len(), 10_000);
    let root = build(&source, &model);
    assert_eq!(source.len(), 1 + 10 + 100, "one tree per directory");

    source.reset_reads();
    let applied = block_on(apply_changes(&source, root, &[put("d3/s7/f42.rs", "new")])).unwrap();
    // Root, d3 and d3/s7: three reads, three writes, nothing else.
    assert_eq!(applied.new_trees.len(), 3);
    assert_eq!(source.reads(), 3);
    let (last_id, last) = applied.new_trees.last().unwrap();
    assert_eq!(Some(*last_id), applied.root, "children before parents");
    // Untouched subtrees keep their ids.
    let old_root = block_on(graft_model::ObjectSource::tree(&source, root.unwrap())).unwrap();
    for (old, new) in old_root.entries.iter().zip(&last.entries) {
        assert_eq!(old.name, new.name);
        assert_eq!(old.id == new.id, old.name != "d3", "{}", old.name);
    }

    source.write(&applied);
    let mut expected = model.clone();
    expected.insert(path("d3/s7/f42.rs"), (EntryKind::File, blob("new")));
    assert_eq!(flatten(&source, applied.root), expected);

    // The diff back reads only the differing subtrees: 3 directories on
    // each side.
    source.reset_reads();
    let page = block_on(diff_trees(&source, root, applied.root, 100)).unwrap();
    assert_eq!(source.reads(), 6);
    assert_eq!(page.changes, vec![put("d3/s7/f42.rs", "new")]);
    assert_eq!(page.next, None);

    // Diffing a snapshot against itself reads nothing.
    source.reset_reads();
    let page = block_on(diff_trees(&source, root, root, 100)).unwrap();
    assert_eq!((source.reads(), page), (0, DiffPage::default()));
}

#[test]
fn adding_a_new_directory_writes_only_its_path() {
    let source = MemSource::default();
    let root = build(&source, &big_model());
    source.reset_reads();
    let applied = block_on(apply_changes(
        &source,
        root,
        &[put("d3/new/deep/x.rs", "x")],
    ))
    .unwrap();
    // Reads root and d3; writes root, d3, d3/new, d3/new/deep.
    assert_eq!(source.reads(), 2);
    assert_eq!(applied.new_trees.len(), 4);
}

#[test]
fn deleting_the_last_file_removes_empty_directories() {
    let source = MemSource::default();
    let model: Model = [
        (path("a/b/c/only"), (EntryKind::File, blob("1"))),
        (path("keep"), (EntryKind::File, blob("2"))),
    ]
    .into();
    let root = build(&source, &model);
    let applied = block_on(apply_changes(&source, root, &[del("a/b/c/only")])).unwrap();
    assert_eq!(applied.new_trees.len(), 1, "only the root is rewritten");
    let root_tree = &applied.new_trees[0].1;
    assert_eq!(root_tree.entries.len(), 1);
    assert_eq!(root_tree.entries[0].name, "keep");

    // Removing everything leaves an empty snapshot.
    let applied = block_on(apply_changes(
        &source,
        root,
        &[del("a/b/c/only"), del("keep")],
    ))
    .unwrap();
    assert_eq!(applied.root, None);
    assert!(applied.new_trees.is_empty());
}

#[test]
fn identical_new_subtrees_are_written_once() {
    let source = MemSource::default();
    let applied = block_on(apply_changes(
        &source,
        None,
        &[put("a/x", "1"), put("b/x", "1")],
    ))
    .unwrap();
    // a and b are the same tree; plus the root.
    assert_eq!(applied.new_trees.len(), 2);
}

#[test]
fn empty_root_tree_is_an_empty_snapshot() {
    let source = MemSource::default();
    let empty = Tree::default();
    source.write(&graft_model::Applied {
        root: Some(empty.id()),
        new_trees: vec![(empty.id(), empty.clone())],
    });
    let applied = block_on(apply_changes(&source, Some(empty.id()), &[put("a", "1")])).unwrap();
    source.write(&applied);
    let page = block_on(diff_trees(&source, None, applied.root, 10)).unwrap();
    assert_eq!(page.changes, vec![put("a", "1")]);
}

#[test]
fn diff_orders_by_full_path_bytes() {
    // "a.txt" sorts between file "a" and directory "a/": '.' < '/'.
    let source = MemSource::default();
    let from: Model = [(path("a"), (EntryKind::File, blob("1")))].into();
    let to: Model = [
        (path("a/x"), (EntryKind::File, blob("2"))),
        (path("a.txt"), (EntryKind::File, blob("3"))),
    ]
    .into();
    let (f, t) = (build(&source, &from), build(&source, &to));
    let page = block_on(diff_trees(&source, f, t, 10)).unwrap();
    assert_eq!(
        page.changes,
        vec![del("a"), put("a.txt", "3"), put("a/x", "2")]
    );
    assert_eq!(page.changes, model_diff(&from, &to));
}

#[test]
fn diff_reports_kind_changes() {
    let source = MemSource::default();
    let from: Model = [(path("run"), (EntryKind::File, blob("1")))].into();
    let to: Model = [(path("run"), (EntryKind::Exec, blob("1")))].into();
    let page = block_on(diff_trees(
        &source,
        build(&source, &from),
        build(&source, &to),
        10,
    ))
    .unwrap();
    assert_eq!(
        page.changes,
        vec![Change::Put {
            path: path("run"),
            kind: EntryKind::Exec,
            id: blob("1"),
        }]
    );
}

#[test]
fn diff_pages_resume_without_rereading_earlier_subtrees() {
    let source = MemSource::default();
    let root = build(&source, &big_model());
    let changes = vec![put("d1/s1/f1.rs", "x"), put("d8/s8/f8.rs", "y")];
    let applied = block_on(apply_changes(&source, root, &changes)).unwrap();
    source.write(&applied);

    let first = block_on(diff_trees(&source, root, applied.root, 1)).unwrap();
    assert_eq!(first.changes, vec![changes[0].clone()]);
    assert_eq!(first.next, Some(path("d1/s1/f1.rs")));

    source.reset_reads();
    let second = block_on(diff_trees_after(
        &source,
        root,
        applied.root,
        first.next.as_ref(),
        1,
    ))
    .unwrap();
    assert_eq!(second.changes, vec![changes[1].clone()]);
    assert_eq!(second.next, None);
    // Both sides of: the root, the cursor's own path (d1, d1/s1) and the
    // next change's path (d8, d8/s8). No other subtree before the cursor
    // is read.
    assert_eq!(source.reads(), 10);
}

#[test]
fn diff_of_a_removed_directory_lists_every_file() {
    let source = MemSource::default();
    let from: Model = [
        (path("gone/a"), (EntryKind::File, blob("1"))),
        (path("gone/sub/b"), (EntryKind::File, blob("2"))),
        (path("stay"), (EntryKind::File, blob("3"))),
    ]
    .into();
    let to: Model = [(path("stay"), (EntryKind::File, blob("3")))].into();
    let page = block_on(diff_trees(
        &source,
        build(&source, &from),
        build(&source, &to),
        10,
    ))
    .unwrap();
    assert_eq!(page.changes, vec![del("gone/a"), del("gone/sub/b")]);
}

// ---- properties against the flat model ----

fn arb_path() -> impl Strategy<Value = RepoPath> {
    let name = prop::sample::select(vec!["a", "b", "a.txt", "a-b", "c"]);
    prop::collection::vec(name, 1..4).prop_map(|c| path(&c.join("/")))
}

fn arb_entry() -> impl Strategy<Value = (EntryKind, ObjectId)> {
    (
        prop::sample::select(vec![EntryKind::File, EntryKind::Exec, EntryKind::Symlink]),
        0u8..3,
    )
        .prop_map(|(k, b)| (k, blob(&b.to_string())))
}

/// Drops paths that would be both a file and a directory.
fn conflict_free(m: Model) -> Model {
    let keys: Vec<RepoPath> = m.keys().cloned().collect();
    m.into_iter()
        .filter(|(p, _)| !keys.iter().any(|q| q != p && q.starts_with(p)))
        .collect()
}

fn arb_model() -> impl Strategy<Value = Model> {
    prop::collection::btree_map(arb_path(), arb_entry(), 0..24).prop_map(conflict_free)
}

/// A model and a mutation of it that shares most of its structure.
fn arb_pair() -> impl Strategy<Value = (Model, Model)> {
    (
        arb_model(),
        prop::collection::vec(any::<bool>(), 24),
        arb_model(),
    )
        .prop_map(|(a, keep, extra)| {
            let mut b: Model = a
                .iter()
                .zip(keep.iter().cycle())
                .filter(|(_, k)| **k)
                .map(|((p, v), _)| (p.clone(), *v))
                .collect();
            b.extend(extra);
            (a, conflict_free(b))
        })
}

proptest! {
    #[test]
    fn apply_matches_the_model((a, b) in arb_pair()) {
        let source = MemSource::default();
        let ra = build(&source, &a);
        prop_assert_eq!(flatten(&source, ra), a.clone());

        let changes = model_diff(&a, &b);
        let applied = block_on(apply_changes(&source, ra, &changes)).unwrap();
        source.write(&applied);
        prop_assert_eq!(flatten(&source, applied.root), b.clone());
        // The same snapshot has the same root id however it was reached.
        prop_assert_eq!(applied.root, build(&source, &b));
    }

    #[test]
    fn diff_matches_the_model((a, b) in arb_pair(), limit in 1usize..5) {
        let source = MemSource::default();
        let (ra, rb) = (build(&source, &a), build(&source, &b));
        let expected = model_diff(&a, &b);

        let page = block_on(diff_trees(&source, ra, rb, usize::MAX - 1)).unwrap();
        prop_assert_eq!(&page.changes, &expected);
        prop_assert_eq!(page.next, None);

        // Paging with a small limit yields the same changes.
        let mut all = Vec::new();
        let mut after: Option<RepoPath> = None;
        loop {
            let page = block_on(diff_trees_after(&source, ra, rb, after.as_ref(), limit)).unwrap();
            prop_assert!(page.changes.len() <= limit);
            all.extend(page.changes);
            match page.next {
                Some(n) => after = Some(n),
                None => break,
            }
        }
        prop_assert_eq!(all, expected.clone());

        // Applying the diff to `a` gives `b`.
        let applied = block_on(apply_changes(&source, ra, &expected)).unwrap();
        prop_assert_eq!(applied.root, rb);
    }
}
