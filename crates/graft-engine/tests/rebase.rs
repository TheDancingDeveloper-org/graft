//! G2.7: file-level `Rebase` and `Sync`, on both stores, plus races,
//! crashes and read counting.

mod common;

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicU8, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use common::*;
use graft_engine::{EngineConfig, REBASED_FROM};
use graft_model::{Change, ChangesetId, EntryKind, ObjectId, RepoPath};
use graft_proto::error::{ApiResult, ConflictPath};
use graft_proto::types::*;
use graft_proto::{ApiError, CallContext, RepoApi, WorkspaceApi};
use graft_store::memory::MemoryStore;
use graft_store::{
    ChangesetRecord, DoomMark, Lsn, MetaStore, MetaTxn, Protection, RecordState, RefRecord, RepoId,
    StoreError, StoreResult, TimedRow, TxnOutcome, WorkspaceRecord, Write,
};

/// A repository with `main` at a landed root holding `files`.
struct R<'f, M> {
    repo: Repo<'f, M>,
    root: ChangesetId,
}

impl<'f, M: MetaStore> R<'f, M> {
    fn new(f: &'f Fixture<M>, files: &[(&str, &[u8])]) -> Self {
        let repo = Repo::new(f, "r");
        let root = repo.commit(vec![], files, &[]).id;
        repo.update_ref("main", None, root, RefKind::Line)
            .expect("main");
        Self { repo, root }
    }

    /// Commits on `parent` without a workspace.
    fn commit(
        &self,
        parent: ChangesetId,
        files: &[(&str, &[u8])],
        deletes: &[&str],
    ) -> ChangesetId {
        self.repo.commit(vec![parent], files, deletes).id
    }

    /// Commits raw changes (any kind) on `parent`.
    fn commit_changes(&self, parent: ChangesetId, mut changes: Vec<Change>) -> ChangesetId {
        changes.sort_by(|a, b| a.path().cmp(b.path()));
        self.repo
            .try_commit(vec![parent], changes)
            .expect("commit")
            .id
    }

    /// Lands `files` on `main`, returning the new tip.
    fn land(&self, files: &[(&str, &[u8])], deletes: &[&str]) -> ChangesetId {
        let tip = self.tip();
        let next = self.commit(tip, files, deletes);
        self.repo
            .update_ref("main", Some(tip), next, RefKind::Line)
            .expect("land");
        next
    }

    fn land_changes(&self, changes: Vec<Change>) -> ChangesetId {
        let tip = self.tip();
        let next = self.commit_changes(tip, changes);
        self.repo
            .update_ref("main", Some(tip), next, RefKind::Line)
            .expect("land");
        next
    }

    fn tip(&self) -> ChangesetId {
        block_on(self.repo.e().resolve_ref(
            &self.repo.cx,
            ResolveRef {
                repo: "r".to_owned(),
                name: "main".to_owned(),
            },
        ))
        .expect("resolve")
        .target
    }

    fn rebase_as(
        &self,
        cx: &CallContext,
        c: ChangesetId,
        onto: ChangesetId,
    ) -> ApiResult<ChangesetId> {
        block_on(self.repo.e().rebase(
            cx,
            Rebase {
                repo: "r".to_owned(),
                changeset: c,
                onto,
                merge: MergeMode::File,
                dry_run: false,
            },
        ))
    }

    fn rebase(&self, c: ChangesetId, onto: ChangesetId) -> ApiResult<ChangesetId> {
        self.rebase_as(&self.repo.cx, c, onto)
    }

    /// A line-mode rebase, a dry run or not.
    fn rebase_line(
        &self,
        c: ChangesetId,
        onto: ChangesetId,
        dry_run: bool,
    ) -> ApiResult<ChangesetId> {
        block_on(self.repo.e().rebase(
            &self.repo.cx,
            Rebase {
                repo: "r".to_owned(),
                changeset: c,
                onto,
                merge: MergeMode::Line,
                dry_run,
            },
        ))
    }

    /// A changeset's pins and expiry.
    fn protection(&self, id: ChangesetId) -> Protection {
        let meta = self.repo.e().stores().0;
        let repo = block_on(meta.repo("r")).expect("repo").expect("exists");
        block_on(meta.protection(repo, id))
            .expect("protection")
            .expect("stored")
    }

    /// The number of stored object frames.
    fn frames(&self) -> usize {
        self.repo.f.objects.frames.lock().expect("frames").len()
    }

    /// The snapshot at `id`, as path → (kind, contents).
    fn snapshot(&self, id: ChangesetId) -> BTreeMap<String, Vec<u8>> {
        self.repo.fetch(self.repo.changeset(id).changeset.root)
    }

    fn wref(&self, name: &str) -> WorkspaceRef {
        WorkspaceRef {
            repo: "r".to_owned(),
            name: name.to_owned(),
        }
    }

    fn open(&self, name: &str) -> WorkspaceView {
        self.open_as(&self.repo.cx, name).expect("open")
    }

    fn open_as(&self, cx: &CallContext, name: &str) -> ApiResult<WorkspaceView> {
        block_on(self.repo.e().open_workspace(
            cx,
            OpenWorkspace {
                repo: "r".to_owned(),
                name: Some(name.to_owned()),
                line: "main".to_owned(),
                at: None,
                lease_ms: 0,
            },
        ))
    }

    /// Commits `files` into workspace `ws` on `parent`.
    fn commit_in(&self, ws: &str, parent: ChangesetId, files: &[(&str, &[u8])]) -> ChangesetId {
        let blobs: Vec<&[u8]> = files.iter().map(|(_, b)| *b).collect();
        self.repo.upload(&blobs);
        let mut changes: Vec<Change> = files.iter().map(|(p, b)| put(p, b)).collect();
        changes.sort_by(|a, b| a.path().cmp(b.path()));
        let mut req = commit_req("r", vec![parent], changes);
        req.workspace = Some(ws.to_owned());
        block_on(self.repo.e().create_changeset(&self.repo.cx, req))
            .expect("commit in workspace")
            .id
    }

    fn sync_as(&self, cx: &CallContext, ws: &str, onto: Option<ChangesetId>) -> ApiResult<Synced> {
        block_on(self.repo.e().sync(cx, self.wref(ws), onto))
    }

    fn sync(&self, ws: &str) -> ApiResult<Synced> {
        self.sync_as(&self.repo.cx, ws, None)
    }

    fn get(&self, ws: &str) -> WorkspaceView {
        block_on(self.repo.e().get_workspace(&self.repo.cx, self.wref(ws))).expect("get")
    }

    fn view(&self, id: ChangesetId) -> ChangesetView {
        self.repo.changeset(id)
    }

    fn journal_len(&self) -> usize {
        self.repo.journal().len()
    }
}

fn paths(conflict: ApiResult<ChangesetId>) -> Vec<String> {
    match conflict {
        Err(ApiError::Conflict { paths, .. }) => paths.into_iter().map(|p| p.path).collect(),
        other => panic!("expected a conflict, got {other:?}"),
    }
}

fn snap(files: &[(&str, &[u8])]) -> BTreeMap<String, Vec<u8>> {
    files
        .iter()
        .map(|(p, b)| ((*p).to_owned(), b.to_vec()))
        .collect()
}

fn disjoint_changes_apply_on_the_target<M: MetaStore>(f: &Fixture<M>) {
    let r = R::new(f, &[("a", b"1"), ("b", b"1")]);
    let c = r.commit(r.root, &[("a", b"2"), ("new/x", b"x")], &[]);
    let onto = r.land(&[("b", b"2"), ("c", b"1")], &[]);

    let rebased = r.rebase(c, onto).expect("rebase");
    assert_ne!(rebased, c);
    assert_eq!(
        r.snapshot(rebased),
        snap(&[("a", b"2"), ("b", b"2"), ("c", b"1"), ("new/x", b"x")])
    );
    let view = r.view(rebased);
    let original = r.view(c).changeset;
    let cs = view.changeset;
    assert_eq!(cs.parents, vec![onto]);
    assert_eq!(cs.changes, original.changes);
    assert_eq!(cs.author, original.author);
    assert_eq!(cs.authored_at, original.authored_at);
    assert_eq!(cs.message, original.message);
    assert_eq!(
        cs.metadata.get(REBASED_FROM).map(String::as_str),
        Some(c.to_string().as_str())
    );
    // Nothing holds it until a workspace adopts it.
    assert_eq!(view.state, DraftState::Abandoned);

    // Deterministic: a retry returns the same id and writes nothing.
    let journal = r.journal_len();
    assert_eq!(r.rebase(c, onto), Ok(rebased));
    assert_eq!(r.journal_len(), journal);
}

fn onto_the_unchanged_base_is_a_no_op<M: MetaStore>(f: &Fixture<M>) {
    let r = R::new(f, &[("a", b"1")]);
    let c = r.commit(r.root, &[("a", b"2")], &[]);
    let journal = r.journal_len();
    assert_eq!(r.rebase(c, r.root), Ok(c));
    // A stack on its own base is unchanged too.
    let d = r.commit(c, &[("b", b"1")], &[]);
    assert_eq!(r.rebase(d, r.root), Ok(d));
    assert_eq!(r.journal_len(), journal + 1);
}

fn identical_changes_on_both_sides_are_dropped<M: MetaStore>(f: &Fixture<M>) {
    let r = R::new(f, &[("a", b"1"), ("b", b"1"), ("gone", b"1")]);
    let c = r.commit(r.root, &[("a", b"2"), ("b", b"2")], &["gone"]);
    // The target already has `a = 2` and no `gone`.
    let onto = r.land(&[("a", b"2")], &["gone"]);
    let rebased = r.rebase(c, onto).expect("rebase");
    let cs = r.view(rebased).changeset;
    assert_eq!(cs.changes, vec![put("b", b"2")]);
    assert_eq!(r.snapshot(rebased), snap(&[("a", b"2"), ("b", b"2")]));

    // A draft whose every change is already there disappears: the result
    // is the target itself.
    let d = r.commit(r.root, &[("a", b"2")], &[]);
    assert_eq!(r.rebase(d, onto), Ok(onto));
}

fn a_conflict_creates_nothing<M: MetaStore>(f: &Fixture<M>) {
    let r = R::new(f, &[("a", b"1"), ("b", b"1")]);
    let c = r.commit(r.root, &[("a", b"ours"), ("b", b"2")], &[]);
    let onto = r.land(&[("a", b"theirs")], &[]);
    let journal = r.repo.journal();
    let objects = r.repo.f.objects.frames.lock().expect("frames").len();
    match r.rebase(c, onto) {
        Err(ApiError::Conflict { paths, onto: at }) => {
            assert_eq!(at, onto);
            assert_eq!(
                paths,
                vec![ConflictPath {
                    path: "a".to_owned(),
                    base: Some(blob_id(b"1")),
                    ours: Some(blob_id(b"ours")),
                    theirs: Some(blob_id(b"theirs")),
                }]
            );
        }
        other => panic!("expected a conflict, got {other:?}"),
    }
    assert_eq!(r.repo.journal(), journal);
    assert_eq!(
        r.repo.f.objects.frames.lock().expect("frames").len(),
        objects
    );
    // Line mode cannot merge one-line edits of the same line: still a
    // clean conflict, still nothing written.
    let line = block_on(r.repo.e().rebase(
        &r.repo.cx,
        Rebase {
            repo: "r".to_owned(),
            changeset: c,
            onto,
            merge: MergeMode::Line,
            dry_run: false,
        },
    ));
    assert_eq!(paths(line), ["a"]);
    assert_eq!(r.repo.journal(), journal);
}

fn deletes_and_renames<M: MetaStore>(f: &Fixture<M>) {
    let r = R::new(f, &[("a", b"1"), ("b", b"1"), ("c", b"1")]);
    // We delete `a`; they edit it.
    let c = r.commit(r.root, &[], &["a"]);
    let onto = r.land(&[("a", b"2")], &[]);
    assert_eq!(paths(r.rebase(c, onto)), ["a"]);
    // We edit `b`; they delete it.
    let c = r.commit(r.root, &[("b", b"2")], &[]);
    let onto2 = r.land(&[], &["b"]);
    assert_eq!(paths(r.rebase(c, onto2)), ["b"]);
    // We rename `c` to `d`; they leave `c` alone.
    let c = r.commit(r.root, &[("d", b"1")], &["c"]);
    let rebased = r.rebase(c, onto2).expect("rename");
    assert_eq!(r.snapshot(rebased), snap(&[("a", b"2"), ("d", b"1")]));
    // We rename `c` to `d`; they edit `c`.
    let onto3 = r.land(&[("c", b"2")], &[]);
    assert_eq!(paths(r.rebase(c, onto3)), ["c"]);
    // Both delete the same file: dropped.
    let both = r.commit(r.root, &[("x", b"1")], &["c"]);
    let onto4 = r.land(&[], &["c"]);
    let rebased = r.rebase(both, onto4).expect("both delete");
    assert_eq!(r.view(rebased).changeset.changes, vec![put("x", b"1")]);
}

fn modes_and_symlinks_are_changes<M: MetaStore>(f: &Fixture<M>) {
    let r = R::new(f, &[("run", b"#!"), ("link", b"target")]);
    let kind = |path: &str, kind, data: &[u8]| {
        r.repo.upload(&[data]);
        Change::Put {
            path: p(path),
            kind,
            id: blob_id(data),
        }
    };
    // We make `run` executable; they edit it.
    let c = r.commit_changes(r.root, vec![kind("run", EntryKind::Exec, b"#!")]);
    let onto = r.land(&[("run", b"#!sh")], &[]);
    assert_eq!(paths(r.rebase(c, onto)), ["run"]);
    // They made it executable too: dropped.
    let onto2 = r.land_changes(vec![kind("run", EntryKind::Exec, b"#!")]);
    let c2 = r.commit_changes(
        r.root,
        vec![
            kind("run", EntryKind::Exec, b"#!"),
            kind("other", EntryKind::File, b"o"),
        ],
    );
    let rebased = r.rebase(c2, onto2).expect("same mode");
    assert_eq!(r.view(rebased).changeset.changes.len(), 1);
    // Symlink targets that diverge conflict; a file turned into a symlink
    // conflicts with an edit of the file.
    let ours = r.commit_changes(r.root, vec![kind("link", EntryKind::Symlink, b"ours")]);
    let onto3 = r.land_changes(vec![kind("link", EntryKind::Symlink, b"theirs")]);
    assert_eq!(paths(r.rebase(ours, onto3)), ["link"]);
}

fn file_directory_clashes_conflict<M: MetaStore>(f: &Fixture<M>) {
    let r = R::new(f, &[("a/x", b"1"), ("f", b"1")]);
    // We add `f/y` (turning `f` into a directory); they edit `f`.
    let c = r.commit(r.root, &[("f/y", b"1")], &["f"]);
    let onto = r.land(&[("f", b"2")], &[]);
    assert_eq!(paths(r.rebase(c, onto)), ["f"]);
    // We add `a/z`; they turned `a` into a file. Our base and theirs agree
    // that `a/z` is absent, so applying is what finds the clash.
    let c = r.commit(r.root, &[("a/z", b"1")], &[]);
    let onto2 = r.land(&[("a", b"file")], &["a/x"]);
    let conflict = paths(r.rebase(c, onto2));
    assert_eq!(conflict.len(), 1);
    assert!(conflict[0] == "a" || conflict[0] == "a/z", "{conflict:?}");
    // We turn directory `d` into a file; they added `d/w` under it.
    let base = r.land(&[("d/x", b"1")], &[]);
    let c = r.commit(base, &[("d", b"file")], &["d/x"]);
    let onto3 = r.land(&[("d/w", b"1")], &[]);
    assert_eq!(paths(r.rebase(c, onto3)), ["d"]);
}

/// Regression (proptest, PR #31): both sides turn file `a` into the same
/// directory. A directory is not an entry, so `Delete a` is already there
/// on the target and the draft disappears.
fn identical_file_to_directory_changes_are_dropped<M: MetaStore>(f: &Fixture<M>) {
    let r = R::new(f, &[("a", b"1"), ("z", b"1")]);
    // A different change below the identical one, so the draft's id is not
    // the target's.
    let c = r.commit(r.root, &[("a/b/x", b"1"), ("y", b"1")], &["a"]);
    let onto = r.land(&[("a/b/x", b"1")], &["a"]);
    let rebased = r.rebase(c, onto).expect("rebase");
    assert_eq!(r.view(rebased).changeset.changes, vec![put("y", b"1")]);
    let d = r.commit(r.root, &[("a/b/x", b"1")], &["a"]);
    assert_eq!(r.rebase(d, onto), Ok(onto));
    // We delete file `a`; they turned it into a directory: already gone.
    let e = r.commit(r.root, &[], &["a"]);
    assert_eq!(r.rebase(e, onto), Ok(onto));
    // Both made `a` a directory, with different files in it: both kept.
    let g = r.commit(r.root, &[("a/c", b"1")], &["a"]);
    let rebased = r.rebase(g, onto).expect("rebase");
    assert_eq!(
        r.snapshot(rebased),
        snap(&[("a/b/x", b"1"), ("a/c", b"1"), ("z", b"1")])
    );
    // The same file inside, with different contents, still conflicts.
    let h = r.commit(r.root, &[("a/b/x", b"2")], &["a"]);
    assert_eq!(paths(r.rebase(h, onto)), ["a/b/x"]);
}

/// The minimal proptest input, as a stack: the bottom draft is the target
/// itself, and the draft above turns `a` back into a file.
fn a_stack_on_an_identical_bottom_is_kept<M: MetaStore>(f: &Fixture<M>) {
    let r = R::new(f, &[("a", b"1")]);
    let c1 = r.commit(r.root, &[("a/b/x", b"1")], &["a"]);
    let c2 = r.commit(c1, &[("a", b"1")], &["a/b/x"]);
    let onto = r.land(&[("a/b/x", b"1")], &["a"]);
    assert_eq!(onto, c1);
    assert_eq!(r.rebase(c2, onto), Ok(c2));
    // A target with the same content but another id: the bottom draft
    // disappears and the top one moves onto the target.
    let onto2 = r.land(&[("q", b"1")], &[]);
    let rebased = r.rebase(c2, onto2).expect("rebase");
    assert_eq!(r.snapshot(rebased), snap(&[("a", b"1"), ("q", b"1")]));
}

fn stacks_rebase_bottom_up<M: MetaStore>(f: &Fixture<M>) {
    let r = R::new(f, &[("a", b"1"), ("b", b"1")]);
    let c1 = r.commit(r.root, &[("a", b"2")], &[]);
    let c2 = r.commit(c1, &[("a", b"3"), ("s", b"1")], &[]);
    let c3 = r.commit(c2, &[("b", b"2")], &[]);
    let onto = r.land(&[("t", b"1")], &[]);
    let head = r.rebase(c3, onto).expect("stack");
    let v3 = r.view(head).changeset;
    let v2 = r.view(v3.parents[0]).changeset;
    let v1 = r.view(v2.parents[0]).changeset;
    assert_eq!(v1.parents, vec![onto]);
    for (v, c) in [(&v1, c1), (&v2, c2), (&v3, c3)] {
        assert_eq!(v.metadata[REBASED_FROM], c.to_string());
        assert_eq!(v.changes, r.view(c).changeset.changes);
    }
    assert_eq!(
        r.snapshot(head),
        snap(&[("a", b"3"), ("b", b"2"), ("s", b"1"), ("t", b"1")])
    );

    // A conflict in an upper draft fails the whole stack: nothing written,
    // not even the drafts below it that rebased cleanly.
    let onto2 = r.land(&[("b", b"theirs")], &[]);
    let journal = r.journal_len();
    assert_eq!(paths(r.rebase(c3, onto2)), ["b"]);
    assert_eq!(r.journal_len(), journal);
    let rebased_c1 = r.rebase(c1, onto2).expect("bottom alone");
    assert_eq!(r.journal_len(), journal + 1);
    assert_eq!(r.view(rebased_c1).changeset.parents, vec![onto2]);
}

fn rebase_checks_scopes_and_limits<M: MetaStore>(f: &Fixture<M>) {
    let r = R::new(f, &[("src/a", b"1"), ("doc/a", b"1")]);
    let c = r.commit(r.root, &[("src/a", b"2")], &[]);
    let onto = r.land(&[("doc/a", b"2")], &[]);
    let read = cx(&["repo:r:read"]);
    assert!(matches!(
        r.rebase_as(&read, c, onto),
        Err(ApiError::PermissionDenied { .. })
    ));
    let docs = cx(&["repo:r:write:doc"]);
    assert!(matches!(
        r.rebase_as(&docs, c, onto),
        Err(ApiError::PermissionDenied { path: Some(p), .. }) if p == "src/a"
    ));
    let src = cx(&["repo:r:write:src"]);
    assert!(r.rebase_as(&src, c, onto).is_ok());
    // Unknown target.
    assert!(matches!(
        r.rebase(c, ChangesetId(ObjectId([7; 32]))),
        Err(ApiError::NotFound { .. })
    ));
}

fn rebase_stacks_are_bounded() {
    let f = memory_with(EngineConfig {
        max_stack: 3,
        ..EngineConfig::default()
    });
    let r = R::new(&f, &[("a", b"0")]);
    let mut head = r.root;
    for b in [b"1" as &[u8], b"2", b"3"] {
        head = r.commit(head, &[("a", b)], &[]);
    }
    let onto = r.land(&[("z", b"1")], &[]);
    assert!(r.rebase(head, onto).is_ok());
    let deeper = r.commit(head, &[("a", b"4")], &[]);
    assert_eq!(
        r.rebase(deeper, onto),
        Err(ApiError::LimitExceeded {
            limit: "stack".to_owned(),
            max: 3,
        })
    );
}

#[test]
fn rebase_stack_limit() {
    rebase_stacks_are_bounded();
}

fn sync_moves_an_empty_workspace<M: MetaStore>(f: &Fixture<M>) {
    let r = R::new(f, &[("a", b"1"), ("b", b"1")]);
    r.open("w");
    let tip = r.land(&[("a", b"2")], &["b"]);
    let synced = r.sync("w").expect("sync");
    assert_eq!(synced.workspace.base, tip);
    assert_eq!(synced.workspace.head, None);
    assert_eq!(synced.changed_paths, vec![p("a"), p("b")]);
    // Again: nothing to do.
    let again = r.sync("w").expect("sync again");
    assert_eq!(again.workspace, synced.workspace);
    assert!(again.changed_paths.is_empty());
}

fn sync_rebases_the_stack<M: MetaStore>(f: &Fixture<M>) {
    let r = R::new(f, &[("a", b"1"), ("b", b"1")]);
    r.open("w");
    let d1 = r.commit_in("w", r.root, &[("a", b"2")]);
    let d2 = r.commit_in("w", d1, &[("c", b"1")]);
    let tip = r.land(&[("b", b"2"), ("t", b"1")], &[]);
    let synced = r.sync("w").expect("sync");
    let ws = synced.workspace;
    assert_eq!(ws.base, tip);
    let head = ws.head.expect("head");
    let n2 = r.view(head);
    let n1 = r.view(n2.changeset.parents[0]);
    assert_eq!(n1.changeset.parents, vec![tip]);
    // The new drafts are held; the old ones point at their successors.
    assert_eq!(n2.state, DraftState::Draft);
    assert_eq!(n1.state, DraftState::Draft);
    let old1 = r.view(d1);
    let old2 = r.view(d2);
    assert_eq!(old1.state, DraftState::Superseded);
    assert_eq!(old1.superseded_by, Some(n2.changeset.parents[0]));
    assert_eq!(old2.state, DraftState::Superseded);
    assert_eq!(old2.superseded_by, Some(head));
    // The client updates the paths that differ between its old head and
    // the new one: their changes, not ours.
    assert_eq!(synced.changed_paths, vec![p("b"), p("t")]);
    assert_eq!(
        r.snapshot(head),
        snap(&[("a", b"2"), ("b", b"2"), ("c", b"1"), ("t", b"1")])
    );
    let last = r.repo.journal().pop().expect("entry");
    assert_eq!(last.kind, "WorkspaceSynced");
    assert_eq!(
        last.body,
        format!(
            "{{\"name\":\"w\",\"old_base\":\"{}\",\"new_base\":\"{tip}\",\"old_head\":\"{d2}\",\"new_head\":\"{head}\"}}",
            r.root
        )
    );
    // Committing continues on the new head.
    let d3 = r.commit_in("w", head, &[("d", b"1")]);
    assert_eq!(r.get("w").head, Some(d3));
}

fn sync_conflict_changes_nothing<M: MetaStore>(f: &Fixture<M>) {
    let r = R::new(f, &[("a", b"1")]);
    r.open("w");
    let d1 = r.commit_in("w", r.root, &[("a", b"ours")]);
    let before = r.get("w");
    let tip = r.land(&[("a", b"theirs")], &[]);
    let journal = r.repo.journal();
    match r.sync("w") {
        Err(ApiError::Conflict { paths, onto }) => {
            assert_eq!(onto, tip);
            assert_eq!(paths.len(), 1);
            assert_eq!(paths[0].path, "a");
        }
        other => panic!("expected a conflict, got {other:?}"),
    }
    assert_eq!(r.get("w"), before);
    assert_eq!(r.view(d1).state, DraftState::Draft);
    assert_eq!(r.repo.journal(), journal);
}

fn sync_drops_drafts_already_landed<M: MetaStore>(f: &Fixture<M>) {
    let r = R::new(f, &[("a", b"1")]);
    r.open("w");
    let d1 = r.commit_in("w", r.root, &[("a", b"2")]);
    // The same change landed by someone else, with another.
    let tip = r.land(&[("a", b"2"), ("b", b"1")], &[]);
    let synced = r.sync("w").expect("sync");
    assert_eq!(synced.workspace.base, tip);
    assert_eq!(synced.workspace.head, None);
    assert_eq!(synced.changed_paths, vec![p("b")]);
    let old = r.view(d1);
    assert_eq!(old.state, DraftState::Superseded);
    assert_eq!(old.superseded_by, Some(tip));
}

fn sync_onto_a_named_line_position<M: MetaStore>(f: &Fixture<M>) {
    let r = R::new(f, &[("a", b"1")]);
    r.open("w");
    let mid = r.land(&[("b", b"1")], &[]);
    let _tip = r.land(&[("c", b"1")], &[]);
    let synced = r.sync_as(&r.repo.cx, "w", Some(mid)).expect("sync");
    assert_eq!(synced.workspace.base, mid);
    assert_eq!(synced.changed_paths, vec![p("b")]);
    // Not on the line.
    let off = r.commit(mid, &[("x", b"1")], &[]);
    assert_eq!(
        r.sync_as(&r.repo.cx, "w", Some(off)),
        Err(ApiError::InvalidChange {
            path: String::new(),
            rule: "onto-not-on-line".to_owned(),
        })
    );
}

fn sync_is_the_owners<M: MetaStore>(f: &Fixture<M>) {
    let r = R::new(f, &[("a", b"1")]);
    r.open("w");
    r.land(&[("b", b"1")], &[]);
    let mut bob = cx(&["repo:r:write"]);
    bob.principal.subject = "bob".to_owned();
    assert!(matches!(
        r.sync_as(&bob, "w", None),
        Err(ApiError::PermissionDenied { .. })
    ));
    let reader = cx(&["repo:r:read"]);
    assert!(matches!(
        r.sync_as(&reader, "w", None),
        Err(ApiError::PermissionDenied { .. })
    ));
    let mut admin = admin("r");
    admin.principal.subject = "root".to_owned();
    assert!(r.sync_as(&admin, "w", None).is_ok());
    // A lapsed lease ends the workspace.
    r.open("v");
    f.clock
        .advance(EngineConfig::default().default_lease_ms as i64 + 1);
    r.land(&[("c", b"1")], &[]);
    assert!(matches!(r.sync("v"), Err(ApiError::LeaseExpired { .. })));
}

fn a_dry_run_writes_nothing<M: MetaStore>(f: &Fixture<M>) {
    let r = R::new(f, &[("a", b"1\n2\n3\n"), ("b", b"1")]);
    let c = r.commit(r.root, &[("a", b"one\n2\n3\n"), ("b", b"2")], &[]);
    let onto = r.land(&[("a", b"1\n2\nthree\n")], &[]);
    let journal = r.repo.journal();
    let frames = r.frames();

    // Line merge makes a new blob and a new changeset; the dry run stores
    // neither, and no record or journal entry.
    let pins_c = r.protection(c);
    let pins_onto = r.protection(onto);
    let record_c = r.view(c);
    let dry = r.rebase_line(c, onto, true).expect("dry run");
    assert_ne!(dry, c);
    assert_eq!(r.repo.journal(), journal);
    assert_eq!(r.frames(), frames);
    let got = block_on(r.repo.e().get_changesets(
        &r.repo.cx,
        GetChangesets {
            repo: "r".to_owned(),
            ids: vec![dry],
        },
    ))
    .expect("get changesets");
    assert_eq!(got.missing, vec![dry]);

    // Pins and records are as they were.
    assert_eq!(r.protection(c), pins_c);
    assert_eq!(r.protection(onto), pins_onto);
    assert_eq!(r.view(c), record_c);

    // The real rebase makes the id the dry run returned.
    let real = r.rebase_line(c, onto, false).expect("rebase");
    assert_eq!(real, dry);
    assert!(r.frames() > frames);
    assert_eq!(
        r.snapshot(real),
        snap(&[("a", b"one\n2\nthree\n"), ("b", b"2")])
    );
}

fn a_dry_run_checks_path_scopes<M: MetaStore>(f: &Fixture<M>) {
    let r = R::new(f, &[("a", b"1"), ("doc/b", b"1")]);
    let c = r.commit(r.root, &[("a", b"ours"), ("doc/b", b"2")], &[]);
    let onto = r.land(&[("a", b"theirs")], &[]);
    let journal = r.repo.journal();
    let frames = r.frames();
    // As a real rebase: a token that may not write `a` learns nothing of
    // it, conflict or not.
    for scopes in [&["repo:r:write:doc"][..], &["repo:r:read"][..]] {
        let caller = cx(scopes);
        for dry_run in [true, false] {
            let got = block_on(r.repo.e().rebase(
                &caller,
                Rebase {
                    repo: "r".to_owned(),
                    changeset: c,
                    onto,
                    merge: MergeMode::Line,
                    dry_run,
                },
            ));
            assert!(
                matches!(got, Err(ApiError::PermissionDenied { .. })),
                "{scopes:?} dry_run={dry_run}: {got:?}"
            );
        }
    }
    assert_eq!(r.repo.journal(), journal);
    assert_eq!(r.frames(), frames);
}

fn a_dry_run_reports_the_same_conflicts<M: MetaStore>(f: &Fixture<M>) {
    let r = R::new(f, &[("a", b"1"), ("b", b"1\n2\n"), ("c", b"1")]);
    let c = r.commit(
        r.root,
        &[("a", b"ours"), ("b", b"ours\n2\n"), ("c", b"2")],
        &[],
    );
    let onto = r.land(&[("a", b"theirs"), ("b", b"theirs\n2\n")], &[]);
    let journal = r.repo.journal();
    let frames = r.frames();
    for mode in [MergeMode::Line, MergeMode::File] {
        let run = |dry_run| {
            block_on(r.repo.e().rebase(
                &r.repo.cx,
                Rebase {
                    repo: "r".to_owned(),
                    changeset: c,
                    onto,
                    merge: mode,
                    dry_run,
                },
            ))
        };
        let dry = run(true);
        assert!(matches!(dry, Err(ApiError::Conflict { .. })), "{dry:?}");
        assert_eq!(paths(dry.clone()), ["a", "b"]);
        assert_eq!(dry, run(false));
    }
    assert_eq!(r.repo.journal(), journal);
    assert_eq!(r.frames(), frames);
}

on_both_stores!(
    a_dry_run_writes_nothing,
    a_dry_run_checks_path_scopes,
    a_dry_run_reports_the_same_conflicts,
    disjoint_changes_apply_on_the_target,
    onto_the_unchanged_base_is_a_no_op,
    identical_changes_on_both_sides_are_dropped,
    a_conflict_creates_nothing,
    deletes_and_renames,
    modes_and_symlinks_are_changes,
    file_directory_clashes_conflict,
    identical_file_to_directory_changes_are_dropped,
    a_stack_on_an_identical_bottom_is_kept,
    stacks_rebase_bottom_up,
    rebase_checks_scopes_and_limits,
    sync_moves_an_empty_workspace,
    sync_rebases_the_stack,
    sync_conflict_changes_nothing,
    sync_drops_drafts_already_landed,
    sync_onto_a_named_line_position,
    sync_is_the_owners,
);

/// Wraps a store to count reads, to fail the next sync commit (before or
/// after it applies), and to slip a concurrent transaction in just before
/// the next sync commit.
struct Hooked<M> {
    inner: M,
    reads: AtomicU64,
    /// 0: none; 1: fail before; 2: fail after.
    crash: AtomicU8,
    inject: Mutex<Option<MetaTxn>>,
}

impl<M> Hooked<M> {
    fn new(inner: M) -> Self {
        Self {
            inner,
            reads: AtomicU64::new(0),
            crash: AtomicU8::new(0),
            inject: Mutex::new(None),
        }
    }

    fn read(&self) {
        self.reads.fetch_add(1, Ordering::SeqCst);
    }
}

fn is_sync(txn: &MetaTxn) -> bool {
    txn.writes
        .iter()
        .any(|w| matches!(w, Write::Journal { kind, .. } if kind == "WorkspaceSynced"))
}

impl<M: MetaStore> MetaStore for Hooked<M> {
    async fn create_repo(&self, name: &str) -> StoreResult<RepoId> {
        self.inner.create_repo(name).await
    }
    async fn repo(&self, name: &str) -> StoreResult<Option<RepoId>> {
        self.read();
        self.inner.repo(name).await
    }
    async fn commit(&self, repo: RepoId, txn: MetaTxn) -> StoreResult<TxnOutcome> {
        if !is_sync(&txn) {
            return self.inner.commit(repo, txn).await;
        }
        let injected = self.inject.lock().expect("inject").take();
        if let Some(other) = injected {
            let outcome = self.inner.commit(repo, other).await?;
            assert!(matches!(outcome, TxnOutcome::Committed(_)), "{outcome:?}");
        }
        match self.crash.swap(0, Ordering::SeqCst) {
            1 => Err(StoreError::Backend("killed before commit".to_owned())),
            2 => {
                self.inner.commit(repo, txn).await?;
                Err(StoreError::Backend("killed after commit".to_owned()))
            }
            _ => self.inner.commit(repo, txn).await,
        }
    }
    async fn changeset(
        &self,
        repo: RepoId,
        id: ChangesetId,
    ) -> StoreResult<Option<(Vec<u8>, ChangesetRecord)>> {
        self.read();
        self.inner.changeset(repo, id).await
    }
    async fn get_ref(&self, repo: RepoId, name: &str) -> StoreResult<Option<RefRecord>> {
        self.read();
        self.inner.get_ref(repo, name).await
    }
    async fn list_refs(
        &self,
        repo: RepoId,
        prefix: &str,
        limit: u32,
    ) -> StoreResult<Vec<RefRecord>> {
        self.read();
        self.inner.list_refs(repo, prefix, limit).await
    }
    async fn workspace(&self, repo: RepoId, name: &str) -> StoreResult<Option<WorkspaceRecord>> {
        self.read();
        self.inner.workspace(repo, name).await
    }
    async fn expired_workspaces(
        &self,
        repo: RepoId,
        before_ms: i64,
        limit: u32,
    ) -> StoreResult<Vec<WorkspaceRecord>> {
        self.read();
        self.inner.expired_workspaces(repo, before_ms, limit).await
    }
    async fn list_workspaces(
        &self,
        repo: RepoId,
        owner: &str,
        after: &str,
        limit: u32,
    ) -> StoreResult<Vec<WorkspaceRecord>> {
        self.read();
        self.inner.list_workspaces(repo, owner, after, limit).await
    }
    async fn count_records(&self, repo: RepoId, state: RecordState) -> StoreResult<u64> {
        self.read();
        self.inner.count_records(repo, state).await
    }
    async fn count_workspaces(&self, repo: RepoId, before_ms: Option<i64>) -> StoreResult<u64> {
        self.read();
        self.inner.count_workspaces(repo, before_ms).await
    }
    async fn derived(
        &self,
        repo: RepoId,
        kind: &str,
        version: u32,
        key: &[u8],
    ) -> StoreResult<Option<Vec<u8>>> {
        self.read();
        self.inner.derived(repo, kind, version, key).await
    }
    async fn line_pos(
        &self,
        repo: RepoId,
        line: &str,
        id: ChangesetId,
    ) -> StoreResult<Option<u64>> {
        self.read();
        self.inner.line_pos(repo, line, id).await
    }
    async fn line_holders(
        &self,
        repo: RepoId,
        id: ChangesetId,
        limit: u32,
    ) -> StoreResult<Vec<(String, u64)>> {
        self.read();
        self.inner.line_holders(repo, id, limit).await
    }
    async fn scan_line_path(
        &self,
        repo: RepoId,
        line: &str,
        path: &RepoPath,
        before: u64,
        limit: u32,
    ) -> StoreResult<Vec<(u64, ChangesetId)>> {
        self.read();
        self.inner
            .scan_line_path(repo, line, path, before, limit)
            .await
    }
    async fn holders(&self, repo: RepoId, object: ObjectId) -> StoreResult<u32> {
        self.read();
        self.inner.holders(repo, object).await
    }
    async fn land(
        &self,
        repo: RepoId,
        draft: ChangesetId,
        line: &str,
    ) -> StoreResult<Option<ChangesetId>> {
        self.read();
        self.inner.land(repo, draft, line).await
    }
    async fn journal(
        &self,
        repo: RepoId,
        from: Lsn,
        limit: u32,
    ) -> StoreResult<Vec<(Lsn, String, Vec<u8>)>> {
        self.read();
        self.inner.journal(repo, from, limit).await
    }
    async fn protection(&self, repo: RepoId, id: ChangesetId) -> StoreResult<Option<Protection>> {
        self.read();
        self.inner.protection(repo, id).await
    }
    async fn expired_records(
        &self,
        repo: RepoId,
        now_ms: i64,
        after: Option<(i64, ChangesetId)>,
        limit: u32,
    ) -> StoreResult<Vec<(i64, ChangesetId)>> {
        self.read();
        self.inner.expired_records(repo, now_ms, after, limit).await
    }
    async fn held_objects(
        &self,
        repo: RepoId,
        holder: &str,
        after: Option<ObjectId>,
        limit: u32,
    ) -> StoreResult<Vec<ObjectId>> {
        self.read();
        self.inner.held_objects(repo, holder, after, limit).await
    }
    async fn expired_rows(
        &self,
        repo: RepoId,
        now_ms: i64,
        after: Option<TimedRow>,
        limit: u32,
    ) -> StoreResult<Vec<TimedRow>> {
        self.read();
        self.inner.expired_rows(repo, now_ms, after, limit).await
    }
    async fn doom_marks(
        &self,
        repo: RepoId,
        ids: &[ObjectId],
    ) -> StoreResult<Vec<(ObjectId, DoomMark)>> {
        self.read();
        self.inner.doom_marks(repo, ids).await
    }
    async fn old_doomed(
        &self,
        repo: RepoId,
        until_ms: i64,
        after: Option<(i64, ObjectId)>,
        limit: u32,
    ) -> StoreResult<Vec<(ObjectId, DoomMark)>> {
        self.read();
        self.inner.old_doomed(repo, until_ms, after, limit).await
    }
    async fn lapsed_workspaces(
        &self,
        repo: RepoId,
        before_ms: i64,
        after: Option<(i64, String)>,
        limit: u32,
    ) -> StoreResult<Vec<WorkspaceRecord>> {
        self.read();
        self.inner
            .lapsed_workspaces(repo, before_ms, after, limit)
            .await
    }
    async fn gc_protection(&self, repo: RepoId) -> StoreResult<bool> {
        self.read();
        self.inner.gc_protection(repo).await
    }
}

fn hooked() -> (Arc<Hooked<MemoryStore>>, Fixture<Hooked<MemoryStore>>) {
    let meta = Arc::new(Hooked::new(MemoryStore::new()));
    let f = build(meta.clone(), EngineConfig::default(), None);
    (meta, f)
}

/// Lands `n` changesets on `main`, each touching `docs/a`, and returns the
/// line, oldest first.
fn history<M: MetaStore>(r: &R<'_, M>, n: usize) -> Vec<ChangesetId> {
    let mut line = vec![r.root];
    for i in 0..n {
        let data = format!("{i}");
        line.push(r.land(&[("docs/a", data.as_bytes())], &[]));
    }
    line
}

/// The target is the same long line every time; only the distance from the
/// draft's base up to it changes. (Eager skip-list rows for the new
/// changeset cost O(log depth) of the target, whatever the distance.)
const DEPTH: usize = 300;

#[test]
fn rebase_cost_does_not_depend_on_the_distance_to_onto() {
    // Reads (metadata and objects) for one rebase onto the tip, from a
    // draft whose base is `distance` landed changesets behind it.
    let cost = |distance: usize| {
        let (meta, f) = hooked();
        let r = R::new(&f, &[("src/lib.rs", b"1"), ("docs/a", b"1")]);
        let line = history(&r, DEPTH);
        let base = line[DEPTH - distance];
        let c = r.commit(base, &[("src/lib.rs", b"2"), ("src/new.rs", b"n")], &[]);
        let onto = r.tip();
        let meta_before = meta.reads.load(Ordering::SeqCst);
        let gets_before = f.objects.gets.load(Ordering::SeqCst);
        let rebased = r.rebase(c, onto).expect("rebase");
        let cost = (
            meta.reads.load(Ordering::SeqCst) - meta_before,
            f.objects.gets.load(Ordering::SeqCst) - gets_before,
        );
        assert_eq!(
            r.snapshot(rebased)["src/new.rs"],
            b"n".to_vec(),
            "distance {distance}"
        );
        cost
    };
    let near = cost(1);
    assert_eq!(cost(16), near);
    assert_eq!(cost(DEPTH), near);
}

#[test]
fn sync_cost_does_not_depend_on_the_distance_to_the_tip() {
    let cost = |distance: usize| {
        let (meta, f) = hooked();
        let r = R::new(&f, &[("src/lib.rs", b"1"), ("docs/a", b"1")]);
        // Every landed changeset touches the same file, so the changed set
        // stays one path whatever the distance.
        let line = history(&r, DEPTH);
        let base = line[DEPTH - distance];
        block_on(r.repo.e().open_workspace(
            &r.repo.cx,
            OpenWorkspace {
                repo: "r".to_owned(),
                name: Some("w".to_owned()),
                line: "main".to_owned(),
                at: Some(base),
                lease_ms: 0,
            },
        ))
        .expect("open");
        let d1 = r.commit_in("w", base, &[("src/lib.rs", b"2")]);
        r.commit_in("w", d1, &[("src/new.rs", b"n")]);
        let before = (
            meta.reads.load(Ordering::SeqCst),
            f.objects.gets.load(Ordering::SeqCst),
        );
        let synced = r.sync("w").expect("sync");
        assert_eq!(synced.changed_paths, vec![p("docs/a")]);
        (
            meta.reads.load(Ordering::SeqCst) - before.0,
            f.objects.gets.load(Ordering::SeqCst) - before.1,
        )
    };
    let near = cost(1);
    assert_eq!(cost(16), near);
    assert_eq!(cost(DEPTH), near);
}

#[test]
fn a_sync_racing_a_set_head_retries_on_the_new_head() {
    let (meta, f) = hooked();
    let r = R::new(&f, &[("a", b"1")]);
    r.open("w");
    let d1 = r.commit_in("w", r.root, &[("a", b"2")]);
    // A second draft made outside, which a concurrent `SetHead` adopts
    // after the sync read the workspace and before it commits.
    let d2 = r.commit(d1, &[("b", b"1")], &[]);
    let tip = r.land(&[("t", b"1")], &[]);
    let repo = block_on(meta.inner.repo("r"))
        .expect("repo")
        .expect("exists");
    let ws = block_on(meta.inner.workspace(repo, "w"))
        .expect("read")
        .expect("ws");
    let (_, rec) = block_on(meta.inner.changeset(repo, d2))
        .expect("read")
        .expect("stored");
    let mut moved = ws.clone();
    moved.head = Some(d2);
    let mut held = rec;
    held.state = RecordState::Draft;
    held.holder = Some("ws:w".to_owned());
    *meta.inject.lock().expect("inject") = Some(MetaTxn {
        preconditions: Vec::new(),
        writes: vec![
            Write::SetRecord {
                id: d2,
                record: held,
            },
            Write::PutWorkspace(moved),
        ],
    });

    let synced = r.sync("w").expect("sync");
    let head = synced.workspace.head.expect("head");
    // Both drafts were rebased: the retry saw the new head.
    assert_eq!(synced.workspace.base, tip);
    assert_eq!(
        r.snapshot(head),
        snap(&[("a", b"2"), ("b", b"1"), ("t", b"1")])
    );
    assert_eq!(r.view(d1).state, DraftState::Superseded);
    assert_eq!(r.view(d2).state, DraftState::Superseded);
    assert_eq!(r.view(d2).superseded_by, Some(head));
}

#[test]
fn a_crash_mid_sync_leaves_no_partial_state() {
    let (meta, f) = hooked();
    let r = R::new(&f, &[("a", b"1")]);
    r.open("w");
    let d1 = r.commit_in("w", r.root, &[("a", b"2")]);
    let before = r.get("w");
    r.land(&[("t", b"1")], &[]);

    // Killed before the commit point: the workspace and its draft are as
    // they were. The rebased changeset is stored, but nothing holds it.
    meta.crash.store(1, Ordering::SeqCst);
    assert!(matches!(r.sync("w"), Err(ApiError::Unavailable { .. })));
    assert_eq!(r.get("w"), before);
    assert_eq!(r.view(d1).state, DraftState::Draft);
    let gauges = block_on(r.repo.e().gauges("r")).expect("gauges");
    assert_eq!((gauges.held, gauges.abandoned), (1, 1));

    // The retry adopts the same changeset.
    let synced = r.sync("w").expect("retry");
    let head = synced.workspace.head.expect("head");
    assert_eq!(r.view(head).state, DraftState::Draft);
    let gauges = block_on(r.repo.e().gauges("r")).expect("gauges");
    assert_eq!(
        (gauges.held, gauges.abandoned, gauges.superseded),
        (1, 0, 1)
    );

    // Killed after the commit point: everything moved together, and the
    // retry finds the workspace already on the tip.
    let tip = r.land(&[("u", b"1")], &[]);
    meta.crash.store(2, Ordering::SeqCst);
    assert!(r.sync("w").is_err());
    let after = r.get("w");
    assert_eq!(after.base, tip);
    let new_head = after.head.expect("head");
    assert_eq!(r.view(new_head).state, DraftState::Draft);
    assert_eq!(r.view(head).superseded_by, Some(new_head));
    let retry = r.sync("w").expect("retry");
    assert_eq!(retry.workspace, after);
}
