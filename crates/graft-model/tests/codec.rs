//! Canonical encodings: decoders accept exactly what the encoders produce.

use proptest::prelude::*;

use graft_model::changeset::DecodeError;
use graft_model::{
    Author, AuthorKind, Change, Changeset, ChangesetId, EntryKind, Metadata, ObjectId, RepoPath,
    Timestamp, Tree, TreeEntry, TreeId,
};

fn arb_id() -> impl Strategy<Value = ObjectId> {
    any::<[u8; 32]>().prop_map(ObjectId)
}

fn arb_name() -> impl Strategy<Value = String> {
    "[a-zA-Z0-9._ é-]{1,12}".prop_filter("not a dot component", |s| s != "." && s != "..")
}

fn arb_kind() -> impl Strategy<Value = EntryKind> {
    prop_oneof![
        Just(EntryKind::File),
        Just(EntryKind::Exec),
        Just(EntryKind::Symlink),
        Just(EntryKind::Tree),
        Just(EntryKind::Submodule),
    ]
}

fn arb_tree() -> impl Strategy<Value = Tree> {
    prop::collection::btree_map(arb_name(), (arb_kind(), arb_id()), 0..20).prop_map(|m| Tree {
        entries: m
            .into_iter()
            .map(|(name, (kind, id))| TreeEntry { name, kind, id })
            .collect(),
    })
}

fn arb_path() -> impl Strategy<Value = RepoPath> {
    prop::collection::vec(arb_name(), 1..4).prop_map(|c| RepoPath::new(&c.join("/")).unwrap())
}

fn arb_changeset() -> impl Strategy<Value = Changeset> {
    let change =
        (arb_path(), prop::option::of((arb_kind(), arb_id()))).prop_map(|(path, put)| match put {
            Some((kind, id)) if kind != EntryKind::Tree => Change::Put { path, kind, id },
            _ => Change::Delete { path },
        });
    (
        prop::collection::vec(arb_id(), 0..4),
        arb_id(),
        prop::collection::vec(change, 0..10),
        (".*", ".*", 0u8..4),
        any::<(i64, i16)>(),
        ".*",
        prop::collection::btree_map(".*", ".*", 0..4),
    )
        .prop_map(
            |(parents, root, mut changes, (name, email, k), (ms, off), message, md)| {
                changes.sort_by(|a, b| {
                    a.path()
                        .as_str()
                        .as_bytes()
                        .cmp(b.path().as_str().as_bytes())
                });
                changes.dedup_by(|a, b| a.path() == b.path());
                Changeset {
                    parents: parents.into_iter().map(ChangesetId).collect(),
                    root: TreeId(root),
                    changes,
                    author: Author {
                        name,
                        email,
                        kind: AuthorKind::from_byte(k).unwrap(),
                    },
                    authored_at: Timestamp {
                        ms,
                        offset_min: off,
                    },
                    message,
                    metadata: md.into_iter().collect::<Metadata>(),
                }
            },
        )
}

proptest! {
    #[test]
    fn tree_round_trips(tree in arb_tree()) {
        let bytes = tree.encode();
        prop_assert_eq!(Tree::decode(&bytes), Ok(tree));
    }

    #[test]
    fn changeset_round_trips(cs in arb_changeset()) {
        let bytes = cs.encode();
        let back = Changeset::decode(&bytes).unwrap();
        prop_assert_eq!(back.id(), cs.id());
        prop_assert_eq!(back, cs);
    }

    #[test]
    fn tree_decoder_never_panics(bytes in prop::collection::vec(any::<u8>(), 0..200)) {
        if let Ok(tree) = Tree::decode(&bytes) {
            prop_assert_eq!(tree.encode(), bytes);
        }
    }

    #[test]
    fn changeset_decoder_never_panics(bytes in prop::collection::vec(any::<u8>(), 0..300)) {
        if let Ok(cs) = Changeset::decode(&bytes) {
            prop_assert_eq!(cs.encode(), bytes);
        }
    }

    #[test]
    fn truncated_changesets_are_rejected(cs in arb_changeset(), cut in any::<prop::sample::Index>()) {
        let bytes = cs.encode();
        let n = cut.index(bytes.len());
        prop_assert!(Changeset::decode(&bytes[..n]).is_err());
    }
}

fn entry(name: &str, kind: u8) -> Vec<u8> {
    let mut v = vec![kind];
    v.extend_from_slice(&(name.len() as u16).to_be_bytes());
    v.extend_from_slice(name.as_bytes());
    v.extend_from_slice(&[7; 32]);
    v
}

#[test]
fn tree_decoder_rejects_non_canonical() {
    assert_eq!(Tree::decode(&[]), Err(DecodeError::Truncated));
    assert_eq!(Tree::decode(&[2]), Err(DecodeError::Version(2)));
    let unsorted = [vec![1], entry("b", b'f'), entry("a", b'f')].concat();
    assert_eq!(
        Tree::decode(&unsorted),
        Err(DecodeError::NotCanonical("entry-order"))
    );
    let dup = [vec![1], entry("a", b'f'), entry("a", b'x')].concat();
    assert_eq!(
        Tree::decode(&dup),
        Err(DecodeError::NotCanonical("entry-order"))
    );
    let kind = [vec![1], entry("a", b'q')].concat();
    assert_eq!(
        Tree::decode(&kind),
        Err(DecodeError::NotCanonical("entry-kind"))
    );
    for bad in ["", ".", "..", "a/b", "a\0b"] {
        let v = [vec![1], entry(bad, b'f')].concat();
        assert_eq!(
            Tree::decode(&v),
            Err(DecodeError::NotCanonical("entry-name")),
            "{bad:?}"
        );
    }
    let mut short = [vec![1], entry("a", b'f')].concat();
    short.pop();
    assert_eq!(Tree::decode(&short), Err(DecodeError::Truncated));
    let mut not_utf8 = [vec![1], entry("ab", b'f')].concat();
    not_utf8[4] = 0xff;
    assert_eq!(
        Tree::decode(&not_utf8),
        Err(DecodeError::NotCanonical("utf8"))
    );
}

fn simple() -> Changeset {
    Changeset {
        parents: vec![],
        root: TreeId(ObjectId([0; 32])),
        changes: vec![Change::Delete {
            path: RepoPath::new("a").unwrap(),
        }],
        author: Author {
            name: String::new(),
            email: String::new(),
            kind: AuthorKind::Human,
        },
        authored_at: Timestamp {
            ms: 0,
            offset_min: 0,
        },
        message: String::new(),
        metadata: Metadata::from([("k".into(), "v".into())]),
    }
}

#[test]
fn changeset_decoder_rejects_non_canonical() {
    let good = simple().encode();
    // Layout: version(1) n(4) root(32) n(4) 'd' len(4) "a" name(4) email(4)
    // kind(1) ms(8) off(2) msg(4) n(4) "k" "v".
    let change_tag = 1 + 4 + 32 + 4;
    let author_kind = change_tag + 1 + 4 + 1 + 4 + 4;

    let mut v = good.clone();
    v[0] = 9;
    assert_eq!(Changeset::decode(&v), Err(DecodeError::Version(9)));

    let mut v = good.clone();
    v.push(0);
    assert_eq!(Changeset::decode(&v), Err(DecodeError::TrailingBytes));

    let mut v = good.clone();
    v[change_tag] = b'x';
    assert_eq!(
        Changeset::decode(&v),
        Err(DecodeError::NotCanonical("change-tag"))
    );

    let mut v = good.clone();
    v[change_tag + 5] = b'/';
    assert_eq!(
        Changeset::decode(&v),
        Err(DecodeError::NotCanonical("change-path"))
    );

    let mut v = good.clone();
    v[author_kind] = 4;
    assert_eq!(
        Changeset::decode(&v),
        Err(DecodeError::NotCanonical("author-kind"))
    );

    // A huge count with no bytes behind it is rejected before allocating.
    let mut v = good.clone();
    v[1..5].copy_from_slice(&u32::MAX.to_be_bytes());
    assert_eq!(Changeset::decode(&v), Err(DecodeError::Truncated));
}

#[test]
fn changeset_decoder_rejects_unsorted_or_duplicate_changes() {
    let mut cs = simple();
    cs.changes = vec![
        Change::Delete {
            path: RepoPath::new("b").unwrap(),
        },
        Change::Delete {
            path: RepoPath::new("a").unwrap(),
        },
    ];
    assert_eq!(
        Changeset::decode(&cs.encode()),
        Err(DecodeError::NotCanonical("change-order"))
    );
    cs.changes[0] = Change::Delete {
        path: RepoPath::new("a").unwrap(),
    };
    assert_eq!(
        Changeset::decode(&cs.encode()),
        Err(DecodeError::NotCanonical("change-order"))
    );
}

#[test]
fn changeset_decoder_rejects_put_of_a_tree() {
    let mut cs = simple();
    cs.changes = vec![Change::Put {
        path: RepoPath::new("a").unwrap(),
        kind: EntryKind::Tree,
        id: ObjectId([1; 32]),
    }];
    assert_eq!(
        Changeset::decode(&cs.encode()),
        Err(DecodeError::NotCanonical("put-kind"))
    );
}

#[test]
fn changeset_decoder_rejects_unsorted_metadata() {
    let mut bytes = simple().encode();
    // Append a second metadata pair "a"="v" after "k" and bump the count.
    let count_at = bytes.len() - (4 + 4 + 1 + 4 + 1);
    bytes[count_at..count_at + 4].copy_from_slice(&2u32.to_be_bytes());
    for s in ["a", "v"] {
        bytes.extend_from_slice(&(s.len() as u32).to_be_bytes());
        bytes.extend_from_slice(s.as_bytes());
    }
    assert_eq!(
        Changeset::decode(&bytes),
        Err(DecodeError::NotCanonical("metadata-order"))
    );
}
