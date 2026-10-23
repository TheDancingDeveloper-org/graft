//! G3.7 properties over random snapshots: a revert of a revert restores the
//! original tree, and stack operations keep the stack's snapshot.

mod common;

use std::collections::BTreeMap;

use common::*;
use graft_model::{Change, ChangesetId, EntryKind, ObjectId, RepoPath, TreeId};
use graft_proto::types::{
    CreateChangeset, OpenWorkspace, RefKind, Revert, StackOp, WorkspaceRef, WorkspaceView,
};
use graft_proto::{ApiError, RepoApi, WorkspaceApi};
use graft_store::MetaStore;
use proptest::prelude::*;

const UNIVERSE: &[&str] = &["a", "a/b", "a/c", "b", "b/y", "c", "d"];

const CONTENTS: &[&[u8]] = &[b"1\n2\n3\n", b"one\n2\n3\n", b"1\n2\nthree\n", b"x"];

const KINDS: &[EntryKind] = &[EntryKind::File, EntryKind::Exec, EntryKind::Symlink];

type Snapshot = BTreeMap<String, (EntryKind, ObjectId)>;

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

/// `s` with `edits` applied, kept a valid snapshot (a put replaces what is
/// above or below its path).
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

struct P<'f, M> {
    r: Repo<'f, M>,
    root: ChangesetId,
    base: Snapshot,
}

impl<'f, M: MetaStore> P<'f, M> {
    fn new(f: &'f Fixture<M>, base: &Edits) -> Self {
        let r = Repo::new(f, "p");
        r.upload(CONTENTS);
        // A sentinel keeps the base non-empty.
        let mut start = Snapshot::new();
        start.insert("z".to_owned(), (EntryKind::File, blob_id(CONTENTS[0])));
        let base = edit(&start, base);
        let root = r
            .try_commit(vec![], changes(&Snapshot::new(), &base))
            .expect("root")
            .id;
        r.update_ref("main", None, root, RefKind::Line)
            .expect("main");
        Self { r, root, base }
    }

    fn root_of(&self, id: ChangesetId) -> TreeId {
        self.r.changeset(id).changeset.root
    }

    /// Commits `to` over `from` at `parent`, in workspace `ws` when given.
    fn commit(
        &self,
        parent: ChangesetId,
        from: &Snapshot,
        to: &Snapshot,
        ws: Option<&str>,
    ) -> Option<ChangesetId> {
        self.commit_as(parent, from, to, ws, "change")
    }

    /// [`P::commit`] with a message, so equal content can make a new id.
    fn commit_as(
        &self,
        parent: ChangesetId,
        from: &Snapshot,
        to: &Snapshot,
        ws: Option<&str>,
        message: &str,
    ) -> Option<ChangesetId> {
        let changes = changes(from, to);
        if changes.is_empty() {
            return None;
        }
        let mut req: CreateChangeset = commit_req("p", vec![parent], changes);
        req.workspace = ws.map(str::to_owned);
        req.message = message.to_owned();
        Some(
            block_on(self.r.e().create_changeset(&self.r.cx, req))
                .expect("commit")
                .id,
        )
    }

    fn revert(&self, c: ChangesetId) -> ChangesetId {
        block_on(self.r.e().revert(
            &self.r.cx,
            Revert {
                repo: "p".to_owned(),
                changeset: c,
                onto: c,
                workspace: None,
            },
        ))
        .expect("revert")
    }

    fn wref(&self) -> WorkspaceRef {
        WorkspaceRef {
            repo: "p".to_owned(),
            name: "w".to_owned(),
        }
    }

    fn open(&self) {
        block_on(self.r.e().open_workspace(
            &self.r.cx,
            OpenWorkspace {
                repo: "p".to_owned(),
                name: Some("w".to_owned()),
                line: "main".to_owned(),
                at: None,
                lease_ms: 0,
            },
        ))
        .expect("open");
    }

    fn stack(&self, op: StackOp) -> Result<WorkspaceView, ApiError> {
        block_on(self.r.e().stack(&self.r.cx, self.wref(), op))
    }

    fn ws(&self) -> WorkspaceView {
        block_on(self.r.e().get_workspace(&self.r.cx, self.wref())).expect("workspace")
    }

    /// The workspace's drafts, bottom first.
    fn drafts(&self) -> Vec<ChangesetId> {
        let ws = self.ws();
        let mut out = Vec::new();
        let mut cur = ws.head;
        while let Some(id) = cur.filter(|id| *id != ws.base) {
            out.push(id);
            cur = self.r.changeset(id).changeset.first_parent();
        }
        out.reverse();
        out
    }

    /// The workspace's top snapshot root.
    fn top(&self) -> TreeId {
        let ws = self.ws();
        self.root_of(ws.head.unwrap_or(ws.base))
    }
}

proptest! {
    #![proptest_config(ProptestConfig {
        max_global_rejects: 1 << 20,
        ..ProptestConfig::with_cases(256)
    })]

    /// Reverting `c` gives its parent's tree; reverting that revert gives
    /// `c`'s tree again.
    #[test]
    fn revert_of_revert_restores_the_original_tree(base in edits(), change in edits()) {
        let f = memory();
        let p = P::new(&f, &base);
        let next = edit(&p.base, &change);
        let Some(c) = p.commit(p.root, &p.base, &next, None) else {
            return Err(TestCaseError::reject("no change"));
        };
        let r1 = p.revert(c);
        prop_assert_eq!(p.root_of(r1), p.root_of(p.root));
        let r2 = p.revert(r1);
        prop_assert_eq!(p.root_of(r2), p.root_of(c));
        // Deterministic.
        prop_assert_eq!(p.revert(c), r1);
    }

    /// Squashing every draft keeps the top snapshot in at most one draft;
    /// a reorder that succeeds keeps it too when the drafts touch disjoint
    /// paths; dropping and re-reading keeps the stack well formed.
    #[test]
    fn stack_ops_keep_the_snapshot(
        base in edits(),
        e1 in edits(),
        e2 in edits(),
        e3 in edits(),
        order in Just([2usize, 0, 1]).prop_shuffle(),
        drop_at in 0usize..3,
    ) {
        let f = memory();
        let p = P::new(&f, &base);
        p.open();
        let s1 = edit(&p.base, &e1);
        let s2 = edit(&s1, &e2);
        let s3 = edit(&s2, &e3);
        let Some(d1) = p.commit(p.root, &p.base, &s1, Some("w")) else {
            return Err(TestCaseError::reject("no first change"));
        };
        let Some(d2) = p.commit(d1, &s1, &s2, Some("w")) else {
            return Err(TestCaseError::reject("no second change"));
        };
        let Some(d3) = p.commit(d2, &s2, &s3, Some("w")) else {
            return Err(TestCaseError::reject("no third change"));
        };
        let drafts = [d1, d2, d3];
        let top = p.top();
        prop_assert_eq!(p.drafts(), drafts.to_vec());

        let paths = |a: &Snapshot, b: &Snapshot| -> Vec<String> {
            changes(a, b).iter().map(|c| c.path().to_string()).collect()
        };
        let touched = [paths(&p.base, &s1), paths(&s1, &s2), paths(&s2, &s3)];
        let disjoint = (0..3).all(|i| {
            (0..3).all(|j| i == j || touched[i].iter().all(|t| !touched[j].contains(t)))
        });
        let reordered: Vec<ChangesetId> = order.iter().map(|&i| drafts[i]).collect();
        match p.stack(StackOp::Reorder(reordered)) {
            Ok(_) => {
                if disjoint {
                    prop_assert_eq!(p.top(), top);
                }
                prop_assert!(p.drafts().len() <= 3);
            }
            Err(ApiError::Conflict { .. }) => prop_assert_eq!(p.drafts(), drafts.to_vec()),
            Err(e) => prop_assert!(false, "reorder failed: {e:?}"),
        }

        // Squash everything: one draft (or none) with the same top.
        let now_top = p.top();
        let all = p.drafts();
        p.stack(StackOp::Squash(all)).expect("squash all");
        prop_assert_eq!(p.top(), now_top);
        prop_assert!(p.drafts().len() <= 1);

        // Empty the stack, rebuild the three drafts, and drop one: the
        // drafts below it are untouched, and the stack shrinks by one or
        // the drop conflicts and changes nothing.
        if let Some(squashed) = p.drafts().first().copied() {
            p.stack(StackOp::Drop(squashed)).expect("drop the squashed draft");
        }
        prop_assert_eq!(p.ws().head, None);
        // New messages: the first drafts' ids are superseded.
        let d1 = p.commit_as(p.root, &p.base, &s1, Some("w"), "again").expect("again 1");
        let d2 = p.commit_as(d1, &s1, &s2, Some("w"), "again").expect("again 2");
        let d3 = p.commit_as(d2, &s2, &s3, Some("w"), "again").expect("again 3");
        let drafts = [d1, d2, d3];
        match p.stack(StackOp::Drop(drafts[drop_at])) {
            Ok(_) => {
                let now = p.drafts();
                prop_assert!(now.len() <= 2);
                prop_assert_eq!(&now[..drop_at], &drafts[..drop_at]);
            }
            Err(ApiError::Conflict { .. }) => prop_assert_eq!(p.drafts(), drafts.to_vec()),
            Err(e) => prop_assert!(false, "drop failed: {e:?}"),
        }
    }
}
