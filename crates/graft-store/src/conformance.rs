//! The port conformance suite (work item G1.2). Every store implementation
//! runs [`run_all`] from its own tests.
//!
//! The cases are black-box: they use only the [`ObjectStore`] and
//! [`MetaStore`] traits, so an implementation passes or fails on its
//! behaviour alone. Each case gets fresh stores from the factory.

use graft_model::{ChangesetId, ObjectId, RepoPath};

use crate::meta::{
    ChangesetRecord, DoomMark, MetaStore, MetaTxn, Precondition, Protection, RecordState, RefKind,
    RefRecord, TimedRow, TxnOutcome, WorkspaceRecord, Write,
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
    case!(line_forks_fall_through_to_the_parent);
    case!(pagination_is_bounded);
    case!(index_reads_page_in_order);
    case!(all_workspaces_page_in_name_order);
    case!(pins_follow_the_parent_state);
    case!(protection_writes_are_checked_and_atomic);
    case!(expiry_is_indexed_for_collection);
    case!(record_preconditions_compare_protection);
    case!(rows_read_by_holder_and_by_expiry);
    case!(doomed_marks_claims_and_deletes);
    case!(old_doomed_page_in_time_order);
    case!(lapsed_workspaces_page_in_lease_order);
    case!(gc_protection_marker);
    case!(derived_value_cas);
    case!(a_collected_changeset_is_replaced);
    case!(draft_path_index);
    case!(workspace_work_item_round_trips);
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
        work_item: None,
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

/// `main` holds positions 0..=4; `b` forks from it at 2 and lands 3 and 4;
/// `c` forks from `b` at 3 and lands 4. Reads below a fork follow the parent,
/// bounded by the fork position.
async fn line_forks_fall_through_to_the_parent<O: ObjectStore, M: MetaStore>(
    _objects: &O,
    meta: &M,
) {
    let repo = new_repo(meta, "forks").await;
    let path = RepoPath::new("a").expect("path");
    let mut writes = Vec::new();
    let mut land = |line: &str, pos: u64, id: ChangesetId| {
        writes.push(Write::PutLinePos {
            line: line.to_owned(),
            changeset: id,
            pos,
        });
        writes.push(Write::PutLinePath {
            line: line.to_owned(),
            path: path.clone(),
            pos,
            changeset: id,
        });
    };
    for pos in 0..5u8 {
        land("main", u64::from(pos), cs(10 + pos));
    }
    land("b", 3, cs(20));
    land("b", 4, cs(21));
    land("c", 4, cs(30));
    writes.push(Write::PutLineFork {
        line: "b".to_owned(),
        parent: "main".to_owned(),
        pos: 2,
    });
    writes.push(Write::PutLineFork {
        line: "c".to_owned(),
        parent: "b".to_owned(),
        pos: 3,
    });
    writes.push(journal("LineFork"));
    assert!(matches!(
        run_txn(meta, repo, txn(vec![], writes)).await,
        TxnOutcome::Committed(_)
    ));

    let pos = |line: &'static str, id: ChangesetId| async move {
        meta.line_pos(repo, line, id).await.expect("read")
    };
    assert_eq!(pos("b", cs(12)).await, Some(2));
    assert_eq!(pos("b", cs(13)).await, None, "above the fork on main");
    assert_eq!(pos("b", cs(20)).await, Some(3));
    assert_eq!(pos("c", cs(10)).await, Some(0), "a fork of a fork");
    assert_eq!(pos("c", cs(20)).await, Some(3));
    assert_eq!(pos("c", cs(21)).await, None, "above the fork on b");
    assert_eq!(pos("c", cs(13)).await, None);
    assert_eq!(pos("c", cs(30)).await, Some(4));
    assert_eq!(
        pos("main", cs(20)).await,
        None,
        "a parent never sees a child"
    );

    let scan = |before: u64, limit: u32| {
        let path = path.clone();
        async move {
            meta.scan_line_path(repo, "c", &path, before, limit)
                .await
                .expect("scan")
        }
    };
    assert_eq!(
        scan(u64::MAX, 10).await,
        vec![
            (4, cs(30)),
            (3, cs(20)),
            (2, cs(12)),
            (1, cs(11)),
            (0, cs(10))
        ]
    );
    assert_eq!(scan(u64::MAX, 2).await, vec![(4, cs(30)), (3, cs(20))]);
    assert_eq!(scan(3, 2).await, vec![(2, cs(12)), (1, cs(11))]);

    // Holders are lines with their own row: `b` and `c` hold cs(10) only
    // through forks.
    let holders = |id| async move { meta.line_holders(repo, id, 10).await.expect("holders") };
    assert_eq!(holders(cs(10)).await, vec![("main".to_owned(), 0)]);
    assert_eq!(holders(cs(20)).await, vec![("b".to_owned(), 3)]);
    assert_eq!(holders(cs(99)).await, vec![]);

    // One fork per line, ever: a second fork row is refused and changes
    // nothing, alone or twice in one transaction.
    let fork = |line: &str, pos| Write::PutLineFork {
        line: line.to_owned(),
        parent: "main".to_owned(),
        pos,
    };
    assert!(
        run_txn_err(
            meta,
            repo,
            txn(vec![], vec![fork("b", 1), journal("LineFork")])
        )
        .await
    );
    assert!(
        run_txn_err(
            meta,
            repo,
            txn(
                vec![],
                vec![fork("d", 1), fork("d", 2), journal("LineFork")]
            )
        )
        .await
    );
    assert_eq!(pos("b", cs(12)).await, Some(2));
    assert_eq!(pos("b", cs(11)).await, Some(1));
    assert_eq!(meta.line_pos(repo, "d", cs(11)).await.expect("read"), None);
}

/// Whether committing `t` fails (as opposed to committing or failing a
/// precondition).
async fn run_txn_err<M: MetaStore>(meta: &M, repo: RepoId, t: MetaTxn) -> bool {
    meta.commit(repo, t).await.is_err()
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

async fn all_workspaces_page_in_name_order<O: ObjectStore, M: MetaStore>(_objects: &O, meta: &M) {
    let repo = new_repo(meta, "all-ws").await;
    let other = new_repo(meta, "all-ws-other").await;
    run_txn(
        meta,
        repo,
        txn(
            vec![],
            vec![
                Write::PutWorkspace(ws("c", None, 1)),
                Write::PutWorkspace(ws("a", None, 2)),
                Write::PutWorkspace(ws("b", None, 3)),
            ],
        ),
    )
    .await;
    run_txn(
        meta,
        other,
        txn(vec![], vec![Write::PutWorkspace(ws("z", None, 4))]),
    )
    .await;

    let first = meta.list_all_workspaces(repo, "", 2).await.expect("page");
    assert_eq!(names(first.clone()), ["a", "b"]);
    let rest = meta.list_all_workspaces(repo, "b", 2).await.expect("page");
    assert_eq!(names(rest), ["c"]);
    assert!(
        meta.list_all_workspaces(repo, "c", 2)
            .await
            .expect("page")
            .is_empty()
    );
}

/// Commits `writes` with no preconditions and asserts that they applied.
async fn apply<M: MetaStore>(meta: &M, repo: RepoId, writes: Vec<Write>) {
    let outcome = run_txn(meta, repo, txn(vec![], writes)).await;
    assert!(matches!(outcome, TxnOutcome::Committed(_)), "{outcome:?}");
}

/// Asserts that `writes` (after a journal entry) are refused as `Invalid`
/// and leave nothing behind.
async fn refused<M: MetaStore>(meta: &M, repo: RepoId, writes: Vec<Write>, why: &str) {
    let before = lsns_of(meta, repo).await;
    let mut all = vec![journal("Before")];
    all.extend(writes);
    let got = meta.commit(repo, txn(vec![], all)).await;
    assert!(matches!(got, Err(StoreError::Invalid(_))), "{why}: {got:?}");
    assert_eq!(lsns_of(meta, repo).await, before, "{why}: nothing written");
}

async fn protection_of<M: MetaStore>(meta: &M, repo: RepoId, id: ChangesetId) -> Protection {
    meta.protection(repo, id)
        .await
        .expect("protection")
        .expect("stored")
}

fn put_record(n: u8, state: RecordState) -> Write {
    Write::PutChangeset {
        id: cs(n),
        content: vec![n],
        record: record(state),
    }
}

fn protection(pins: u32, expires_at_ms: Option<i64>) -> Protection {
    Protection {
        pins,
        expires_at_ms,
    }
}

async fn pins_follow_the_parent_state<O: ObjectStore, M: MetaStore>(_objects: &O, meta: &M) {
    let repo = new_repo(meta, "pins").await;
    assert_eq!(meta.protection(repo, cs(1)).await.expect("read"), None);
    apply(
        meta,
        repo,
        vec![
            put_record(1, RecordState::Draft),
            put_record(2, RecordState::Landed),
            put_record(3, RecordState::Abandoned),
            Write::Pin(cs(1)),
            Write::Pin(cs(1)),
            Write::Pin(cs(2)),
            Write::Pin(cs(3)),
        ],
    )
    .await;
    assert_eq!(protection_of(meta, repo, cs(1)).await, protection(2, None));
    assert_eq!(
        protection_of(meta, repo, cs(2)).await,
        protection(0, None),
        "a landed parent takes no pin"
    );
    assert_eq!(protection_of(meta, repo, cs(3)).await, protection(1, None));

    apply(meta, repo, vec![Write::Unpin(cs(1)), Write::Unpin(cs(2))]).await;
    assert_eq!(protection_of(meta, repo, cs(1)).await.pins, 1);
    assert_eq!(protection_of(meta, repo, cs(2)).await.pins, 0);

    // Landing zeroes the pins and clears the expiry; other transitions keep
    // them.
    apply(
        meta,
        repo,
        vec![
            Write::SetExpiry {
                id: cs(3),
                expires_at_ms: Some(50),
            },
            Write::SetRecord {
                id: cs(3),
                record: record(RecordState::Superseded),
            },
        ],
    )
    .await;
    assert_eq!(
        protection_of(meta, repo, cs(3)).await,
        protection(1, Some(50))
    );
    apply(
        meta,
        repo,
        vec![Write::SetRecord {
            id: cs(3),
            record: record(RecordState::Landed),
        }],
    )
    .await;
    assert_eq!(
        protection_of(meta, repo, cs(3)).await,
        Protection::default()
    );

    // A fresh record has none, and SetPins sets them outright.
    apply(
        meta,
        repo,
        vec![
            put_record(4, RecordState::Draft),
            Write::SetPins { id: cs(4), pins: 7 },
            Write::Pin(cs(4)),
        ],
    )
    .await;
    assert_eq!(protection_of(meta, repo, cs(4)).await.pins, 8);

    // Pins written in the transaction that stores the parent count.
    apply(
        meta,
        repo,
        vec![put_record(5, RecordState::Draft), Write::Pin(cs(5))],
    )
    .await;
    assert_eq!(protection_of(meta, repo, cs(5)).await.pins, 1);
}

async fn protection_writes_are_checked_and_atomic<O: ObjectStore, M: MetaStore>(
    _objects: &O,
    meta: &M,
) {
    let repo = new_repo(meta, "protection-checks").await;
    apply(
        meta,
        repo,
        vec![
            put_record(1, RecordState::Draft),
            put_record(2, RecordState::Landed),
            put_record(3, RecordState::Collected),
            Write::Pin(cs(1)),
        ],
    )
    .await;
    refused(meta, repo, vec![Write::Pin(cs(9))], "pin an absent record").await;
    refused(
        meta,
        repo,
        vec![Write::Pin(cs(3))],
        "pin a collected record",
    )
    .await;
    refused(
        meta,
        repo,
        vec![Write::Unpin(cs(9))],
        "unpin an absent record",
    )
    .await;
    refused(
        meta,
        repo,
        vec![Write::Unpin(cs(3))],
        "unpin a collected record",
    )
    .await;
    refused(
        meta,
        repo,
        vec![Write::Unpin(cs(1)), Write::Pin(cs(9))],
        "a refused write after an unpin",
    )
    .await;
    assert_eq!(
        protection_of(meta, repo, cs(1)).await.pins,
        1,
        "the refused transaction's unpin is not kept"
    );
    apply(meta, repo, vec![Write::Unpin(cs(1)), Write::Unpin(cs(1))]).await;
    assert_eq!(
        protection_of(meta, repo, cs(1)).await.pins,
        0,
        "pins never written stay at 0"
    );
    refused(
        meta,
        repo,
        vec![Write::SetExpiry {
            id: cs(9),
            expires_at_ms: Some(1),
        }],
        "expire an absent record",
    )
    .await;
    refused(
        meta,
        repo,
        vec![Write::SetExpiry {
            id: cs(2),
            expires_at_ms: Some(1),
        }],
        "expire a landed record",
    )
    .await;
    refused(
        meta,
        repo,
        vec![Write::SetPins { id: cs(2), pins: 1 }],
        "pin a landed record outright",
    )
    .await;
    refused(
        meta,
        repo,
        vec![Write::SetPins { id: cs(9), pins: 0 }],
        "set the pins of an absent record",
    )
    .await;
    // Clearing what a landed record does not have is allowed.
    apply(
        meta,
        repo,
        vec![
            Write::SetExpiry {
                id: cs(2),
                expires_at_ms: None,
            },
            Write::SetPins { id: cs(2), pins: 0 },
        ],
    )
    .await;
}

async fn expiry_is_indexed_for_collection<O: ObjectStore, M: MetaStore>(_objects: &O, meta: &M) {
    let repo = new_repo(meta, "expiry").await;
    let other = new_repo(meta, "expiry-other").await;
    let mut writes = Vec::new();
    for (n, at) in [(1, 30), (2, 10), (3, 20), (4, 20), (5, 99), (6, 10)] {
        writes.push(put_record(n, RecordState::Abandoned));
        writes.push(Write::SetExpiry {
            id: cs(n),
            expires_at_ms: Some(at),
        });
    }
    // 6 is pinned by 7, a draft with no expiry.
    writes.push(put_record(7, RecordState::Draft));
    writes.push(Write::Pin(cs(6)));
    apply(meta, repo, writes).await;
    apply(
        meta,
        other,
        vec![
            put_record(8, RecordState::Abandoned),
            Write::SetExpiry {
                id: cs(8),
                expires_at_ms: Some(0),
            },
        ],
    )
    .await;

    let all = meta
        .expired_records(repo, 30, None, 100)
        .await
        .expect("expired");
    assert_eq!(
        all,
        [(10, cs(2)), (20, cs(3)), (20, cs(4)), (30, cs(1))],
        "expires_at <= now, unpinned, in (expiry, id) order, this repository only"
    );
    let first = meta.expired_records(repo, 30, None, 2).await.expect("page");
    assert_eq!(first, [(10, cs(2)), (20, cs(3))]);
    let second = meta
        .expired_records(repo, 30, first.last().copied(), 2)
        .await
        .expect("page");
    assert_eq!(second, [(20, cs(4)), (30, cs(1))]);

    // Unpinning, clearing and landing move records in and out.
    apply(
        meta,
        repo,
        vec![
            Write::Unpin(cs(6)),
            Write::SetExpiry {
                id: cs(3),
                expires_at_ms: None,
            },
            Write::SetRecord {
                id: cs(4),
                record: record(RecordState::Landed),
            },
        ],
    )
    .await;
    let now = meta
        .expired_records(repo, 30, None, 100)
        .await
        .expect("expired");
    assert_eq!(now, [(10, cs(2)), (10, cs(6)), (30, cs(1))]);
}

async fn record_preconditions_compare_protection<O: ObjectStore, M: MetaStore>(
    _objects: &O,
    meta: &M,
) {
    let repo = new_repo(meta, "record-is").await;
    apply(
        meta,
        repo,
        vec![
            put_record(1, RecordState::Abandoned),
            put_record(2, RecordState::Landed),
            put_record(3, RecordState::Collected),
            Write::SetExpiry {
                id: cs(1),
                expires_at_ms: Some(5),
            },
        ],
    )
    .await;
    let check = |pre: Precondition| {
        let t = txn(vec![pre], vec![journal("Checked")]);
        async move {
            matches!(
                meta.commit(repo, t).await.expect("commit"),
                TxnOutcome::Committed(_)
            )
        }
    };
    assert!(check(Precondition::RecordLive(cs(1))).await);
    assert!(check(Precondition::RecordLive(cs(2))).await);
    assert!(!check(Precondition::RecordLive(cs(3))).await, "collected");
    assert!(!check(Precondition::RecordLive(cs(9))).await, "absent");

    let is = |n: u8, state: RecordState, p: Protection| Precondition::RecordIs {
        id: cs(n),
        record: record(state),
        protection: p,
    };
    // The precondition's own journal entries do not touch record 1 (its
    // record keeps lsn 0), so it can be checked repeatedly.
    assert!(check(is(1, RecordState::Abandoned, protection(0, Some(5)))).await);
    assert!(!check(is(1, RecordState::Abandoned, protection(0, Some(6)))).await);
    assert!(!check(is(1, RecordState::Abandoned, protection(1, Some(5)))).await);
    assert!(!check(is(1, RecordState::Superseded, protection(0, Some(5)))).await);
    assert!(!check(is(9, RecordState::Abandoned, Protection::default())).await);
    apply(
        meta,
        repo,
        vec![put_record(4, RecordState::Draft), Write::Pin(cs(1))],
    )
    .await;
    assert!(
        !check(is(1, RecordState::Abandoned, protection(0, Some(5)))).await,
        "a new pin fails the compare"
    );
    assert!(check(is(1, RecordState::Abandoned, protection(1, Some(5)))).await);
}

async fn rows_read_by_holder_and_by_expiry<O: ObjectStore, M: MetaStore>(_objects: &O, meta: &M) {
    let repo = new_repo(meta, "rows").await;
    let other = new_repo(meta, "rows-other").await;
    let add = |object: ObjectId, holder: &str| Write::AddHolder {
        object,
        holder: holder.to_owned(),
    };
    let until = |object: ObjectId, holder: &str, at: i64| Write::AddHolderUntil {
        object,
        holder: holder.to_owned(),
        expires_at_ms: at,
    };
    apply(
        meta,
        repo,
        vec![
            add(oid(3), "c1"),
            add(oid(1), "c1"),
            add(oid(2), "c2"),
            add(oid(2), "c1"),
            until(oid(4), "upload:40:u", 40),
            until(oid(1), "claim:20:r", 20),
            until(oid(5), "upload:20:u", 20),
            // The first add of a row decides its expiry.
            until(oid(4), "upload:40:u", 10),
        ],
    )
    .await;
    apply(meta, other, vec![add(oid(1), "c1"), until(oid(1), "x", 0)]).await;

    let held = meta
        .held_objects(repo, "c1", None, 100)
        .await
        .expect("held");
    assert_eq!(held, [oid(1), oid(2), oid(3)]);
    let page = meta
        .held_objects(repo, "c1", Some(oid(1)), 1)
        .await
        .expect("page");
    assert_eq!(page, [oid(2)]);
    assert!(
        meta.held_objects(repo, "nobody", None, 100)
            .await
            .expect("held")
            .is_empty()
    );
    assert_eq!(
        meta.holders(repo, oid(1)).await.expect("count"),
        2,
        "a timed row is a row"
    );

    let row = |at: i64, object: ObjectId, holder: &str| TimedRow {
        expires_at_ms: at,
        object,
        holder: holder.to_owned(),
    };
    let expired = meta.expired_rows(repo, 40, None, 100).await.expect("rows");
    assert_eq!(
        expired,
        [
            row(20, oid(1), "claim:20:r"),
            row(20, oid(5), "upload:20:u"),
            row(40, oid(4), "upload:40:u"),
        ]
    );
    let first = meta.expired_rows(repo, 39, None, 1).await.expect("page");
    assert_eq!(first, [row(20, oid(1), "claim:20:r")]);
    let second = meta
        .expired_rows(repo, 39, first.last().cloned(), 5)
        .await
        .expect("page");
    assert_eq!(second, [row(20, oid(5), "upload:20:u")]);

    apply(
        meta,
        repo,
        vec![Write::RemoveHolder {
            object: oid(1),
            holder: "claim:20:r".to_owned(),
        }],
    )
    .await;
    assert_eq!(
        meta.expired_rows(repo, 40, None, 100)
            .await
            .expect("rows")
            .len(),
        2
    );
    // A removed timed row added again without an expiry has none.
    apply(meta, repo, vec![add(oid(1), "claim:20:r")]).await;
    assert_eq!(
        meta.expired_rows(repo, 40, None, 100)
            .await
            .expect("rows")
            .len(),
        2
    );
}

async fn doomed_marks_claims_and_deletes<O: ObjectStore, M: MetaStore>(_objects: &O, meta: &M) {
    let repo = new_repo(meta, "doomed").await;
    let (a, b, c) = (oid(1), oid(2), oid(3));
    let doom = |object: ObjectId, since_ms: i64| Write::Doom { object, since_ms };
    let row = |object: ObjectId, holder: &str| Write::AddHolder {
        object,
        holder: holder.to_owned(),
    };
    let mark = |since_ms: i64, deleting: bool| DoomMark { since_ms, deleting };
    let marks =
        |ids: Vec<ObjectId>| async move { meta.doom_marks(repo, &ids).await.expect("marks") };
    let holds = |pre: Vec<Precondition>| {
        let t = txn(pre, vec![journal("Checked")]);
        async move {
            matches!(
                meta.commit(repo, t).await.expect("commit"),
                TxnOutcome::Committed(_)
            )
        }
    };

    apply(meta, repo, vec![doom(a, 10), doom(b, 20), row(c, "h")]).await;
    assert_eq!(
        marks(vec![c, b, a]).await,
        [(b, mark(20, false)), (a, mark(10, false))],
        "marks in the order asked, unmarked ids left out"
    );
    refused(meta, repo, vec![doom(c, 5)], "doom a held object").await;
    refused(
        meta,
        repo,
        vec![Write::MarkDeleting(c)],
        "mark an undoomed object deleting",
    )
    .await;
    // Doomed again: the first time stays.
    apply(meta, repo, vec![doom(a, 99)]).await;
    assert_eq!(marks(vec![a]).await, [(a, mark(10, false))]);
    // A row removed and the object doomed in one transaction.
    apply(
        meta,
        repo,
        vec![
            Write::RemoveHolder {
                object: c,
                holder: "h".to_owned(),
            },
            doom(c, 30),
        ],
    )
    .await;
    assert_eq!(marks(vec![c]).await, [(c, mark(30, false))]);

    assert!(
        holds(vec![
            Precondition::ObjectDoomed(a),
            Precondition::ObjectUnheld(a)
        ])
        .await
    );
    assert!(!holds(vec![Precondition::ObjectDoomed(oid(9))]).await);
    assert!(holds(vec![Precondition::ObjectUnheld(oid(9))]).await);

    // A claim: the doomed mark goes and a row holds the object.
    apply(
        meta,
        repo,
        vec![Write::AddHolderUntil {
            object: a,
            holder: "claim:50:r".to_owned(),
            expires_at_ms: 50,
        }],
    )
    .await;
    assert!(marks(vec![a]).await.is_empty(), "a row revives the object");
    assert!(!holds(vec![Precondition::ObjectUnheld(a)]).await);
    assert!(!holds(vec![Precondition::ObjectDoomed(a)]).await);

    // A delete: deleting, then the mark goes after the frame.
    apply(meta, repo, vec![Write::MarkDeleting(b)]).await;
    assert_eq!(marks(vec![b]).await, [(b, mark(20, true))]);
    assert!(
        !holds(vec![Precondition::ObjectDoomed(b)]).await,
        "a claim cannot take an object being deleted"
    );
    refused(meta, repo, vec![doom(b, 1)], "doom an object being deleted").await;
    apply(meta, repo, vec![Write::Undoom(b), Write::Undoom(oid(9))]).await;
    assert!(marks(vec![b]).await.is_empty());

    // A row on an object being deleted leaves the mark: the writer should
    // have waited for the delete.
    apply(meta, repo, vec![Write::MarkDeleting(c), row(c, "late")]).await;
    assert_eq!(marks(vec![c]).await, [(c, mark(30, true))]);
}

async fn old_doomed_page_in_time_order<O: ObjectStore, M: MetaStore>(_objects: &O, meta: &M) {
    let repo = new_repo(meta, "old-doomed").await;
    let other = new_repo(meta, "old-doomed-other").await;
    let mut writes = Vec::new();
    for (n, since_ms) in [(5, 10), (1, 30), (3, 10), (2, 20), (4, 50)] {
        writes.push(Write::Doom {
            object: oid(n),
            since_ms,
        });
    }
    apply(meta, repo, writes).await;
    apply(
        meta,
        other,
        vec![Write::Doom {
            object: oid(7),
            since_ms: 0,
        }],
    )
    .await;
    let ids =
        |rows: Vec<(ObjectId, DoomMark)>| rows.into_iter().map(|(o, _)| o).collect::<Vec<_>>();
    let all = meta.old_doomed(repo, 30, None, 100).await.expect("doomed");
    assert_eq!(ids(all), [oid(3), oid(5), oid(2), oid(1)]);
    let first = meta.old_doomed(repo, 30, None, 2).await.expect("page");
    let last = first.last().map(|(o, m)| (m.since_ms, *o));
    assert_eq!(ids(first), [oid(3), oid(5)]);
    let second = meta.old_doomed(repo, 30, last, 2).await.expect("page");
    assert_eq!(ids(second), [oid(2), oid(1)]);
}

async fn lapsed_workspaces_page_in_lease_order<O: ObjectStore, M: MetaStore>(
    _objects: &O,
    meta: &M,
) {
    let repo = new_repo(meta, "lapsed").await;
    let mut writes = Vec::new();
    for (name, lease) in [("d", 10), ("a", 30), ("c", 10), ("b", 20), ("e", 40)] {
        writes.push(Write::PutWorkspace(ws(name, None, lease)));
    }
    apply(meta, repo, writes).await;
    let first = meta
        .lapsed_workspaces(repo, 40, None, 2)
        .await
        .expect("lapsed");
    assert_eq!(names(first.clone()), ["c", "d"]);
    let last = first.last().map(|w| (w.lease_until_ms, w.name.clone()));
    let second = meta
        .lapsed_workspaces(repo, 40, last, 5)
        .await
        .expect("lapsed");
    assert_eq!(
        names(second),
        ["b", "a"],
        "leases end before, not at, the time"
    );
}

async fn gc_protection_marker<O: ObjectStore, M: MetaStore>(_objects: &O, meta: &M) {
    let repo = new_repo(meta, "marker").await;
    assert!(
        meta.gc_protection(repo).await.expect("marker"),
        "a new repository is protected from its first write"
    );
    apply(meta, repo, vec![Write::SetGcProtection(false)]).await;
    assert!(!meta.gc_protection(repo).await.expect("marker"));
    apply(meta, repo, vec![Write::SetGcProtection(true)]).await;
    assert!(meta.gc_protection(repo).await.expect("marker"));
}

async fn derived_value_cas<O: ObjectStore, M: MetaStore>(_objects: &O, meta: &M) {
    let repo = new_repo(meta, "derived-cas").await;
    let is = |value: Option<&[u8]>| Precondition::DerivedIs {
        kind: "lease".to_owned(),
        version: 1,
        key: b"k".to_vec(),
        value: value.map(<[u8]>::to_vec),
    };
    let put = |value: &[u8]| Write::PutDerived {
        kind: "lease".to_owned(),
        version: 1,
        key: b"k".to_vec(),
        value: value.to_vec(),
    };
    assert_eq!(
        run_txn(meta, repo, txn(vec![is(Some(b"a"))], vec![put(b"b")])).await,
        TxnOutcome::PreconditionFailed(0),
        "an absent value is not a present one"
    );
    assert!(matches!(
        run_txn(meta, repo, txn(vec![is(None)], vec![put(b"a")])).await,
        TxnOutcome::Committed(_)
    ));
    assert_eq!(
        run_txn(meta, repo, txn(vec![is(None)], vec![put(b"c")])).await,
        TxnOutcome::PreconditionFailed(0),
        "a second taker loses"
    );
    assert!(matches!(
        run_txn(meta, repo, txn(vec![is(Some(b"a"))], vec![put(b"b")])).await,
        TxnOutcome::Committed(_)
    ));
    assert_eq!(
        meta.derived(repo, "lease", 1, b"k").await.expect("read"),
        Some(b"b".to_vec())
    );
}

async fn a_collected_changeset_is_replaced<O: ObjectStore, M: MetaStore>(_objects: &O, meta: &M) {
    let repo = new_repo(meta, "replace").await;
    apply(
        meta,
        repo,
        vec![Write::PutChangeset {
            id: cs(1),
            content: b"content".to_vec(),
            record: record(RecordState::Abandoned),
        }],
    )
    .await;
    // The sweep's tombstone: no content.
    apply(
        meta,
        repo,
        vec![Write::PutChangeset {
            id: cs(1),
            content: Vec::new(),
            record: record(RecordState::Collected),
        }],
    )
    .await;
    let (content, rec) = meta
        .changeset(repo, cs(1))
        .await
        .expect("read")
        .expect("stored");
    assert!(content.is_empty());
    assert_eq!(rec.state, RecordState::Collected);
    // A revival, guarded by the tombstone.
    let revive = txn(
        vec![Precondition::RecordStateIs {
            id: cs(1),
            state: RecordState::Collected,
        }],
        vec![
            Write::PutChangeset {
                id: cs(1),
                content: b"content".to_vec(),
                record: record(RecordState::Draft),
            },
            Write::SetExpiry {
                id: cs(1),
                expires_at_ms: Some(5),
            },
        ],
    );
    assert!(matches!(
        run_txn(meta, repo, revive).await,
        TxnOutcome::Committed(_)
    ));
    let (content, rec) = meta
        .changeset(repo, cs(1))
        .await
        .expect("read")
        .expect("stored");
    assert_eq!(content, b"content");
    assert_eq!(rec.state, RecordState::Draft);
    let p = meta
        .protection(repo, cs(1))
        .await
        .expect("read")
        .expect("protection");
    assert_eq!((p.pins, p.expires_at_ms), (0, Some(5)));
}

async fn draft_path_index<O: ObjectStore, M: MetaStore>(_objects: &O, meta: &M) {
    let repo = new_repo(meta, "draft-paths").await;
    let p = |s: &str| RepoPath::new(s).expect("valid path");
    let set = |ws: &str, line: &str, paths: &[&str]| Write::SetDraftPaths {
        workspace: ws.into(),
        line: line.into(),
        paths: paths.iter().map(|s| p(s)).collect(),
    };
    let want = |v: &[(&str, &str)]| {
        v.iter()
            .map(|(a, b)| (p(a), (*b).to_owned()))
            .collect::<Vec<_>>()
    };
    apply(
        meta,
        repo,
        vec![
            // Siblings that sort inside [src, src0) but are not beneath it.
            set("a", "main", &["src", "src/x.rs", "src-y", "src.z", "src0"]),
            set("b", "main", &["src/x.rs", "doc/z"]),
            set("c", "dev", &["src/x.rs"]),
        ],
    )
    .await;
    let src = p("src");
    let scan = |after, limit| meta.scan_draft_paths(repo, "main", &src, after, limit);
    let all = want(&[("src", "a"), ("src/x.rs", "a"), ("src/x.rs", "b")]);
    assert_eq!(scan(None, 10).await.expect("scan"), all);
    // Every row scanned is a hit: a page of n is the first n hits.
    assert_eq!(scan(None, 2).await.expect("scan"), all[..2]);
    assert_eq!(
        scan(Some((p("src"), "a".into())), 1).await.expect("scan"),
        all[1..2]
    );
    assert_eq!(
        scan(Some((p("src/x.rs"), "a".into())), 10)
            .await
            .expect("scan"),
        all[2..]
    );
    assert_eq!(
        meta.scan_draft_paths(repo, "main", &p("src/x.rs"), None, 10)
            .await
            .expect("scan"),
        want(&[("src/x.rs", "a"), ("src/x.rs", "b")])
    );
    assert_eq!(
        meta.scan_draft_paths(repo, "dev", &src, None, 10)
            .await
            .expect("scan"),
        want(&[("src/x.rs", "c")])
    );
    // Add is a set union.
    apply(
        meta,
        repo,
        vec![Write::AddDraftPaths {
            workspace: "b".into(),
            line: "main".into(),
            paths: vec![p("src/x.rs"), p("src/w.rs")],
        }],
    )
    .await;
    assert_eq!(
        meta.workspace_draft_paths(repo, "b", None, 10)
            .await
            .expect("paths"),
        vec![p("doc/z"), p("src/w.rs"), p("src/x.rs")]
    );
    assert_eq!(
        meta.workspace_draft_paths(repo, "b", Some(p("doc/z")), 1)
            .await
            .expect("paths"),
        vec![p("src/w.rs")]
    );
    // Replace, clear, and delete with the workspace.
    apply(
        meta,
        repo,
        vec![
            set("a", "main", &["doc/z"]),
            set("c", "dev", &[]),
            Write::DeleteWorkspace("b".into()),
        ],
    )
    .await;
    assert!(scan(None, 10).await.expect("scan").is_empty());
    assert_eq!(
        meta.workspace_draft_paths(repo, "a", None, 10)
            .await
            .expect("paths"),
        vec![p("doc/z")]
    );
    for gone in ["b", "c"] {
        assert!(
            meta.workspace_draft_paths(repo, gone, None, 10)
                .await
                .expect("paths")
                .is_empty()
        );
    }
}

async fn workspace_work_item_round_trips<O: ObjectStore, M: MetaStore>(_objects: &O, meta: &M) {
    let repo = new_repo(meta, "work-item").await;
    let record = WorkspaceRecord {
        work_item: Some("WI-1".to_owned()),
        ..ws("w", None, 10)
    };
    apply(meta, repo, vec![Write::PutWorkspace(record.clone())]).await;
    assert_eq!(meta.workspace(repo, "w").await.expect("read"), Some(record));
}
