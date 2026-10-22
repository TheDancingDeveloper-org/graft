//! `Rebase` against a brute-force model over whole snapshots: random base,
//! target and two-draft stacks over paths that clash as file and directory,
//! with file, exec and symlink entries.

mod common;

use std::collections::{BTreeMap, BTreeSet};

use common::*;
use graft_model::{Change, ChangesetId, EntryKind, ObjectId, RepoPath};
use graft_proto::types::{MergeMode, Rebase, RefKind};
use graft_proto::{ApiError, RepoApi};
use graft_store::MetaStore;
use proptest::prelude::*;

const UNIVERSE: &[&str] = &["a", "a/b", "a/c", "a/b/x", "b", "b/y", "c"];

const CONTENTS: &[&[u8]] = &[b"one", b"two", b"three"];

const KINDS: &[EntryKind] = &[EntryKind::File, EntryKind::Exec, EntryKind::Symlink];

type Snapshot = BTreeMap<String, (EntryKind, ObjectId)>;

/// A valid snapshot from chosen entries: a path that is an ancestor of
/// another chosen path is dropped, and a sentinel keeps it non-empty.
fn snapshot(chosen: &[(usize, usize, usize)]) -> Snapshot {
    let mut out = Snapshot::new();
    for &(path, content, kind) in chosen {
        out.insert(
            UNIVERSE[path].to_owned(),
            (KINDS[kind], blob_id(CONTENTS[content])),
        );
    }
    let keys: Vec<String> = out.keys().cloned().collect();
    out.retain(|k, _| !keys.iter().any(|o| o.starts_with(&format!("{k}/"))));
    out.insert("zz".to_owned(), (EntryKind::File, blob_id(CONTENTS[0])));
    out
}

fn changes(old: &Snapshot, new: &Snapshot) -> Vec<Change> {
    let mut out: Vec<Change> = new
        .iter()
        .filter(|(p, e)| old.get(*p) != Some(e))
        .map(|(path, &(kind, id))| Change::Put {
            path: RepoPath::new(path).expect("path"),
            kind,
            id,
        })
        .collect();
    out.extend(old.keys().filter(|p| !new.contains_key(*p)).map(|p| del(p)));
    out.sort_by(|a, b| a.path().cmp(b.path()));
    out
}

/// What a snapshot holds at a path, as the rebase compares it: a file
/// entry, or nothing. Changes are file-level, so a directory (and anything
/// beneath a file) is nothing; clashes are found by applying.
#[derive(Debug, PartialEq, Eq)]
enum At {
    File(EntryKind, ObjectId),
    Absent,
}

fn at(s: &Snapshot, path: &str) -> At {
    match s.get(path) {
        Some(&(kind, id)) => At::File(kind, id),
        None => At::Absent,
    }
}

fn change_at(c: &Change) -> At {
    match c {
        Change::Put { kind, id, .. } => At::File(*kind, *id),
        Change::Delete { .. } => At::Absent,
    }
}

/// The model's answer.
#[derive(Debug)]
enum Outcome {
    /// The new snapshot and the changes kept (empty: the draft disappears).
    Rebased(Snapshot, Vec<Change>),
    /// Conflicts found by comparing entries, by path.
    Conflict(BTreeSet<String>),
    /// The kept changes make a path both a file and a directory.
    Clash,
}

fn valid(s: &Snapshot) -> bool {
    !s.keys()
        .any(|k| s.keys().any(|o| o.starts_with(&format!("{k}/"))))
}

/// The design §7.8 table, by brute force over whole snapshots.
fn model(base: &Snapshot, ours: &Snapshot, onto: &Snapshot) -> Outcome {
    let mut kept = Vec::new();
    let mut conflicts = BTreeSet::new();
    for change in changes(base, ours) {
        let path = change.path().as_str();
        let (b, o) = (at(base, path), at(onto, path));
        if b == o {
            kept.push(change);
        } else if change_at(&change) != o {
            conflicts.insert(path.to_owned());
        }
    }
    if !conflicts.is_empty() {
        return Outcome::Conflict(conflicts);
    }
    let mut out = onto.clone();
    for change in &kept {
        match change {
            Change::Put { path, kind, id } => {
                out.insert(path.to_string(), (*kind, *id));
            }
            Change::Delete { path } => {
                out.remove(path.as_str());
            }
        }
    }
    if valid(&out) {
        Outcome::Rebased(out, kept)
    } else {
        Outcome::Clash
    }
}

fn entries() -> impl Strategy<Value = Vec<(usize, usize, usize)>> {
    prop::collection::vec(
        (0..UNIVERSE.len(), 0..CONTENTS.len(), 0..KINDS.len()),
        0..UNIVERSE.len(),
    )
}

/// A few edits: a path, and its new entry or `None` to delete it.
type Edits = Vec<(usize, Option<(usize, usize)>)>;

fn edits() -> impl Strategy<Value = Edits> {
    prop::collection::vec(
        (
            0..UNIVERSE.len(),
            prop::option::of((0..CONTENTS.len(), 0..KINDS.len())),
        ),
        1..4,
    )
}

/// `s` with `edits` applied. A put replaces whatever is above or below its
/// path, so the result stays a valid snapshot; that is how files become
/// directories and back.
fn edit(s: &Snapshot, edits: &Edits) -> Snapshot {
    let mut out = s.clone();
    for &(path, entry) in edits {
        let path = UNIVERSE[path];
        match entry {
            None => {
                out.remove(path);
            }
            Some((content, kind)) => {
                out.retain(|k, _| {
                    !path.starts_with(&format!("{k}/")) && !k.starts_with(&format!("{path}/"))
                });
                out.insert(path.to_owned(), (KINDS[kind], blob_id(CONTENTS[content])));
            }
        }
    }
    out
}

fn commit_on<M: MetaStore>(
    repo: &Repo<'_, M>,
    parent: (ChangesetId, &Snapshot),
    s: &Snapshot,
) -> ChangesetId {
    let changes = changes(parent.1, s);
    if changes.is_empty() {
        return parent.0;
    }
    repo.try_commit(vec![parent.0], changes).expect("commit").id
}

fn rebase<M: MetaStore>(
    repo: &Repo<'_, M>,
    c: ChangesetId,
    onto: ChangesetId,
) -> Result<ChangesetId, ApiError> {
    block_on(repo.e().rebase(
        &repo.cx,
        Rebase {
            repo: "m".to_owned(),
            changeset: c,
            onto,
            merge: MergeMode::File,
        },
    ))
}

proptest! {
    // About a fifth of the inputs make no draft change and are rejected,
    // so allow enough rejects for a long run (PROPTEST_CASES=5000 and up).
    #![proptest_config(ProptestConfig {
        max_global_rejects: 1 << 20,
        ..ProptestConfig::with_cases(512)
    })]

    #[test]
    fn rebase_matches_the_model(
        base in entries(),
        onto in edits(),
        ours in edits(),
        upper in edits(),
    ) {
        let f = memory();
        let repo = Repo::new(&f, "m");
        repo.upload(CONTENTS);
        let base = snapshot(&base);
        let (onto, ours) = (edit(&base, &onto), edit(&base, &ours));
        let upper = edit(&ours, &upper);
        prop_assume!(!changes(&base, &ours).is_empty());

        let b = repo
            .try_commit(vec![], changes(&Snapshot::new(), &base))
            .expect("base")
            .id;
        repo.update_ref("main", None, b, RefKind::Line).expect("main");
        let o = commit_on(&repo, (b, &base), &onto);
        let c1 = commit_on(&repo, (b, &base), &ours);
        let c2 = commit_on(&repo, (c1, &ours), &upper);

        // Onto its own base, a stack is unchanged.
        prop_assert_eq!(rebase(&repo, c2, b), Ok(c2));

        // The model, draft by draft, bottom up. A draft whose changes all
        // disappear leaves the target as the next draft's parent.
        let mut expected = match model(&base, &ours, &onto) {
            Outcome::Rebased(s, _) => Ok(s),
            other => Err(other),
        };
        if let (true, Ok(r1)) = (c2 != c1, &expected) {
            expected = match model(&ours, &upper, r1) {
                Outcome::Rebased(s, _) => Ok(s),
                other => Err(other),
            };
        }

        let journal = repo.journal().len();
        let got = rebase(&repo, c2, o);
        match (&expected, &got) {
            (Ok(snap), Ok(id)) => {
                // Compare roots: commit the model's snapshot on the target.
                let want = commit_on(&repo, (o, &onto), snap);
                prop_assert_eq!(repo.changeset(*id).changeset.root, repo.changeset(want).changeset.root);
                // Repeating returns the same id and writes nothing more.
                let journal = repo.journal().len();
                prop_assert_eq!(rebase(&repo, c2, o), Ok(*id));
                prop_assert_eq!(repo.journal().len(), journal);
                // The base is unchanged: the drafts come back as they were.
                if o == b {
                    prop_assert_eq!(*id, c2);
                }
            }
            (Err(Outcome::Conflict(want)), Err(ApiError::Conflict { paths, onto: at })) => {
                prop_assert_eq!(*at, o);
                let got: BTreeSet<String> = paths.iter().map(|p| p.path.clone()).collect();
                prop_assert_eq!(&got, want);
                prop_assert_eq!(repo.journal().len(), journal);
            }
            (Err(Outcome::Clash), Err(ApiError::Conflict { paths, .. })) => {
                prop_assert_eq!(paths.len(), 1);
                prop_assert_eq!(repo.journal().len(), journal);
            }
            _ => prop_assert!(false, "model {expected:?}, engine {got:?}"),
        }
    }

    /// A single draft over a disjoint target: the result is the target
    /// plus exactly the draft's changes.
    #[test]
    fn a_rebased_snapshot_is_the_sum_of_its_changes(
        base in entries(),
        onto in edits(),
        ours in edits(),
    ) {
        let f = memory();
        let repo = Repo::new(&f, "m");
        repo.upload(CONTENTS);
        let base = snapshot(&base);
        let (onto, ours) = (edit(&base, &onto), edit(&base, &ours));
        let mine = changes(&base, &ours);
        prop_assume!(!mine.is_empty());
        let b = repo
            .try_commit(vec![], changes(&Snapshot::new(), &base))
            .expect("base")
            .id;
        repo.update_ref("main", None, b, RefKind::Line).expect("main");
        let o = commit_on(&repo, (b, &base), &onto);
        let c = commit_on(&repo, (b, &base), &ours);
        if let Outcome::Rebased(_, kept) = model(&base, &ours, &onto) {
            let id = rebase(&repo, c, o).expect("rebase");
            if kept.is_empty() {
                prop_assert_eq!(id, o);
            } else if o != b {
                prop_assert_eq!(&repo.changeset(id).changeset.changes, &kept);
                prop_assert_eq!(repo.changeset(id).changeset.parents, vec![o]);
            }
        }
    }
}
