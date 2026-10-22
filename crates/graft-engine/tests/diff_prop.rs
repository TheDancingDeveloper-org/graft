//! `Diff` against a brute-force comparison of whole snapshots: every page
//! size, with and without path filters, including files that become
//! directories and back.

mod common;

use std::collections::BTreeMap;

use common::*;
use graft_model::{Change, ChangesetId, EntryKind, ObjectId, RepoPath};
use graft_proto::RepoApi;
use graft_proto::types::{Diff, Page};
use graft_store::MetaStore;
use proptest::prelude::*;

/// Paths that clash with each other as file and directory, so snapshots
/// built from them exercise every shape change.
const UNIVERSE: &[&str] = &[
    "a", "a/b", "a/c", "a/b/x", "a.b", "b", "b/y", "c/d/e", "c/f", "d",
];

const PREFIXES: &[&str] = &["a", "a/b", "b", "c/d", "a.b", "zz"];

const CONTENTS: &[&[u8]] = &[b"one", b"two", b"three"];

type Snapshot = BTreeMap<String, (EntryKind, ObjectId)>;

/// A valid snapshot from chosen entries: a path that is an ancestor of
/// another chosen path is dropped.
fn snapshot(chosen: &[(usize, usize, bool)]) -> Snapshot {
    let mut out = Snapshot::new();
    for &(path, content, exec) in chosen {
        let kind = if exec {
            EntryKind::Exec
        } else {
            EntryKind::File
        };
        out.insert(
            UNIVERSE[path].to_owned(),
            (kind, blob_id(CONTENTS[content])),
        );
    }
    let keys: Vec<String> = out.keys().cloned().collect();
    out.retain(|k, _| !keys.iter().any(|o| o.starts_with(&format!("{k}/"))));
    // A sentinel keeps every snapshot non-empty.
    out.insert(
        "zz/base".to_owned(),
        (EntryKind::File, blob_id(CONTENTS[0])),
    );
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

/// The brute-force diff: compare every path of both snapshots.
fn brute(from: &Snapshot, to: &Snapshot, prefixes: &[RepoPath]) -> Vec<Change> {
    changes(from, to)
        .into_iter()
        .filter(|c| prefixes.is_empty() || prefixes.iter().any(|q| c.path().starts_with(q)))
        .collect()
}

fn entries() -> impl Strategy<Value = Vec<(usize, usize, bool)>> {
    prop::collection::vec(
        (0..UNIVERSE.len(), 0..CONTENTS.len(), any::<bool>()),
        0..UNIVERSE.len(),
    )
}

fn commit_snapshot<M: MetaStore>(
    repo: &Repo<'_, M>,
    base: (ChangesetId, &Snapshot),
    s: &Snapshot,
) -> ChangesetId {
    let changes = changes(base.1, s);
    if changes.is_empty() {
        return base.0;
    }
    repo.try_commit(vec![base.0], changes).expect("commit").id
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(128))]

    #[test]
    fn diff_matches_brute_force(
        from in entries(),
        to in entries(),
        limit in 1u32..5,
        filters in prop::collection::vec(0..PREFIXES.len(), 0..3),
    ) {
        let f = memory();
        let repo = Repo::new(&f, "p");
        repo.upload(CONTENTS);
        let empty = snapshot(&[]);
        let base = repo.try_commit(vec![], changes(&Snapshot::new(), &empty)).expect("base").id;
        let (from, to) = (snapshot(&from), snapshot(&to));
        let a = commit_snapshot(&repo, (base, &empty), &from);
        let b = commit_snapshot(&repo, (base, &empty), &to);
        let prefixes: Vec<RepoPath> = filters.iter().map(|&i| p(PREFIXES[i])).collect();

        let mut got = Vec::new();
        let mut token = None;
        loop {
            let page = block_on(repo.e().diff(&repo.cx, Diff {
                repo: "p".to_owned(),
                from: a,
                to: b,
                paths: prefixes.clone(),
                patch: None,
                page: Page { limit, token },
            })).expect("diff");
            prop_assert!(page.files.len() <= limit as usize);
            got.extend(page.files.into_iter().map(|f| f.change));
            match page.next {
                Some(t) => token = Some(t),
                None => break,
            }
        }
        prop_assert_eq!(got, brute(&from, &to, &prefixes));
    }
}
