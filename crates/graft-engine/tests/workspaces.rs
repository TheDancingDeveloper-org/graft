//! G2.1: the workspace API, draft states and holders, on both stores.

mod common;

use std::sync::Arc;
use std::sync::atomic::{AtomicU8, Ordering};

use common::*;
use graft_engine::{Engine, EngineConfig, Gauges};
use graft_model::{ChangesetId, ObjectId, RepoPath};
use graft_proto::error::ApiResult;
use graft_proto::types::*;
use graft_proto::{ApiError, CallContext, RepoApi, WorkspaceApi};
use graft_store::memory::MemoryStore;
use graft_store::{
    ChangesetRecord, DoomMark, Lsn, MetaStore, MetaTxn, Protection, RecordState, RefRecord, RepoId,
    StoreError, StoreResult, TimedRow, TxnOutcome, WorkspaceRecord,
};

const HOUR: u64 = 3600 * 1000;

/// A repository with `main` at a landed root, and the caller's workspace
/// shortcuts.
struct Ws<'f, M> {
    repo: Repo<'f, M>,
    root: ChangesetId,
}

impl<'f, M: MetaStore> Ws<'f, M> {
    fn new(f: &'f Fixture<M>, name: &str) -> Self {
        let repo = Repo::new(f, name);
        let root = repo.commit(vec![], &[("README", b"hello")], &[]).id;
        repo.update_ref("main", None, root, RefKind::Line)
            .expect("main");
        repo.upload(&[b"1", b"2", b"3", b"4", b"5"]);
        Self { repo, root }
    }

    fn e(&self) -> &Engine<M, Faulty> {
        self.repo.e()
    }

    fn wref(&self, name: &str) -> WorkspaceRef {
        WorkspaceRef {
            repo: self.repo.name.clone(),
            name: name.to_owned(),
        }
    }

    fn open_as(&self, cx: &CallContext, name: &str, lease_ms: u64) -> ApiResult<WorkspaceView> {
        block_on(self.e().open_workspace(
            cx,
            OpenWorkspace {
                repo: self.repo.name.clone(),
                name: Some(name.to_owned()),
                line: "main".to_owned(),
                at: None,
                lease_ms,
            },
        ))
    }

    fn open(&self, name: &str) -> WorkspaceView {
        self.open_as(&self.repo.cx, name, 0).expect("open")
    }

    fn get(&self, name: &str) -> ApiResult<WorkspaceView> {
        block_on(self.e().get_workspace(&self.repo.cx, self.wref(name)))
    }

    fn commit_in_as(
        &self,
        cx: &CallContext,
        ws: &str,
        parent: ChangesetId,
        content: &[u8],
    ) -> ApiResult<Created> {
        let mut req = commit_req(&self.repo.name, vec![parent], vec![put("f", content)]);
        req.workspace = Some(ws.to_owned());
        block_on(self.e().create_changeset(cx, req))
    }

    fn commit_in(&self, ws: &str, parent: ChangesetId, content: &[u8]) -> ApiResult<Created> {
        self.commit_in_as(&self.repo.cx, ws, parent, content)
    }

    fn set_head(
        &self,
        ws: &str,
        expected: Option<ChangesetId>,
        new: Option<ChangesetId>,
    ) -> ApiResult<WorkspaceView> {
        block_on(self.e().set_head(
            &self.repo.cx,
            SetHead {
                workspace: self.wref(ws),
                expected,
                new,
            },
        ))
    }

    fn renew(&self, ws: &str, lease_ms: u64) -> ApiResult<WorkspaceView> {
        block_on(self.e().renew_lease(&self.repo.cx, self.wref(ws), lease_ms))
    }

    fn close(&self, ws: &str) -> ApiResult<()> {
        block_on(
            self.e()
                .close_workspace(&self.repo.cx, self.wref(ws), false),
        )
    }

    fn state(&self, id: ChangesetId) -> DraftState {
        self.repo.changeset(id).state
    }

    fn record(&self, id: ChangesetId) -> ChangesetRecord {
        let (meta, _) = self.e().stores();
        let repo = block_on(meta.repo(&self.repo.name))
            .expect("repo")
            .expect("exists");
        block_on(meta.changeset(repo, id))
            .expect("read")
            .expect("stored")
            .1
    }

    fn gauges(&self) -> Gauges {
        block_on(self.e().gauges(&self.repo.name)).expect("gauges")
    }
}

fn as_subject(subject: &str, scopes: &[&str]) -> CallContext {
    let mut cx = cx(scopes);
    cx.principal.subject = subject.to_owned();
    cx
}

fn open_get_list_renew_close<M: MetaStore>(f: &Fixture<M>) {
    let w = Ws::new(f, "r");
    let opened = w.open("w1");
    assert_eq!(opened.base, w.root);
    assert_eq!(opened.head, None);
    assert_eq!(opened.owner, "tester");
    assert_eq!(opened.lease_until_ms, 1_000_000 + 2 * HOUR as i64);
    assert_eq!(w.get("w1").expect("get"), opened);
    // A retried open returns the same workspace.
    assert_eq!(w.open("w1"), opened);

    for name in ["w3", "w2"] {
        w.open(name);
    }
    let bob = as_subject("bob", &["repo:r:write"]);
    w.open_as(&bob, "bob-1", 0).expect("bob opens");
    let list = |token: Option<&str>, limit| {
        block_on(w.e().list_workspaces(
            &w.repo.cx,
            "r".to_owned(),
            Page {
                limit,
                token: token.map(str::to_owned),
            },
        ))
        .expect("list")
        .into_iter()
        .map(|v| v.name)
        .collect::<Vec<_>>()
    };
    assert_eq!(list(None, 0), ["w1", "w2", "w3"]);
    assert_eq!(list(None, 2), ["w1", "w2"]);
    assert_eq!(list(Some("w2"), 2), ["w3"]);

    let renewed = w.renew("w1", HOUR).expect("renew");
    assert_eq!(renewed.lease_until_ms, 1_000_000 + HOUR as i64);
    // A lease is capped.
    let capped = w.renew("w1", u64::MAX).expect("renew");
    assert_eq!(
        capped.lease_until_ms,
        1_000_000 + EngineConfig::default().max_lease_ms as i64
    );

    w.close("w1").expect("close");
    assert!(matches!(w.get("w1"), Err(ApiError::NotFound { .. })));
    w.close("w1").expect("closing again is a retry");

    let kinds: Vec<String> = w.repo.journal().into_iter().map(|e| e.kind).collect();
    assert!(kinds.contains(&"WorkspaceOpened".to_owned()));
    assert!(kinds.contains(&"WorkspaceRenewed".to_owned()));
    assert_eq!(kinds.last().map(String::as_str), Some("WorkspaceClosed"));
}

fn open_checks_line_name_and_scope<M: MetaStore>(f: &Fixture<M>) {
    let w = Ws::new(f, "r");
    // Names that differ only by case or normalisation are refused, not
    // folded.
    for bad in ["W1", "caf\u{e9}", "cafe\u{301}", "", "a//b"] {
        assert!(
            matches!(
                w.open_as(&w.repo.cx, bad, 0),
                Err(ApiError::InvalidPath { .. })
            ),
            "{bad:?}"
        );
    }
    let mut req = OpenWorkspace {
        repo: "r".to_owned(),
        name: None,
        line: "nope".to_owned(),
        at: None,
        lease_ms: 0,
    };
    assert!(matches!(
        block_on(w.e().open_workspace(&w.repo.cx, req.clone())),
        Err(ApiError::NotFound { .. })
    ));
    w.repo
        .update_ref("tag", None, w.root, RefKind::Tag)
        .expect("tag");
    req.line = "tag".to_owned();
    assert!(matches!(
        block_on(w.e().open_workspace(&w.repo.cx, req.clone())),
        Err(ApiError::InvalidChange { .. })
    ));
    // A generated name is stable for a retried request.
    req.line = "main".to_owned();
    let a = block_on(w.e().open_workspace(&w.repo.cx, req.clone())).expect("open");
    let b = block_on(w.e().open_workspace(&w.repo.cx, req.clone())).expect("open");
    assert_eq!(a, b);
    assert!(a.name.starts_with("w-"));

    let reader = cx(&["repo:r:read"]);
    assert!(matches!(
        w.open_as(&reader, "w9", 0),
        Err(ApiError::PermissionDenied { .. })
    ));
    // A path-scoped writer may hold a workspace.
    let scoped = as_subject("scoped", &["repo:r:write:src"]);
    w.open_as(&scoped, "scoped", 0).expect("open");
}

fn commits_in_a_workspace_are_held_drafts<M: MetaStore>(f: &Fixture<M>) {
    let w = Ws::new(f, "r");
    w.open("w");
    let c1 = w.commit_in("w", w.root, b"1").expect("commit");
    assert_eq!(c1.state, DraftState::Draft);
    // Object holder rows: the upload lease, then the draft that claims it,
    // and the trees the draft derived.
    let (meta, _) = w.e().stores();
    let rid = block_on(meta.repo("r")).expect("repo").expect("exists");
    let holders = |o| block_on(meta.holders(rid, o)).expect("holders");
    assert_eq!(holders(blob_id(b"2")), 1, "the upload lease");
    let c2 = w.commit_in("w", c1.id, b"2").expect("commit");
    assert_eq!(holders(blob_id(b"2")), 2, "the lease and the draft");
    assert_eq!(holders(c2.root.0), 1, "the derived root");
    assert_eq!(w.get("w").expect("get").head, Some(c2.id));
    let rec = w.record(c2.id);
    assert_eq!(rec.state, RecordState::Draft);
    assert_eq!(rec.holder.as_deref(), Some("ws:w"));
    let last = w.repo.journal().pop().expect("entry");
    assert_eq!(
        last.body,
        format!(
            "{{\"id\":\"{}\",\"state\":\"draft\",\"workspace\":\"w\"}}",
            c2.id
        )
    );

    // A retry returns the same draft and changes nothing.
    let journal = w.repo.journal().len();
    assert_eq!(w.commit_in("w", c1.id, b"2").expect("retry").id, c2.id);
    assert_eq!(w.repo.journal().len(), journal);
    assert_eq!(w.get("w").expect("get").head, Some(c2.id));

    // A stale parent is a conflict and writes nothing.
    assert_eq!(
        w.commit_in("w", c1.id, b"3"),
        Err(ApiError::RefConflict {
            name: "w".to_owned(),
            actual: Some(c2.id),
        })
    );
    assert_eq!(w.repo.journal().len(), journal);
    // A workspace commit needs a parent.
    let mut req = commit_req("r", vec![], vec![put("f", b"1")]);
    req.workspace = Some("w".to_owned());
    assert!(matches!(
        block_on(w.e().create_changeset(&w.repo.cx, req)),
        Err(ApiError::InvalidChange { .. })
    ));

    assert_eq!(
        w.gauges(),
        Gauges {
            held: 2,
            abandoned: 0,
            superseded: 0,
            active: 1,
            lapsed: 0,
        }
    );
}

fn stale_set_head_fails_and_changes_nothing<M: MetaStore>(f: &Fixture<M>) {
    let w = Ws::new(f, "r");
    w.open("w");
    let c1 = w.commit_in("w", w.root, b"1").expect("commit").id;
    let before = (w.get("w").expect("get"), w.record(c1), w.repo.journal());
    for expected in [None, Some(w.root)] {
        assert_eq!(
            w.set_head("w", expected, None),
            Err(ApiError::RefConflict {
                name: "w".to_owned(),
                actual: Some(c1),
            })
        );
    }
    assert_eq!(
        (w.get("w").expect("get"), w.record(c1), w.repo.journal()),
        before
    );
    // Setting the head it already has is a no-op.
    assert_eq!(w.set_head("w", Some(c1), Some(c1)).expect("same"), before.0);
}

fn amend_supersedes_and_drop_abandons<M: MetaStore>(f: &Fixture<M>) {
    let w = Ws::new(f, "r");
    w.open("w");
    let c1 = w.commit_in("w", w.root, b"1").expect("commit").id;
    // An amend: the replacement is made outside the workspace (nothing
    // holds it), then the head moves to it.
    let amended = w.repo.commit(vec![w.root], &[("f", b"2")], &[]).id;
    assert_eq!(w.state(amended), DraftState::Abandoned);
    w.set_head("w", Some(c1), Some(amended)).expect("amend");
    assert_eq!(w.state(amended), DraftState::Draft);
    let old = w.record(c1);
    assert_eq!(old.state, RecordState::Superseded);
    assert_eq!(old.superseded_by, Some(amended));
    assert_eq!(old.holder, None);

    // Moving down the stack abandons what leaves it.
    let c3 = w.commit_in("w", amended, b"3").expect("commit").id;
    w.set_head("w", Some(c3), Some(amended)).expect("down");
    assert_eq!(w.state(c3), DraftState::Abandoned);
    // ...and it can be adopted again.
    w.set_head("w", Some(amended), Some(c3)).expect("adopt");
    assert_eq!(w.state(c3), DraftState::Draft);
    w.set_head("w", Some(c3), None).expect("drop all");
    assert_eq!(w.state(c3), DraftState::Abandoned);
    assert_eq!(w.state(amended), DraftState::Abandoned);

    // A superseded, landed or foreign draft cannot join a stack.
    w.set_head("w", None, Some(amended)).expect("adopt");
    assert!(matches!(
        w.set_head("w", Some(amended), Some(c1)),
        Err(ApiError::InvalidChange { .. })
    ));
    assert!(matches!(
        w.set_head("w", Some(amended), Some(w.root)),
        Err(ApiError::InvalidChange { .. })
    ));
    w.open("other");
    assert!(matches!(
        w.set_head("other", None, Some(amended)),
        Err(ApiError::InvalidChange { .. })
    ));
}

fn expired_lease_abandons_drafts<M: MetaStore>(f: &Fixture<M>) {
    let w = Ws::new(f, "r");
    w.open_as(&w.repo.cx, "w", HOUR).expect("open");
    let c1 = w.commit_in("w", w.root, b"1").expect("commit").id;
    let c2 = w.commit_in("w", c1, b"2").expect("commit").id;
    f.clock.advance(HOUR as i64 + 1);
    assert_eq!(
        w.gauges(),
        Gauges {
            held: 2,
            abandoned: 0,
            superseded: 0,
            active: 0,
            lapsed: 1,
        }
    );
    assert_eq!(
        w.renew("w", HOUR),
        Err(ApiError::LeaseExpired {
            workspace: "w".to_owned(),
        })
    );
    for id in [c1, c2] {
        assert_eq!(w.state(id), DraftState::Abandoned);
    }
    assert!(matches!(w.get("w"), Err(ApiError::NotFound { .. })));

    // The periodic sweep does the same for workspaces nobody touches.
    w.open_as(&w.repo.cx, "x", HOUR).expect("open");
    let c3 = w.commit_in("x", w.root, b"3").expect("commit").id;
    w.open_as(&w.repo.cx, "y", 3 * HOUR).expect("open");
    f.clock.advance(HOUR as i64 + 1);
    assert_eq!(block_on(w.e().expire_leases("r", 0)).expect("expire"), 1);
    assert_eq!(w.state(c3), DraftState::Abandoned);
    assert!(w.get("y").is_ok());
    assert_eq!(
        w.gauges(),
        Gauges {
            held: 0,
            abandoned: 3,
            superseded: 0,
            active: 1,
            lapsed: 0,
        }
    );

    // An expired name can be taken again, by anyone; the old owner's
    // drafts stay abandoned and the old owner cannot commit into it.
    w.open_as(&w.repo.cx, "z", HOUR).expect("open");
    let c4 = w.commit_in("z", w.root, b"4").expect("commit").id;
    f.clock.advance(HOUR as i64 + 1);
    let bob = as_subject("bob", &["repo:r:write"]);
    let taken = w.open_as(&bob, "z", HOUR).expect("reopen");
    assert_eq!(taken.owner, "bob");
    assert_eq!(taken.head, None);
    assert_eq!(w.state(c4), DraftState::Abandoned);
    let alice = as_subject("tester", &["repo:r:write"]);
    assert!(matches!(
        w.commit_in_as(&alice, "z", w.root, b"5"),
        Err(ApiError::PermissionDenied { .. })
    ));
}

fn only_the_owner_or_an_admin_changes_a_workspace<M: MetaStore>(f: &Fixture<M>) {
    let w = Ws::new(f, "r");
    let bob = as_subject("bob", &["repo:r:write"]);
    w.open_as(&bob, "bobs", 0).expect("open");
    let mallory = as_subject("mallory", &["repo:r:write"]);
    let wref = w.wref("bobs");
    assert!(matches!(
        block_on(w.e().renew_lease(&mallory, wref.clone(), HOUR)),
        Err(ApiError::PermissionDenied { .. })
    ));
    assert!(matches!(
        block_on(w.e().close_workspace(&mallory, wref.clone(), false)),
        Err(ApiError::PermissionDenied { .. })
    ));
    assert!(matches!(
        w.commit_in_as(&mallory, "bobs", w.root, b"1"),
        Err(ApiError::PermissionDenied { .. })
    ));
    // An admin (Vogt renewing for a session) may.
    w.renew("bobs", HOUR).expect("admin renews");
    w.close("bobs").expect("admin closes");
}

fn set_head_checks_path_scopes_of_adopted_changesets<M: MetaStore>(f: &Fixture<M>) {
    let w = Ws::new(f, "r");
    let docs = as_subject("docs-writer", &["repo:r:write:docs"]);
    w.open_as(&docs, "docs", 0).expect("open");
    let set_head = |new| {
        block_on(w.e().set_head(
            &docs,
            SetHead {
                workspace: w.wref("docs"),
                expected: None,
                new: Some(new),
            },
        ))
    };
    // An abandoned changeset made by someone with wider scope.
    let secret = w.repo.commit(vec![w.root], &[("secret/x", b"1")], &[]).id;
    let before = (
        w.get("docs").expect("get"),
        w.record(secret),
        w.repo.journal(),
    );
    assert_eq!(
        set_head(secret),
        Err(ApiError::PermissionDenied {
            scope: "repo:r:write".to_owned(),
            path: Some("secret/x".to_owned()),
        })
    );
    assert_eq!(
        (
            w.get("docs").expect("get"),
            w.record(secret),
            w.repo.journal()
        ),
        before,
        "a refused adoption changes nothing"
    );
    // One change outside the prefix is enough to refuse.
    let mixed = w
        .repo
        .commit(vec![w.root], &[("docs/a", b"2"), ("src/b", b"3")], &[])
        .id;
    assert!(matches!(
        set_head(mixed),
        Err(ApiError::PermissionDenied { .. })
    ));
    // Within the prefix it is adopted.
    let fine = w.repo.commit(vec![w.root], &[("docs/a", b"4")], &[]).id;
    assert_eq!(set_head(fine).expect("adopt").head, Some(fine));
    assert_eq!(w.state(fine), DraftState::Draft);
}

fn opens_race_and_one_wins<M: MetaStore + 'static>(f: &Fixture<M>) {
    let w = Ws::new(f, "r");
    let results: Vec<_> = std::thread::scope(|s| {
        let handles: Vec<_> = (0..8)
            .map(|i| {
                let w = &w;
                s.spawn(move || {
                    let cx = as_subject(&format!("racer-{i}"), &["repo:r:write"]);
                    w.open_as(&cx, "contested", 0)
                })
            })
            .collect();
        handles
            .into_iter()
            .map(|h| h.join().expect("thread"))
            .collect()
    });
    let winners: Vec<_> = results.iter().filter_map(|r| r.as_ref().ok()).collect();
    assert_eq!(winners.len(), 1, "{results:?}");
    assert_eq!(w.get("contested").expect("get").owner, winners[0].owner);
    for r in results.iter().filter(|r| r.is_err()) {
        assert!(matches!(r, Err(ApiError::RefConflict { .. })), "{r:?}");
    }

    // Commits racing onto one head: one wins, the rest conflict, and every
    // draft has its holder.
    w.open("mine");
    let results: Vec<_> = std::thread::scope(|s| {
        let handles: Vec<_> = (1..=5u8)
            .map(|i| {
                let w = &w;
                s.spawn(move || w.commit_in("mine", w.root, &[b'0' + i]))
            })
            .collect();
        handles
            .into_iter()
            .map(|h| h.join().expect("thread"))
            .collect()
    });
    let won: Vec<_> = results.iter().filter_map(|r| r.as_ref().ok()).collect();
    assert_eq!(won.len(), 1, "{results:?}");
    assert_eq!(w.get("mine").expect("get").head, Some(won[0].id));
    assert_eq!(w.gauges().held, 1);
}

#[test]
fn stacks_are_bounded() {
    let f = memory_with(EngineConfig {
        max_stack: 3,
        ..EngineConfig::default()
    });
    let w = Ws::new(&f, "r");
    w.open("w");
    let mut head = w.root;
    for b in [b"1", b"2", b"3"] {
        head = w.commit_in("w", head, b).expect("commit").id;
    }
    assert_eq!(
        w.commit_in("w", head, b"4"),
        Err(ApiError::LimitExceeded {
            limit: "stack".to_owned(),
            max: 3,
        })
    );
    // A deeper chain made outside cannot become the head either.
    let mut deep = w.root;
    for b in [b"1" as &[u8], b"2", b"3", b"4"] {
        deep = w.repo.commit(vec![deep], &[("g", b)], &[]).id;
    }
    assert!(matches!(
        w.set_head("w", Some(head), Some(deep)),
        Err(ApiError::LimitExceeded { .. })
    ));
    // Nor can the base itself.
    w.open("v");
    assert!(matches!(
        w.set_head("v", None, Some(w.root)),
        Err(ApiError::InvalidChange { .. })
    ));
}

/// Wraps a store so that its next commit fails before or after it applies:
/// a crash before the commit point, or one after it whose answer is lost.
struct Crashy<M> {
    inner: M,
    /// 0: none; 1: fail before; 2: fail after.
    next: AtomicU8,
}

impl<M: MetaStore> Crashy<M> {
    fn arm(&self, mode: u8) {
        self.next.store(mode, Ordering::SeqCst);
    }
}

impl<M: MetaStore> MetaStore for Crashy<M> {
    async fn create_repo(&self, name: &str) -> StoreResult<RepoId> {
        self.inner.create_repo(name).await
    }
    async fn repo(&self, name: &str) -> StoreResult<Option<RepoId>> {
        self.inner.repo(name).await
    }
    async fn commit(&self, repo: RepoId, txn: MetaTxn) -> StoreResult<TxnOutcome> {
        match self.next.swap(0, Ordering::SeqCst) {
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
        self.inner.changeset(repo, id).await
    }
    async fn get_ref(&self, repo: RepoId, name: &str) -> StoreResult<Option<RefRecord>> {
        self.inner.get_ref(repo, name).await
    }
    async fn list_refs(
        &self,
        repo: RepoId,
        prefix: &str,
        limit: u32,
    ) -> StoreResult<Vec<RefRecord>> {
        self.inner.list_refs(repo, prefix, limit).await
    }
    async fn workspace(&self, repo: RepoId, name: &str) -> StoreResult<Option<WorkspaceRecord>> {
        self.inner.workspace(repo, name).await
    }
    async fn expired_workspaces(
        &self,
        repo: RepoId,
        before_ms: i64,
        limit: u32,
    ) -> StoreResult<Vec<WorkspaceRecord>> {
        self.inner.expired_workspaces(repo, before_ms, limit).await
    }
    async fn list_workspaces(
        &self,
        repo: RepoId,
        owner: &str,
        after: &str,
        limit: u32,
    ) -> StoreResult<Vec<WorkspaceRecord>> {
        self.inner.list_workspaces(repo, owner, after, limit).await
    }
    async fn count_records(&self, repo: RepoId, state: RecordState) -> StoreResult<u64> {
        self.inner.count_records(repo, state).await
    }
    async fn count_workspaces(&self, repo: RepoId, before_ms: Option<i64>) -> StoreResult<u64> {
        self.inner.count_workspaces(repo, before_ms).await
    }
    async fn derived(
        &self,
        repo: RepoId,
        kind: &str,
        version: u32,
        key: &[u8],
    ) -> StoreResult<Option<Vec<u8>>> {
        self.inner.derived(repo, kind, version, key).await
    }
    async fn line_pos(
        &self,
        repo: RepoId,
        line: &str,
        id: ChangesetId,
    ) -> StoreResult<Option<u64>> {
        self.inner.line_pos(repo, line, id).await
    }
    async fn line_holders(
        &self,
        repo: RepoId,
        id: ChangesetId,
        limit: u32,
    ) -> StoreResult<Vec<(String, u64)>> {
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
        self.inner
            .scan_line_path(repo, line, path, before, limit)
            .await
    }
    async fn holders(&self, repo: RepoId, object: ObjectId) -> StoreResult<u32> {
        self.inner.holders(repo, object).await
    }
    async fn land(
        &self,
        repo: RepoId,
        draft: ChangesetId,
        line: &str,
    ) -> StoreResult<Option<ChangesetId>> {
        self.inner.land(repo, draft, line).await
    }
    async fn journal(
        &self,
        repo: RepoId,
        from: Lsn,
        limit: u32,
    ) -> StoreResult<Vec<(Lsn, String, Vec<u8>)>> {
        self.inner.journal(repo, from, limit).await
    }
    async fn protection(&self, repo: RepoId, id: ChangesetId) -> StoreResult<Option<Protection>> {
        self.inner.protection(repo, id).await
    }
    async fn expired_records(
        &self,
        repo: RepoId,
        now_ms: i64,
        after: Option<(i64, ChangesetId)>,
        limit: u32,
    ) -> StoreResult<Vec<(i64, ChangesetId)>> {
        self.inner.expired_records(repo, now_ms, after, limit).await
    }
    async fn held_objects(
        &self,
        repo: RepoId,
        holder: &str,
        after: Option<ObjectId>,
        limit: u32,
    ) -> StoreResult<Vec<ObjectId>> {
        self.inner.held_objects(repo, holder, after, limit).await
    }
    async fn expired_rows(
        &self,
        repo: RepoId,
        now_ms: i64,
        after: Option<TimedRow>,
        limit: u32,
    ) -> StoreResult<Vec<TimedRow>> {
        self.inner.expired_rows(repo, now_ms, after, limit).await
    }
    async fn doom_marks(
        &self,
        repo: RepoId,
        ids: &[ObjectId],
    ) -> StoreResult<Vec<(ObjectId, DoomMark)>> {
        self.inner.doom_marks(repo, ids).await
    }
    async fn old_doomed(
        &self,
        repo: RepoId,
        until_ms: i64,
        after: Option<(i64, ObjectId)>,
        limit: u32,
    ) -> StoreResult<Vec<(ObjectId, DoomMark)>> {
        self.inner.old_doomed(repo, until_ms, after, limit).await
    }
    async fn lapsed_workspaces(
        &self,
        repo: RepoId,
        before_ms: i64,
        after: Option<(i64, String)>,
        limit: u32,
    ) -> StoreResult<Vec<WorkspaceRecord>> {
        self.inner
            .lapsed_workspaces(repo, before_ms, after, limit)
            .await
    }
    async fn gc_protection(&self, repo: RepoId) -> StoreResult<bool> {
        self.inner.gc_protection(repo).await
    }
}

#[test]
fn a_crash_at_the_commit_point_leaves_all_or_nothing() {
    let meta = Arc::new(Crashy {
        inner: MemoryStore::new(),
        next: AtomicU8::new(0),
    });
    let f = build(meta.clone(), EngineConfig::default(), None);
    let w = Ws::new(&f, "r");
    w.open("w");
    let journal = w.repo.journal();

    // Killed before the commit point: no draft, no head move, no entry.
    meta.arm(1);
    assert!(matches!(
        w.commit_in("w", w.root, b"1"),
        Err(ApiError::Unavailable { .. })
    ));
    assert_eq!(w.get("w").expect("get").head, None);
    assert_eq!(w.repo.journal(), journal);
    assert_eq!(w.gauges().held, 0);

    // Killed after it: the draft, its holder and the head moved together,
    // and the client's retry finds them.
    meta.arm(2);
    assert!(w.commit_in("w", w.root, b"1").is_err());
    let head = w.get("w").expect("get").head.expect("moved");
    assert_eq!(w.record(head).holder.as_deref(), Some("ws:w"));
    let retry = w.commit_in("w", w.root, b"1").expect("retry");
    assert_eq!(retry.id, head);
    assert_eq!(retry.state, DraftState::Draft);

    // A line move killed after its commit: ref, landed records and the
    // journal entry all made it.
    meta.arm(2);
    assert!(
        w.repo
            .update_ref("main", Some(w.root), head, RefKind::Line)
            .is_err()
    );
    assert_eq!(w.state(head), DraftState::Landed);
    let moved = w.repo.journal().pop().expect("entry");
    assert_eq!(moved.kind, "RefMoved");
    // A SetHead killed after its commit: retrying the CAS reports the
    // head it already moved to.
    let next = w.commit_in("w", head, b"2").expect("commit").id;
    meta.arm(2);
    assert!(w.set_head("w", Some(next), None).is_err());
    assert_eq!(
        w.set_head("w", Some(next), None),
        Err(ApiError::RefConflict {
            name: "w".to_owned(),
            actual: None,
        })
    );
    assert_eq!(w.state(next), DraftState::Abandoned);
}

on_both_stores!(
    open_get_list_renew_close,
    open_checks_line_name_and_scope,
    commits_in_a_workspace_are_held_drafts,
    stale_set_head_fails_and_changes_nothing,
    amend_supersedes_and_drop_abandons,
    expired_lease_abandons_drafts,
    only_the_owner_or_an_admin_changes_a_workspace,
    opens_race_and_one_wins,
    set_head_checks_path_scopes_of_adopted_changesets,
);
