//! One test per changeset validity rule that needs no store (design §4.5).
//! Rules 1 (parents exist) and 4 (blobs exist) need the store and belong to
//! the engine.

mod common;

use common::{MemSource, blob, block_on, build, del, path, put};
use graft_model::changeset::{CheckOptions, Rule};
use graft_model::tree_ops::ApplyError;
use graft_model::{
    Author, AuthorKind, Change, Changeset, ChangesetId, EntryKind, Metadata, ObjectId, PathError,
    Portability, Timestamp, TreeId, apply_changes,
};

fn changeset(parents: usize, changes: Vec<Change>) -> Changeset {
    Changeset {
        parents: (0..parents)
            .map(|i| ChangesetId(ObjectId([i as u8 + 1; 32])))
            .collect(),
        root: TreeId(ObjectId([0; 32])),
        changes,
        author: Author {
            name: "t".into(),
            email: "t@example.com".into(),
            kind: AuthorKind::Human,
        },
        authored_at: Timestamp {
            ms: 0,
            offset_min: 0,
        },
        message: String::new(),
        metadata: Metadata::new(),
    }
}

fn rule_of(cs: &Changeset, opts: &CheckOptions) -> Option<Rule> {
    cs.check(opts).err().map(|v| v.rule)
}

fn apply_rule(changes: &[Change]) -> (Option<String>, Rule) {
    let source = MemSource::default();
    let model = [
        (path("a/file"), (EntryKind::File, blob("1"))),
        (path("a/dir/x"), (EntryKind::File, blob("2"))),
        (path("top"), (EntryKind::File, blob("3"))),
    ]
    .into();
    let root = build(&source, &model);
    match block_on(apply_changes(&source, root, changes)) {
        Err(ApplyError::Invalid(v)) => (v.path.map(|p| p.to_string()), v.rule),
        other => panic!("expected a violation, got {other:?}"),
    }
}

#[test]
fn valid_changeset_passes() {
    let cs = changeset(1, vec![put("a", "1"), del("b")]);
    assert_eq!(cs.check(&CheckOptions::default()), Ok(()));
}

#[test]
fn rule_2_root_changeset_needs_permission() {
    let cs = changeset(0, vec![put("a", "1")]);
    assert_eq!(
        rule_of(&cs, &CheckOptions::default()),
        Some(Rule::RootChangeset)
    );
    let opts = CheckOptions {
        allow_root: true,
        ..CheckOptions::default()
    };
    assert_eq!(cs.check(&opts), Ok(()));
}

#[test]
fn rule_3_each_path_at_most_once() {
    let cs = changeset(1, vec![put("a", "1"), del("a")]);
    assert_eq!(
        rule_of(&cs, &CheckOptions::default()),
        Some(Rule::DuplicatePath)
    );
    // apply_changes enforces it too, whatever the order.
    assert_eq!(
        apply_rule(&[del("top"), put("top", "x")]).1,
        Rule::DuplicatePath
    );
}

#[test]
fn rule_3_changes_sorted_by_path_bytes() {
    let cs = changeset(1, vec![put("b", "1"), put("a", "1")]);
    assert_eq!(
        rule_of(&cs, &CheckOptions::default()),
        Some(Rule::ChangesSorted)
    );
    // Byte order: '.' sorts before '/'.
    let cs = changeset(1, vec![put("a.txt", "1"), put("a/b", "1")]);
    assert_eq!(cs.check(&CheckOptions::default()), Ok(()));
    let cs = changeset(1, vec![put("a/b", "1"), put("a.txt", "1")]);
    assert_eq!(
        rule_of(&cs, &CheckOptions::default()),
        Some(Rule::ChangesSorted)
    );
}

#[test]
fn rule_3_paths_pass_portability_when_enabled() {
    let cs = changeset(1, vec![put("src/aux.rs", "1")]);
    assert_eq!(cs.check(&CheckOptions::default()), Ok(()));
    let opts = CheckOptions {
        portable_paths: true,
        ..CheckOptions::default()
    };
    let v = cs.check(&opts).unwrap_err();
    assert_eq!(v.path, Some(path("src/aux.rs")));
    assert_eq!(
        v.rule,
        Rule::Path(PathError::NotPortable(Portability::ReservedName))
    );
}

#[test]
fn rule_5_delete_names_an_existing_entry() {
    assert_eq!(
        apply_rule(&[del("missing")]),
        (Some("missing".into()), Rule::DeleteMissing)
    );
    assert_eq!(
        apply_rule(&[del("a/nope/x")]),
        (Some("a/nope/x".into()), Rule::DeleteMissing)
    );
}

#[test]
fn rule_5_delete_cannot_name_a_directory() {
    assert_eq!(
        apply_rule(&[del("a/dir")]),
        (Some("a/dir".into()), Rule::DeleteMissing)
    );
}

#[test]
fn rule_6_put_is_not_a_no_op() {
    assert_eq!(
        apply_rule(&[put("top", "3")]),
        (Some("top".into()), Rule::PutNoOp)
    );
}

#[test]
fn rule_7_no_file_over_a_directory() {
    assert_eq!(
        apply_rule(&[put("a/dir", "z")]),
        (Some("a/dir".into()), Rule::FileAndDirectory)
    );
}

#[test]
fn rule_7_no_directory_under_a_file() {
    assert_eq!(
        apply_rule(&[put("top/x", "z")]),
        (Some("top".into()), Rule::FileAndDirectory)
    );
}

#[test]
fn rule_7_no_new_path_both_file_and_directory() {
    assert_eq!(
        apply_rule(&[put("new", "z"), put("new/x", "z")]),
        (Some("new".into()), Rule::FileAndDirectory)
    );
}

#[test]
fn rule_8_change_count_within_limit() {
    let cs = changeset(1, vec![put("a", "1"), put("b", "1"), put("c", "1")]);
    let opts = CheckOptions {
        max_changes: 2,
        ..CheckOptions::default()
    };
    assert_eq!(rule_of(&cs, &opts), Some(Rule::TooManyChanges));
    assert_eq!(CheckOptions::default().max_changes, 100_000);
}

#[test]
fn rule_9_empty_changeset_needs_several_parents() {
    let cs = changeset(1, vec![]);
    assert_eq!(
        rule_of(&cs, &CheckOptions::default()),
        Some(Rule::EmptyChangeset)
    );
    let cs = changeset(2, vec![]);
    assert_eq!(cs.check(&CheckOptions::default()), Ok(()));
}

#[test]
fn rule_9_canonical_empty_root_is_the_only_native_exception() {
    let root = Changeset::canonical_empty_root();
    let opts = CheckOptions {
        allow_root: true,
        ..CheckOptions::default()
    };
    assert_eq!(root.check(&opts), Ok(()));
    let mut other = root.clone();
    other.message = "not the root".into();
    assert_eq!(rule_of(&other, &opts), Some(Rule::EmptyChangeset));
    let mut with_parent = root.clone();
    with_parent.parents = vec![ChangesetId(ObjectId([1; 32]))];
    assert_eq!(rule_of(&with_parent, &opts), Some(Rule::EmptyChangeset));
    let mut imported = root.clone();
    imported
        .metadata
        .insert("mirror.git-commit".into(), "abc".into());
    assert_eq!(imported.check(&opts), Ok(()));
    assert!(!imported.is_canonical_empty_root());
}

#[test]
fn put_kind_is_never_tree() {
    let bad = Change::Put {
        path: path("x"),
        kind: EntryKind::Tree,
        id: blob("1"),
    };
    let cs = changeset(1, vec![bad.clone()]);
    assert_eq!(rule_of(&cs, &CheckOptions::default()), Some(Rule::PutKind));
    assert_eq!(apply_rule(&[bad]).1, Rule::PutKind);
}

#[test]
fn replacing_a_file_with_a_directory_and_back_is_allowed() {
    let source = MemSource::default();
    let model = [(path("a/file"), (EntryKind::File, blob("1")))].into();
    let root = build(&source, &model);
    // File to directory: delete the file, put beneath it.
    let applied = block_on(apply_changes(
        &source,
        root,
        &[del("a/file"), put("a/file/x", "2")],
    ))
    .unwrap();
    source.write(&applied);
    assert_eq!(
        common::flatten(&source, applied.root),
        [(path("a/file/x"), (EntryKind::File, blob("2")))].into()
    );
    // Directory to file: delete everything beneath, put the file.
    let back = block_on(apply_changes(
        &source,
        applied.root,
        &[put("a/file", "1"), del("a/file/x")],
    ))
    .unwrap();
    assert_eq!(back.root, root);
}

#[test]
fn rules_have_stable_names() {
    let names: Vec<_> = [
        Rule::ParentExists,
        Rule::RootChangeset,
        Rule::DuplicatePath,
        Rule::ChangesSorted,
        Rule::BlobExists,
        Rule::DeleteMissing,
        Rule::PutNoOp,
        Rule::FileAndDirectory,
        Rule::TooManyChanges,
        Rule::EmptyChangeset,
        Rule::PutKind,
    ]
    .iter()
    .map(Rule::as_str)
    .collect();
    assert_eq!(
        names,
        [
            "parent-exists",
            "root-changeset",
            "duplicate-path",
            "changes-sorted",
            "blob-exists",
            "delete-missing",
            "put-no-op",
            "file-and-directory",
            "too-many-changes",
            "empty-changeset",
            "put-kind",
        ]
    );
}
