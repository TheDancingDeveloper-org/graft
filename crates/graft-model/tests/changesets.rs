//! Store-free changeset validity rules.

use graft_model::{
    Author, AuthorKind, Changeset, ChangesetId, CheckOptions, Metadata, ObjectId, Rule, Timestamp,
    Tree,
};

fn empty_changeset(parents: Vec<ChangesetId>, metadata: Metadata) -> Changeset {
    Changeset {
        parents,
        root: Tree::default().id(),
        changes: Vec::new(),
        author: Author {
            name: "Git import".to_owned(),
            email: "import@example.com".to_owned(),
            kind: AuthorKind::Bot,
        },
        authored_at: Timestamp {
            ms: 1_700_000_000_000,
            offset_min: 0,
        },
        message: "empty".to_owned(),
        metadata,
    }
}

#[test]
fn only_git_import_provenance_permits_empty_non_merge_changesets() {
    let parent = ChangesetId(ObjectId([1; 32]));
    let native = empty_changeset(vec![parent], Metadata::new());
    assert_eq!(
        native.check(&CheckOptions::default()).unwrap_err().rule,
        Rule::EmptyChangeset
    );

    let metadata = Metadata::from([(
        "mirror.git-commit".to_owned(),
        "0123456789012345678901234567890123456789".to_owned(),
    )]);
    empty_changeset(vec![parent], metadata.clone())
        .check(&CheckOptions::default())
        .unwrap();
    empty_changeset(Vec::new(), metadata)
        .check(&CheckOptions {
            allow_root: true,
            ..CheckOptions::default()
        })
        .unwrap();
}
