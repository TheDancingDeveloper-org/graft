//! The port conformance suite (work item G1.2). Every store implementation
//! runs [`run_all`] from its own tests.
//!
//! The cases are black-box: they use only the [`ObjectStore`] and
//! [`MetaStore`] traits, so an implementation passes or fails on its
//! behaviour alone. Each case gets fresh stores from the factory.

use graft_model::{ChangesetId, ObjectId};

use crate::meta::{
    ChangesetRecord, MetaStore, MetaTxn, Precondition, RecordState, RefKind, RefRecord, TxnOutcome,
    WorkspaceRecord, Write,
};
use crate::object::ObjectStore;
use crate::{MAX_PAGE, RepoId, StoreError};

/// Runs every conformance case against stores made by `factory`. Each case
/// gets fresh stores.
pub async fn run_all<O, M, F>(factory: F)
where
    O: ObjectStore,
    M: MetaStore,
    F: Fn() -> (O, M),
{
    macro_rules! case {
        ($name:ident) => {{
            let (objects, meta) = factory();
            $name(&objects, &meta).await;
        }};
    }
    case!(object_put_is_put_if_absent);
    case!(object_delete_is_idempotent);
    case!(object_stats_count_frames);
    case!(repos_are_named_and_isolated);
    case!(changeset_insert_is_idempotent);
    case!(ref_cas_absent_stale_and_success);
    case!(ref_version_is_monotonic);
    case!(failed_transactions_write_nothing);
    case!(journal_lsns_are_monotonic_and_gap_free);
    case!(workspace_head_cas);
    case!(workspace_leases);
    case!(workspace_record_cas);
    case!(record_state_precondition);
    case!(workspaces_list_by_owner);
    case!(counts_follow_states_and_leases);
    case!(holder_rows_are_a_set);
    case!(land_records_are_idempotent);
    case!(derived_values_are_keyed_by_kind_version_and_key);
    case!(line_positions_are_per_line);
    case!(pagination_is_bounded);
    case!(index_reads_page_in_order);
}

fn cs(n: u8) -> ChangesetId {
    ChangesetId(ObjectId([n; 32]))
}

fn oid(n: u8) -> ObjectId {
    ObjectId([n; 32])
}

fn record(state: RecordState) -> ChangesetRecord {
    ChangesetRecord {
        lsn: 0,
        received_at_ms: 0,
        state,
        holder: None,
        superseded_by: None,
    }
}

fn journal(kind: &str) -> Write {
    Write::Journal {
        kind: kind.to_owned(),
        body: kind.as_bytes().to_vec(),
    }
}

fn txn(preconditions: Vec<Precondition>, writes: Vec<Write>) -> MetaTxn {
    MetaTxn {
        preconditions,
        writes,
    }
}

fn ref_rec(name: &str, target: ChangesetId, version: u64) -> RefRecord {
    RefRecord {
        name: name.to_owned(),
        target,
        version,
        kind: RefKind::Line,
    }
}

fn ws(name: &str, head: Option<ChangesetId>, lease_until_ms: i64) -> WorkspaceRecord {
    WorkspaceRecord {
        name: name.to_owned(),
        owner: "alice".to_owned(),
        session: None,
        line: "main".to_owned(),
        base: cs(1),
        head,
        lease_until_ms,
    }
}

async fn new_repo<M: MetaStore>(meta: &M, name: &str) -> RepoId {
    meta.create_repo(name).await.expect("create repo")
}

async fn run_txn<M: MetaStore>(meta: &M, repo: RepoId, t: MetaTxn) -> TxnOutcome {
    meta.commit(repo, t).await.expect("commit")
}

async fn lsns_of<M: MetaStore>(meta: &M, repo: RepoId) -> Vec<u64> {
    meta.journal(repo, 1, u32::MAX)
        .await
        .expect("journal")
        .into_iter()
        .map(|(lsn, _, _)| lsn)
        .collect()
}

fn names(rows: Vec<WorkspaceRecord>) -> Vec<String> {
    rows.into_iter().map(|w| w.name).collect()
}

async fn object_put_is_put_if_absent<O: ObjectStore, M: MetaStore>(objects: &O, meta: &M) {
    let repo = new_repo(meta, "objects").await;
    let id = oid(1);
    objects.put(repo, id, b"first".to_vec()).await.expect("put");
    objects
        .put(repo, id, b"first".to_vec())
        .await
        .expect("repeat put");
    objects
        .put(repo, id, b"other".to_vec())
        .await
        .expect("put over an existing id");
    assert_eq!(
        objects.get(repo, id).await.expect("get"),
        Some(b"first".to_vec()),
        "put-if-absent keeps the first frame"
    );
    assert_eq!(
        objects.has(repo, &[id, oid(2)]).await.expect("has"),
        vec![id],
        "has returns only the present ids"
    );
    assert_eq!(objects.get(repo, oid(2)).await.expect("get"), None);
}

async fn object_delete_is_idempotent<O: ObjectStore, M: MetaStore>(objects: &O, meta: &M) {
    let repo = new_repo(meta, "delete").await;
    let id = oid(1);
    objects.put(repo, id, b"gone".to_vec()).await.expect("put");
    objects.delete(repo, id).await.expect("delete");
    objects
        .delete(repo, id)
        .await
        .expect("deleting an absent object succeeds");
    assert_eq!(objects.get(repo, id).await.expect("get"), None);
    assert!(objects.has(repo, &[id]).await.expect("has").is_empty());
}

async fn object_stats_count_frames<O: ObjectStore, M: MetaStore>(objects: &O, meta: &M) {
    let repo = new_repo(meta, "stats").await;
    objects.put(repo, oid(1), vec![0; 3]).await.expect("put");
    objects.put(repo, oid(1), vec![0; 3]).await.expect("put");
    objects.put(repo, oid(2), vec![0; 5]).await.expect("put");
    let stats = objects.stats().await.expect("stats");
    assert_eq!(stats.objects, 2, "a repeated put is not a new object");
    assert_eq!(stats.bytes, 8, "bytes are the stored frame lengths");
    objects.delete(repo, oid(1)).await.expect("delete");
    let stats = objects.stats().await.expect("stats");
    assert_eq!((stats.objects, stats.bytes), (1, 5));
}

async fn repos_are_named_and_isolated<O: ObjectStore, M: MetaStore>(objects: &O, meta: &M) {
    let a = new_repo(meta, "a").await;
    let b = new_repo(meta, "b").await;
    assert_ne!(a, b, "repository ids are distinct");
    assert!(matches!(
        meta.create_repo("a").await,
        Err(StoreError::Exists)
    ));
    assert_eq!(meta.repo("a").await.expect("lookup"), Some(a));
    assert_eq!(meta.repo("missing").await.expect("lookup"), None);

    let t = txn(
        vec![],
        vec![
            Write::PutChangeset {
                id: cs(1),
                content: b"c".to_vec(),
                record: record(RecordState::Draft),
            },
            journal("ChangesetCreated"),
        ],
    );
    assert_eq!(run_txn(meta, a, t).await, TxnOutcome::Committed(vec![1]));
    objects.put(a, oid(7), b"x".to_vec()).await.expect("put");

    assert!(meta.changeset(a, cs(1)).await.expect("read").is_some());
    assert!(meta.changeset(b, cs(1)).await.expect("read").is_none());
    assert!(meta.journal(b, 1, 10).await.expect("journal").is_empty());
    assert!(objects.has(b, &[oid(7)]).await.expect("has").is_empty());

    let unknown = RepoId(u32::MAX);
    let refused = meta.commit(unknown, txn(vec![], vec![journal("x")])).await;
    assert!(
        matches!(refused, Err(StoreError::Invalid(_))),
        "a write to an unknown repository is refused"
    );
}

async fn changeset_insert_is_idempotent<O: ObjectStore, M: MetaStore>(_objects: &O, meta: &M) {
    let repo = new_repo(meta, "changesets").await;
    let id = cs(1);
    let insert = || {
        txn(
            vec![Precondition::ChangesetAbsent(id)],
            vec![
                Write::PutChangeset {
                    id,
                    content: b"body".to_vec(),
                    record: record(RecordState::Draft),
                },
                journal("ChangesetCreated"),
            ],
        )
    };
    assert_eq!(
        run_txn(meta, repo, insert()).await,
        TxnOutcome::Committed(vec![1])
    );
    assert_eq!(
        run_txn(meta, repo, insert()).await,
        TxnOutcome::PreconditionFailed(0),
        "a retried insert reports the existing changeset"
    );
    assert_eq!(lsns_of(meta, repo).await, vec![1], "a retry writes nothing");

    let (content, rec) = meta
        .changeset(repo, id)
        .await
        .expect("read")
        .expect("present");
    assert_eq!(content, b"body");
    assert_eq!(rec.lsn, 1, "the record carries its creating journal LSN");
    assert_eq!(rec.state, RecordState::Draft);

    let update = txn(
        vec![],
        vec![
            Write::SetRecord {
                id,
                record: record(RecordState::Landed),
            },
            journal("LandAttempt"),
        ],
    );
    assert_eq!(
        run_txn(meta, repo, update).await,
        TxnOutcome::Committed(vec![2])
    );
    let (content, rec) = meta
        .changeset(repo, id)
        .await
        .expect("read")
        .expect("present");
    assert_eq!(content, b"body", "a record update keeps the content");
    assert_eq!(rec.state, RecordState::Landed);
    assert_eq!(rec.lsn, 2);

    let orphan = txn(
        vec![],
        vec![
            Write::SetRecord {
                id: cs(9),
                record: record(RecordState::Landed),
            },
            journal("LandAttempt"),
        ],
    );
    assert!(matches!(
        meta.commit(repo, orphan).await,
        Err(StoreError::Invalid(_))
    ));
    assert_eq!(lsns_of(meta, repo).await, vec![1, 2]);
}

async fn ref_cas_absent_stale_and_success<O: ObjectStore, M: MetaStore>(_objects: &O, meta: &M) {
    let repo = new_repo(meta, "refs").await;
    let (r, s) = (cs(1), cs(2));
    let name = "line/main";
    let move_to = |expected: Option<(ChangesetId, u64)>, new: RefRecord| {
        txn(
            vec![Precondition::RefIs {
                name: name.to_owned(),
                expected,
            }],
            vec![Write::SetRef(new), journal("RefMoved")],
        )
    };

    // Expecting a version on an absent ref is stale.
    assert_eq!(
        run_txn(meta, repo, move_to(Some((r, 1)), ref_rec(name, r, 2))).await,
        TxnOutcome::PreconditionFailed(0)
    );
    // Creating the ref: expecting absence succeeds.
    assert_eq!(
        run_txn(meta, repo, move_to(None, ref_rec(name, r, 1))).await,
        TxnOutcome::Committed(vec![1])
    );
    assert_eq!(
        meta.get_ref(repo, name).await.expect("read"),
        Some(ref_rec(name, r, 1))
    );
    // Expecting absence on a present ref is stale.
    assert_eq!(
        run_txn(meta, repo, move_to(None, ref_rec(name, r, 1))).await,
        TxnOutcome::PreconditionFailed(0)
    );
    // A stale target fails, and so does a stale version.
    assert_eq!(
        run_txn(meta, repo, move_to(Some((s, 1)), ref_rec(name, s, 2))).await,
        TxnOutcome::PreconditionFailed(0),
        "stale target"
    );
    assert_eq!(
        run_txn(meta, repo, move_to(Some((r, 0)), ref_rec(name, s, 2))).await,
        TxnOutcome::PreconditionFailed(0),
        "stale version"
    );
    // The current expectation succeeds.
    assert_eq!(
        run_txn(meta, repo, move_to(Some((r, 1)), ref_rec(name, s, 2))).await,
        TxnOutcome::Committed(vec![2])
    );
    assert_eq!(
        meta.get_ref(repo, name).await.expect("read"),
        Some(ref_rec(name, s, 2))
    );
}

async fn ref_version_is_monotonic<O: ObjectStore, M: MetaStore>(_objects: &O, meta: &M) {
    let repo = new_repo(meta, "versions").await;
    let (r, s) = (cs(1), cs(2));
    let name = "line/main";
    assert_eq!(
        run_txn(
            meta,
            repo,
            txn(
                vec![Precondition::RefIs {
                    name: name.to_owned(),
                    expected: None,
                }],
                vec![Write::SetRef(ref_rec(name, r, 1)), journal("RefMoved")],
            ),
        )
        .await,
        TxnOutcome::Committed(vec![1])
    );

    // A set without a RefIs precondition on the same name is refused.
    let unguarded = txn(
        vec![],
        vec![Write::SetRef(ref_rec(name, s, 2)), journal("RefMoved")],
    );
    assert!(matches!(
        meta.commit(repo, unguarded).await,
        Err(StoreError::Invalid(_))
    ));

    // A version that does not move the ref forward is refused, even when
    // the precondition holds.
    let guard = || Precondition::RefIs {
        name: name.to_owned(),
        expected: Some((r, 1)),
    };
    for version in [1, 3] {
        let bad = txn(
            vec![guard()],
            vec![
                Write::SetRef(ref_rec(name, s, version)),
                journal("RefMoved"),
            ],
        );
        assert!(
            matches!(meta.commit(repo, bad).await, Err(StoreError::Invalid(_))),
            "version {version} does not follow 1"
        );
    }
    assert_eq!(
        meta.get_ref(repo, name).await.expect("read"),
        Some(ref_rec(name, r, 1)),
        "a refused set leaves the ref alone"
    );

    // Each success advances the version by one.
    assert_eq!(
        run_txn(
            meta,
            repo,
            txn(
                vec![guard()],
                vec![Write::SetRef(ref_rec(name, s, 2)), journal("RefMoved")],
            ),
        )
        .await,
        TxnOutcome::Committed(vec![2])
    );
    let guard2 = Precondition::RefIs {
        name: name.to_owned(),
        expected: Some((s, 2)),
    };
    assert_eq!(
        run_txn(
            meta,
            repo,
            txn(
                vec![guard2],
                vec![Write::SetRef(ref_rec(name, r, 3)), journal("RefMoved")],
            ),
        )
        .await,
        TxnOutcome::Committed(vec![3])
    );
    assert_eq!(
        lsns_of(meta, repo).await,
        vec![1, 2, 3],
        "only the successful moves were journalled"
    );
}

async fn failed_transactions_write_nothing<O: ObjectStore, M: MetaStore>(_objects: &O, meta: &M) {
    let repo = new_repo(meta, "atomic").await;
    let name = "line/x";
    let partial_writes = || {
        vec![
            Write::PutChangeset {
                id: cs(3),
                content: b"c".to_vec(),
                record: record(RecordState::Draft),
            },
            Write::PutWorkspace(ws("w", None, 10)),
            Write::PutDerived {
                kind: "tree".to_owned(),
                version: 1,
                key: b"k".to_vec(),
                value: b"v".to_vec(),
            },
            journal("ChangesetCreated"),
        ]
    };

    // A failed precondition writes nothing.
    let stale = txn(
        vec![Precondition::RefIs {
            name: name.to_owned(),
            expected: Some((cs(1), 1)),
        }],
        partial_writes(),
    );
    assert_eq!(
        run_txn(meta, repo, stale).await,
        TxnOutcome::PreconditionFailed(0)
    );

    // A refused write, after valid ones, writes nothing either.
    let refused = txn(
        vec![Precondition::RefIs {
            name: name.to_owned(),
            expected: None,
        }],
        {
            let mut writes = partial_writes();
            writes.insert(2, Write::SetRef(ref_rec(name, cs(1), 5)));
            writes
        },
    );
    assert!(matches!(
        meta.commit(repo, refused).await,
        Err(StoreError::Invalid(_))
    ));

    assert_eq!(meta.changeset(repo, cs(3)).await.expect("read"), None);
    assert_eq!(meta.workspace(repo, "w").await.expect("read"), None);
    assert_eq!(
        meta.derived(repo, "tree", 1, b"k").await.expect("read"),
        None
    );
    assert_eq!(meta.get_ref(repo, name).await.expect("read"), None);
    assert!(meta.journal(repo, 1, 10).await.expect("journal").is_empty());
}

async fn journal_lsns_are_monotonic_and_gap_free<O: ObjectStore, M: MetaStore>(
    _objects: &O,
    meta: &M,
) {
    let repo = new_repo(meta, "log").await;
    for n in 1u64..=3 {
        let t = txn(vec![], vec![journal(&format!("k{n}"))]);
        assert_eq!(run_txn(meta, repo, t).await, TxnOutcome::Committed(vec![n]));
    }

    // A failed transaction takes no LSN.
    let stale = txn(
        vec![Precondition::RefIs {
            name: "nope".to_owned(),
            expected: Some((cs(1), 1)),
        }],
        vec![journal("never")],
    );
    assert_eq!(
        run_txn(meta, repo, stale).await,
        TxnOutcome::PreconditionFailed(0)
    );

    // Several entries in one transaction take consecutive LSNs.
    let pair = txn(vec![], vec![journal("k4"), journal("k5")]);
    assert_eq!(
        run_txn(meta, repo, pair).await,
        TxnOutcome::Committed(vec![4, 5])
    );

    let all = meta.journal(repo, 1, 100).await.expect("journal");
    assert_eq!(
        all,
        (1u64..=5)
            .map(|n| (n, format!("k{n}"), format!("k{n}").into_bytes()))
            .collect::<Vec<_>>(),
        "entries come back in LSN order, with their kind and body"
    );
    let tail: Vec<u64> = meta
        .journal(repo, 3, 100)
        .await
        .expect("journal")
        .into_iter()
        .map(|(lsn, _, _)| lsn)
        .collect();
    assert_eq!(tail, vec![3, 4, 5], "from is inclusive");
    assert!(
        meta.journal(repo, 6, 100)
            .await
            .expect("journal")
            .is_empty()
    );

    // LSNs are per repository.
    let other = new_repo(meta, "log-other").await;
    assert_eq!(
        run_txn(meta, other, txn(vec![], vec![journal("x")])).await,
        TxnOutcome::Committed(vec![1])
    );
}

async fn workspace_head_cas<O: ObjectStore, M: MetaStore>(_objects: &O, meta: &M) {
    let repo = new_repo(meta, "workspaces").await;
    let (c1, c2) = (cs(1), cs(2));
    let head_is = |name: &str, head: Option<ChangesetId>| Precondition::WorkspaceHeadIs {
        name: name.to_owned(),
        head,
    };

    assert_eq!(
        run_txn(
            meta,
            repo,
            txn(
                vec![],
                vec![
                    Write::PutWorkspace(ws("ws-a", None, 1000)),
                    journal("WorkspaceOpened")
                ],
            ),
        )
        .await,
        TxnOutcome::Committed(vec![1])
    );

    // An absent workspace never matches, not even for "no head".
    assert_eq!(
        run_txn(
            meta,
            repo,
            txn(
                vec![head_is("ws-x", None)],
                vec![
                    Write::PutWorkspace(ws("ws-x", None, 1)),
                    journal("WorkspaceOpened")
                ],
            ),
        )
        .await,
        TxnOutcome::PreconditionFailed(0)
    );
    assert_eq!(meta.workspace(repo, "ws-x").await.expect("read"), None);

    // A stale head fails and writes nothing.
    assert_eq!(
        run_txn(
            meta,
            repo,
            txn(
                vec![head_is("ws-a", Some(c2))],
                vec![
                    Write::PutWorkspace(ws("ws-a", Some(c1), 1000)),
                    journal("WorkspaceRenewed")
                ],
            ),
        )
        .await,
        TxnOutcome::PreconditionFailed(0)
    );

    // The current head moves; a second move from the old head is stale.
    assert_eq!(
        run_txn(
            meta,
            repo,
            txn(
                vec![head_is("ws-a", None)],
                vec![
                    Write::PutWorkspace(ws("ws-a", Some(c2), 1000)),
                    journal("WorkspaceRenewed")
                ],
            ),
        )
        .await,
        TxnOutcome::Committed(vec![2])
    );
    assert_eq!(
        run_txn(
            meta,
            repo,
            txn(
                vec![head_is("ws-a", None)],
                vec![
                    Write::PutWorkspace(ws("ws-a", Some(c1), 1000)),
                    journal("WorkspaceRenewed")
                ],
            ),
        )
        .await,
        TxnOutcome::PreconditionFailed(0)
    );
    assert_eq!(
        run_txn(
            meta,
            repo,
            txn(
                vec![head_is("ws-a", Some(c2))],
                vec![
                    Write::PutWorkspace(ws("ws-a", Some(c1), 1000)),
                    journal("WorkspaceRenewed")
                ],
            ),
        )
        .await,
        TxnOutcome::Committed(vec![3])
    );
    let stored = meta
        .workspace(repo, "ws-a")
        .await
        .expect("read")
        .expect("present");
    assert_eq!(stored.head, Some(c1));
}

async fn workspace_leases<O: ObjectStore, M: MetaStore>(_objects: &O, meta: &M) {
    let repo = new_repo(meta, "leases").await;
    for (name, lease) in [("ws-a", 1000), ("ws-b", 3000)] {
        let t = txn(
            vec![],
            vec![
                Write::PutWorkspace(ws(name, None, lease)),
                journal("WorkspaceOpened"),
            ],
        );
        assert!(matches!(
            run_txn(meta, repo, t).await,
            TxnOutcome::Committed(_)
        ));
    }

    let expired = |before: i64| meta.expired_workspaces(repo, before, 10);
    assert!(expired(500).await.expect("expired").is_empty());
    assert_eq!(
        names(expired(2000).await.expect("expired")),
        vec!["ws-a"],
        "a lease ends before the cut-off"
    );
    assert_eq!(
        names(expired(4000).await.expect("expired")),
        vec!["ws-a", "ws-b"]
    );

    // Renewing a lease takes the workspace out of the expired set.
    let renew = txn(
        vec![],
        vec![
            Write::PutWorkspace(ws("ws-a", None, 5000)),
            journal("WorkspaceRenewed"),
        ],
    );
    assert!(matches!(
        run_txn(meta, repo, renew).await,
        TxnOutcome::Committed(_)
    ));
    assert_eq!(names(expired(4000).await.expect("expired")), vec!["ws-b"]);

    // Closing a workspace removes it; closing it again is a no-op.
    for _ in 0..2 {
        let close = txn(
            vec![],
            vec![
                Write::DeleteWorkspace("ws-b".to_owned()),
                journal("WorkspaceClosed"),
            ],
        );
        assert!(matches!(
            run_txn(meta, repo, close).await,
            TxnOutcome::Committed(_)
        ));
    }
    assert_eq!(meta.workspace(repo, "ws-b").await.expect("read"), None);
    assert!(expired(4000).await.expect("expired").is_empty());
}

async fn holder_rows_are_a_set<O: ObjectStore, M: MetaStore>(_objects: &O, meta: &M) {
    let repo = new_repo(meta, "holders").await;
    let (object, other) = (oid(1), oid(2));
    assert_eq!(meta.holders(repo, object).await.expect("count"), 0);

    let add = txn(
        vec![],
        vec![
            Write::AddHolder {
                object,
                holder: "ws:a".to_owned(),
            },
            Write::AddHolder {
                object,
                holder: "ws:b".to_owned(),
            },
            Write::AddHolder {
                object,
                holder: "ws:a".to_owned(),
            },
            journal("HolderAdded"),
        ],
    );
    assert!(matches!(
        run_txn(meta, repo, add).await,
        TxnOutcome::Committed(_)
    ));
    assert_eq!(
        meta.holders(repo, object).await.expect("count"),
        2,
        "a repeated (object, holder) row counts once"
    );
    assert_eq!(meta.holders(repo, other).await.expect("count"), 0);

    let remove = |holder: &str| {
        txn(
            vec![],
            vec![
                Write::RemoveHolder {
                    object,
                    holder: holder.to_owned(),
                },
                journal("HolderRemoved"),
            ],
        )
    };
    assert!(matches!(
        run_txn(meta, repo, remove("ws:a")).await,
        TxnOutcome::Committed(_)
    ));
    assert_eq!(meta.holders(repo, object).await.expect("count"), 1);
    assert!(matches!(
        run_txn(meta, repo, remove("ws:a")).await,
        TxnOutcome::Committed(_)
    ));
    assert_eq!(
        meta.holders(repo, object).await.expect("count"),
        1,
        "removing an absent row is a no-op"
    );
    assert!(matches!(
        run_txn(meta, repo, remove("ws:b")).await,
        TxnOutcome::Committed(_)
    ));
    assert_eq!(meta.holders(repo, object).await.expect("count"), 0);
}

async fn land_records_are_idempotent<O: ObjectStore, M: MetaStore>(_objects: &O, meta: &M) {
    let repo = new_repo(meta, "lands").await;
    let (draft, first, second) = (cs(3), cs(4), cs(5));
    assert_eq!(meta.land(repo, draft, "main").await.expect("land"), None);

    let land = |landed: ChangesetId| {
        txn(
            vec![],
            vec![
                Write::PutLand {
                    draft,
                    line: "main".to_owned(),
                    landed,
                },
                journal("LandAttempt"),
            ],
        )
    };
    assert!(matches!(
        run_txn(meta, repo, land(first)).await,
        TxnOutcome::Committed(_)
    ));
    assert_eq!(
        meta.land(repo, draft, "main").await.expect("land"),
        Some(first)
    );
    // A retried land keeps the first result.
    assert!(matches!(
        run_txn(meta, repo, land(second)).await,
        TxnOutcome::Committed(_)
    ));
    assert_eq!(
        meta.land(repo, draft, "main").await.expect("land"),
        Some(first)
    );
    assert_eq!(meta.land(repo, draft, "other").await.expect("land"), None);
}

async fn derived_values_are_keyed_by_kind_version_and_key<O: ObjectStore, M: MetaStore>(
    _objects: &O,
    meta: &M,
) {
    let repo = new_repo(meta, "derived").await;
    let put = |version: u32, value: &[u8]| {
        txn(
            vec![],
            vec![
                Write::PutDerived {
                    kind: "tree".to_owned(),
                    version,
                    key: b"k".to_vec(),
                    value: value.to_vec(),
                },
                journal("Derived"),
            ],
        )
    };
    assert!(matches!(
        run_txn(meta, repo, put(1, b"v1")).await,
        TxnOutcome::Committed(_)
    ));
    let get = |kind: &str, version: u32, key: &[u8]| {
        let (kind, key) = (kind.to_owned(), key.to_vec());
        async move {
            meta.derived(repo, &kind, version, &key)
                .await
                .expect("read")
        }
    };
    assert_eq!(get("tree", 1, b"k").await, Some(b"v1".to_vec()));
    assert_eq!(
        get("tree", 2, b"k").await,
        None,
        "another version is another key"
    );
    assert_eq!(get("tree", 1, b"j").await, None);
    assert_eq!(get("trie", 1, b"k").await, None);

    assert!(matches!(
        run_txn(meta, repo, put(1, b"v2")).await,
        TxnOutcome::Committed(_)
    ));
    assert_eq!(get("tree", 1, b"k").await, Some(b"v2".to_vec()));
}

async fn line_positions_are_per_line<O: ObjectStore, M: MetaStore>(_objects: &O, meta: &M) {
    let repo = new_repo(meta, "positions").await;
    let set = |line: &str, changeset: ChangesetId, pos: u64| {
        txn(
            vec![],
            vec![
                Write::PutLinePos {
                    line: line.to_owned(),
                    changeset,
                    pos,
                },
                journal("LinePosition"),
            ],
        )
    };
    assert!(matches!(
        run_txn(meta, repo, set("main", cs(1), 7)).await,
        TxnOutcome::Committed(_)
    ));
    assert_eq!(
        meta.line_pos(repo, "main", cs(1)).await.expect("read"),
        Some(7)
    );
    assert_eq!(
        meta.line_pos(repo, "main", cs(2)).await.expect("read"),
        None
    );
    assert_eq!(
        meta.line_pos(repo, "other", cs(1)).await.expect("read"),
        None
    );
}

async fn pagination_is_bounded<O: ObjectStore, M: MetaStore>(_objects: &O, meta: &M) {
    let repo = new_repo(meta, "pages").await;
    let refs = ["line/a", "line/b", "line/c", "line/d", "line/e", "tag/x"];
    for name in refs {
        let t = txn(
            vec![Precondition::RefIs {
                name: name.to_owned(),
                expected: None,
            }],
            vec![Write::SetRef(ref_rec(name, cs(1), 1)), journal("RefMoved")],
        );
        assert!(matches!(
            run_txn(meta, repo, t).await,
            TxnOutcome::Committed(_)
        ));
    }
    let page = |prefix: &str, limit: u32| {
        let prefix = prefix.to_owned();
        async move { meta.list_refs(repo, &prefix, limit).await.expect("list") }
    };
    let first_two: Vec<String> = page("line/", 2).await.into_iter().map(|r| r.name).collect();
    assert_eq!(
        first_two,
        vec!["line/a", "line/b"],
        "sorted, and the limit holds"
    );
    assert_eq!(
        page("line/", 0).await.len(),
        1,
        "a zero limit is raised to one"
    );
    assert_eq!(page("", u32::MAX).await.len(), refs.len());

    // One transaction with more journal entries than a page holds.
    let bulk = (0..MAX_PAGE + 5).map(|_| journal("bulk")).collect();
    let outcome = run_txn(meta, repo, txn(vec![], bulk)).await;
    let TxnOutcome::Committed(lsns) = outcome else {
        panic!("bulk commit failed: {outcome:?}");
    };
    assert_eq!(lsns.len(), MAX_PAGE as usize + 5);
    let total = refs.len() as u64 + lsns.len() as u64;
    assert_eq!(
        meta.journal(repo, 1, u32::MAX)
            .await
            .expect("journal")
            .len(),
        MAX_PAGE as usize,
        "a journal page is capped at the page bound"
    );
    let tail = meta
        .journal(repo, total - 4, u32::MAX)
        .await
        .expect("journal");
    assert_eq!(
        tail.iter().map(|(lsn, _, _)| *lsn).collect::<Vec<_>>(),
        (total - 4..=total).collect::<Vec<_>>()
    );

    // Expired-workspace pages honour the limit too.
    for name in ["ws-1", "ws-2", "ws-3"] {
        let t = txn(
            vec![],
            vec![
                Write::PutWorkspace(ws(name, None, 1)),
                journal("WorkspaceOpened"),
            ],
        );
        assert!(matches!(
            run_txn(meta, repo, t).await,
            TxnOutcome::Committed(_)
        ));
    }
    assert_eq!(
        meta.expired_workspaces(repo, 100, 2)
            .await
            .expect("expired")
            .len(),
        2
    );
    assert_eq!(
        meta.expired_workspaces(repo, 100, u32::MAX)
            .await
            .expect("expired")
            .len(),
        3
    );
}

async fn workspace_record_cas<O: ObjectStore, M: MetaStore>(_objects: &O, meta: &M) {
    let repo = new_repo(meta, "workspace-cas").await;
    let is = |expected: Option<WorkspaceRecord>| Precondition::WorkspaceIs {
        name: "ws-a".to_owned(),
        expected,
    };
    let open = ws("ws-a", None, 1000);

    // Absent: an open succeeds once; a second open of the name fails.
    for want in [
        TxnOutcome::Committed(vec![1]),
        TxnOutcome::PreconditionFailed(0),
    ] {
        let t = txn(
            vec![is(None)],
            vec![
                Write::PutWorkspace(open.clone()),
                journal("WorkspaceOpened"),
            ],
        );
        assert_eq!(run_txn(meta, repo, t).await, want);
    }

    // A stale record (any field) fails and writes nothing.
    let mut renewed = open.clone();
    renewed.lease_until_ms = 2000;
    let mut stale = open.clone();
    stale.lease_until_ms = 999;
    let t = txn(
        vec![is(Some(stale))],
        vec![
            Write::PutWorkspace(renewed.clone()),
            journal("WorkspaceRenewed"),
        ],
    );
    assert_eq!(
        run_txn(meta, repo, t).await,
        TxnOutcome::PreconditionFailed(0)
    );
    assert_eq!(
        meta.workspace(repo, "ws-a").await.expect("read"),
        Some(open.clone())
    );

    // The current record matches.
    let t = txn(
        vec![is(Some(open.clone()))],
        vec![
            Write::PutWorkspace(renewed.clone()),
            journal("WorkspaceRenewed"),
        ],
    );
    assert_eq!(run_txn(meta, repo, t).await, TxnOutcome::Committed(vec![2]));

    // Close by CAS, then the absent expectation holds again.
    let t = txn(
        vec![is(Some(renewed))],
        vec![
            Write::DeleteWorkspace("ws-a".to_owned()),
            journal("WorkspaceClosed"),
        ],
    );
    assert_eq!(run_txn(meta, repo, t).await, TxnOutcome::Committed(vec![3]));
    let t = txn(vec![is(None)], vec![]);
    assert_eq!(run_txn(meta, repo, t).await, TxnOutcome::Committed(vec![]));
}

async fn record_state_precondition<O: ObjectStore, M: MetaStore>(_objects: &O, meta: &M) {
    let repo = new_repo(meta, "record-cas").await;
    let is = |state: RecordState| Precondition::RecordStateIs { id: cs(1), state };

    // An absent changeset matches no state.
    let t = txn(vec![is(RecordState::Draft)], vec![journal("x")]);
    assert_eq!(
        run_txn(meta, repo, t).await,
        TxnOutcome::PreconditionFailed(0)
    );

    let t = txn(
        vec![],
        vec![
            Write::PutChangeset {
                id: cs(1),
                content: b"c1".to_vec(),
                record: record(RecordState::Draft),
            },
            journal("ChangesetCreated"),
        ],
    );
    assert_eq!(run_txn(meta, repo, t).await, TxnOutcome::Committed(vec![1]));

    let to = |state| Write::SetRecord {
        id: cs(1),
        record: record(state),
    };
    let t = txn(
        vec![is(RecordState::Abandoned)],
        vec![to(RecordState::Landed)],
    );
    assert_eq!(
        run_txn(meta, repo, t).await,
        TxnOutcome::PreconditionFailed(0)
    );
    let t = txn(
        vec![is(RecordState::Draft)],
        vec![to(RecordState::Abandoned)],
    );
    assert_eq!(run_txn(meta, repo, t).await, TxnOutcome::Committed(vec![]));
    let (_, rec) = meta
        .changeset(repo, cs(1))
        .await
        .expect("read")
        .expect("stored");
    assert_eq!(rec.state, RecordState::Abandoned);
}

async fn workspaces_list_by_owner<O: ObjectStore, M: MetaStore>(_objects: &O, meta: &M) {
    let repo = new_repo(meta, "ws-list").await;
    let other = new_repo(meta, "ws-list-other").await;
    let mut writes = Vec::new();
    for (name, owner) in [("d", "alice"), ("b", "bob"), ("a", "alice"), ("c", "alice")] {
        let mut w = ws(name, None, 1000);
        w.owner = owner.to_owned();
        writes.push(Write::PutWorkspace(w));
    }
    run_txn(meta, repo, txn(vec![], writes)).await;
    let mut elsewhere = ws("z", None, 1000);
    elsewhere.owner = "alice".to_owned();
    run_txn(
        meta,
        other,
        txn(vec![], vec![Write::PutWorkspace(elsewhere)]),
    )
    .await;

    let list = |after: &'static str, limit| meta.list_workspaces(repo, "alice", after, limit);
    assert_eq!(names(list("", 10).await.expect("list")), ["a", "c", "d"]);
    assert_eq!(names(list("", 2).await.expect("list")), ["a", "c"]);
    assert_eq!(names(list("c", 2).await.expect("list")), ["d"]);
    assert!(list("d", 2).await.expect("list").is_empty());
    assert_eq!(
        names(
            meta.list_workspaces(repo, "bob", "", 0)
                .await
                .expect("list")
        ),
        ["b"],
        "a zero limit still returns one row"
    );
    assert!(
        meta.list_workspaces(repo, "carol", "", 10)
            .await
            .expect("list")
            .is_empty()
    );
}

async fn counts_follow_states_and_leases<O: ObjectStore, M: MetaStore>(_objects: &O, meta: &M) {
    let repo = new_repo(meta, "counts").await;
    let other = new_repo(meta, "counts-other").await;
    let mut writes = Vec::new();
    for (n, state) in [
        (1, RecordState::Draft),
        (2, RecordState::Draft),
        (3, RecordState::Abandoned),
        (4, RecordState::Landed),
    ] {
        writes.push(Write::PutChangeset {
            id: cs(n),
            content: vec![n],
            record: record(state),
        });
    }
    writes.push(Write::PutWorkspace(ws("a", None, 1000)));
    writes.push(Write::PutWorkspace(ws("b", None, 3000)));
    run_txn(meta, repo, txn(vec![], writes)).await;
    run_txn(
        meta,
        other,
        txn(
            vec![],
            vec![
                Write::PutChangeset {
                    id: cs(9),
                    content: vec![9],
                    record: record(RecordState::Draft),
                },
                Write::PutWorkspace(ws("c", None, 0)),
            ],
        ),
    )
    .await;

    let count = |state| meta.count_records(repo, state);
    assert_eq!(count(RecordState::Draft).await.expect("count"), 2);
    assert_eq!(count(RecordState::Abandoned).await.expect("count"), 1);
    assert_eq!(count(RecordState::Superseded).await.expect("count"), 0);
    let workspaces = |before| meta.count_workspaces(repo, before);
    assert_eq!(workspaces(None).await.expect("count"), 2);
    assert_eq!(workspaces(Some(1000)).await.expect("count"), 0);
    assert_eq!(workspaces(Some(1001)).await.expect("count"), 1);
    assert_eq!(workspaces(Some(i64::MAX)).await.expect("count"), 2);
}

async fn index_reads_page_in_order<O: ObjectStore, M: MetaStore>(_objects: &O, meta: &M) {
    let repo = new_repo(meta, "index").await;
    let other = new_repo(meta, "index-other").await;
    let mut writes = Vec::new();
    for (n, state) in [
        (5, RecordState::Draft),
        (1, RecordState::Abandoned),
        (3, RecordState::Draft),
        (2, RecordState::Draft),
        (4, RecordState::Superseded),
    ] {
        writes.push(Write::PutChangeset {
            id: cs(n),
            content: vec![n],
            record: record(state),
        });
    }
    run_txn(meta, repo, txn(vec![], writes)).await;
    run_txn(
        meta,
        other,
        txn(
            vec![],
            vec![Write::PutChangeset {
                id: cs(9),
                content: vec![9],
                record: record(RecordState::Draft),
            }],
        ),
    )
    .await;

    // Drafts in id order, two at a time, resuming after the last id.
    let first = meta
        .list_records(repo, RecordState::Draft, None, 2)
        .await
        .expect("page");
    assert_eq!(
        first.iter().map(|(id, _)| *id).collect::<Vec<_>>(),
        [cs(2), cs(3)]
    );
    let second = meta
        .list_records(repo, RecordState::Draft, Some(cs(3)), 2)
        .await
        .expect("page");
    assert_eq!(
        second.iter().map(|(id, _)| *id).collect::<Vec<_>>(),
        [cs(5)]
    );
    assert_eq!(second[0].1.state, RecordState::Draft);
    let rest = meta
        .list_records(repo, RecordState::Draft, Some(cs(5)), 2)
        .await
        .expect("page");
    assert!(rest.is_empty());
    let abandoned = meta
        .list_records(repo, RecordState::Abandoned, None, 10)
        .await
        .expect("page");
    assert_eq!(
        abandoned.iter().map(|(id, _)| *id).collect::<Vec<_>>(),
        [cs(1)]
    );
    let superseded = meta
        .list_records(repo, RecordState::Superseded, None, 10)
        .await
        .expect("page");
    assert_eq!(
        superseded.iter().map(|(id, _)| *id).collect::<Vec<_>>(),
        [cs(4)]
    );

    // Holder names of one object, in name order, resumable.
    let object = oid(7);
    let add = |holder: &str| Write::AddHolder {
        object,
        holder: holder.to_owned(),
    };
    run_txn(
        meta,
        repo,
        txn(
            vec![],
            vec![add("upload:9:a"), add("c1"), add("ws:b"), add("ws:a")],
        ),
    )
    .await;
    let page = meta.holder_rows(repo, object, None, 2).await.expect("rows");
    assert_eq!(page, ["c1", "upload:9:a"]);
    let page = meta
        .holder_rows(repo, object, Some("upload:9:a".to_owned()), 2)
        .await
        .expect("rows");
    assert_eq!(page, ["ws:a", "ws:b"]);
    let all = meta
        .holder_rows(repo, object, None, 100)
        .await
        .expect("rows");
    assert_eq!(
        all.len() as u32,
        meta.holders(repo, object).await.expect("count")
    );
    assert!(
        meta.holder_rows(repo, oid(8), None, 10)
            .await
            .expect("rows")
            .is_empty()
    );
}
