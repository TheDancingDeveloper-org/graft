//! Golden vectors: these ids and encodings are durable formats. If a test
//! here fails, an encoding changed. That needs a migration and an operator
//! decision, not a new vector.

use std::collections::BTreeMap;

use graft_model::{
    Author, AuthorKind, BlobId, Change, Changeset, ChangesetId, EntryKind, ObjectId, ObjectKind,
    RepoPath, Timestamp, Tree, TreeEntry, TreeId,
};

fn hex(b: &[u8]) -> String {
    b.iter().map(|b| format!("{b:02x}")).collect()
}

const BLOB: &[u8] = b"hello, graft\n";

fn golden_tree() -> Tree {
    Tree {
        entries: vec![
            TreeEntry {
                name: "README.md".into(),
                kind: EntryKind::File,
                id: BlobId::hash(BLOB).0,
            },
            TreeEntry {
                name: "run.sh".into(),
                kind: EntryKind::Exec,
                id: BlobId::hash(b"#!/bin/sh\n").0,
            },
            TreeEntry {
                name: "src".into(),
                kind: EntryKind::Tree,
                id: ObjectId([0x11; 32]),
            },
        ],
    }
}

fn golden_changeset() -> Changeset {
    Changeset {
        parents: vec![ChangesetId(ObjectId([0x22; 32]))],
        root: golden_tree().id(),
        changes: vec![
            Change::Put {
                path: RepoPath::new("README.md").unwrap(),
                kind: EntryKind::File,
                id: BlobId::hash(BLOB).0,
            },
            Change::Delete {
                path: RepoPath::new("old/notes.txt").unwrap(),
            },
        ],
        author: Author {
            name: "Ada".into(),
            email: "ada@example.com".into(),
            kind: AuthorKind::Agent,
        },
        authored_at: Timestamp {
            ms: 1_790_000_000_000,
            offset_min: 600,
        },
        message: "Update the readme".into(),
        metadata: BTreeMap::from([("vogt.work_item".to_owned(), "WI-1".to_owned())]),
    }
}

#[test]
fn golden_blob_id() {
    assert_eq!(
        BlobId::hash(BLOB).to_string(),
        "b5c1eb18777436c10d0c36067dcca7b83c1d96a19e31c3e0a98d7b0c7c79df34d"
    );
}

#[test]
fn golden_empty_blob_id() {
    assert_eq!(
        BlobId::hash(b"").to_string(),
        "b32e0968b89754373938e8cc2bcc2987494c94f0cdb5d0a006c981fceabb678f6"
    );
}

#[test]
fn golden_tree_encoding_and_id() {
    let tree = golden_tree();
    assert_eq!(
        hex(&tree.encode()),
        "01660009524541444d452e6d645c1eb18777436c10d0c36067dcca7b83c1d96a19e31c3e0a98d7b0c7c79df34d78000672756e2e7368d6b10947cafde9b6a67104ab2b0ae1584a17a3785c0e7f467e82a0285089850b6400037372631111111111111111111111111111111111111111111111111111111111111111"
    );
    assert_eq!(
        tree.id().to_string(),
        "td731993dd6a4345d5ddb4096c5189e9384595bfe277044753ead502a8cad030f"
    );
}

#[test]
fn golden_empty_tree_id() {
    assert_eq!(Tree::default().encode(), vec![1]);
    assert_eq!(
        Tree::default().id().to_string(),
        "td0a89364d3af4e3469f3f4f545b3fb47ef2c72f9f2daddeedbd1821d18a4234a"
    );
}

#[test]
fn golden_changeset_encoding_and_id() {
    let cs = golden_changeset();
    assert_eq!(
        hex(&cs.encode()),
        "01000000012222222222222222222222222222222222222222222222222222222222222222d731993dd6a4345d5ddb4096c5189e9384595bfe277044753ead502a8cad030f000000027000000009524541444d452e6d64665c1eb18777436c10d0c36067dcca7b83c1d96a19e31c3e0a98d7b0c7c79df34d640000000d6f6c642f6e6f7465732e747874000000034164610000000f616461406578616d706c652e636f6d01000001a0c4506c000258000000115570646174652074686520726561646d65000000010000000e766f67742e776f726b5f6974656d0000000457492d31"
    );
    assert_eq!(
        cs.id().to_string(),
        "c7495c0738bbe2413fd7073bd47478eafd90476a21d0613c75866bd28d667e859"
    );
    assert_eq!(Changeset::decode(&cs.encode()).unwrap(), cs);
}

#[test]
fn kinds_are_separated() {
    let payload = Tree::default().encode();
    let as_blob = ObjectId::hash(ObjectKind::Blob, &payload);
    let as_tree = ObjectId::hash(ObjectKind::Tree, &payload);
    let as_cs = ObjectId::hash(ObjectKind::Changeset, &payload);
    assert_ne!(as_blob, as_tree);
    assert_ne!(as_tree, as_cs);
    assert_ne!(as_blob, as_cs);
    assert_eq!(TreeId::hash(&payload).0, as_tree);
}

#[test]
fn id_is_blake3_derive_key() {
    assert_eq!(
        ObjectId::hash(ObjectKind::Blob, BLOB).0,
        blake3::derive_key("graft 1 blob", BLOB)
    );
}
