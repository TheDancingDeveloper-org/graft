//! The `mirror_requests` table and line slots (D41 §3.5): writes are CAS
//! only, a row reads back exactly, and pages are bounded. Every test runs
//! on SQLite and on the in-memory fake, which must agree.

mod common;

use common::{TempDir, block_on};
use graft_model::{ChangesetId, ObjectId};
use graft_store::memory::MemoryStore;
use graft_store::{
    BatchState, LineSlot, MetaStore, MetaTxn, MirrorRequestKind, MirrorRequestQuery,
    MirrorRequestRecord, MirrorRequestResult, MirrorRequestState, Precondition, StoreError,
    TxnOutcome, Write,
};
use graft_store_sqlite::{SqliteConfig, SqliteMetaStore};

fn request(id: u64, key: &str, version: u64) -> MirrorRequestRecord {
    MirrorRequestRecord {
        id,
        line: "main".into(),
        kind: MirrorRequestKind::CutBatch,
        client_request_id: key.into(),
        base: Some(ChangesetId(ObjectId([1; 32]))),
        excludes: vec![
            ChangesetId(ObjectId([2; 32])),
            ChangesetId(ObjectId([3; 32])),
        ],
        upstream: None,
        state: MirrorRequestState::Claimed,
        batch: Some(BatchState::Open),
        claimed_by_epoch: Some(4),
        claimed_at_ms: Some(5),
        deadline_ms: 6,
        result: MirrorRequestResult {
            pr_number: Some(7),
            pr_url: Some("https://github.com/o/r/pull/7".into()),
            branch: Some(format!("graft/batch-{id}")),
            lands: vec![ChangesetId(ObjectId([8; 32]))],
            error_code: Some("timeout".into()),
        },
        requested_by: "op".into(),
        created_at_ms: 9,
        updated_at_ms: 10,
        version,
    }
}

fn create(r: MirrorRequestRecord, slot: bool) -> MetaTxn {
    let mut txn = MetaTxn {
        preconditions: vec![
            Precondition::MirrorRequestKeyIs {
                kind: r.kind,
                client_request_id: r.client_request_id.clone(),
                id: None,
            },
            Precondition::MirrorRequestVersionIs {
                id: r.id,
                version: None,
            },
        ],
        writes: Vec::new(),
    };
    if slot {
        txn.preconditions.push(Precondition::LineSlotIs {
            line: r.line.clone(),
            slot: LineSlot::Batch,
            request: None,
        });
        txn.writes.push(Write::SetLineSlot {
            line: r.line.clone(),
            slot: LineSlot::Batch,
            request: Some(r.id),
        });
    }
    txn.writes.push(Write::PutMirrorRequest(r));
    txn
}

async fn scenario(s: &impl MetaStore) {
    let repo = s.create_repo("r").await.expect("repo");
    assert_eq!(s.last_mirror_request_id(repo).await.expect("last"), 0);
    let one = request(1, "k1", 1);
    assert!(matches!(
        s.commit(repo, create(one.clone(), true)).await,
        Ok(TxnOutcome::Committed(_))
    ));
    // The row reads back exactly, by id and by key.
    assert_eq!(
        s.get_mirror_request(repo, 1).await.expect("get"),
        Some(one.clone())
    );
    assert_eq!(
        s.find_mirror_request(repo, MirrorRequestKind::CutBatch, "k1")
            .await
            .expect("find"),
        Some(one.clone())
    );
    assert_eq!(
        s.find_mirror_request(repo, MirrorRequestKind::Rewind, "k1")
            .await
            .expect("find"),
        None
    );
    assert_eq!(
        s.line_slot(repo, "main", LineSlot::Batch)
            .await
            .expect("slot"),
        Some(1)
    );
    // A second create under the same key, or onto the held slot, fails its
    // precondition and writes nothing.
    assert!(matches!(
        s.commit(repo, create(request(2, "k1", 1), false)).await,
        Ok(TxnOutcome::PreconditionFailed(0))
    ));
    assert!(matches!(
        s.commit(repo, create(request(2, "k2", 1), true)).await,
        Ok(TxnOutcome::PreconditionFailed(2))
    ));
    assert_eq!(s.last_mirror_request_id(repo).await.expect("last"), 1);
    // Unguarded or out-of-order writes are refused.
    let bare = MetaTxn {
        preconditions: Vec::new(),
        writes: vec![Write::PutMirrorRequest(request(1, "k1", 2))],
    };
    assert!(matches!(
        s.commit(repo, bare).await,
        Err(StoreError::Invalid(_))
    ));
    let skip = MetaTxn {
        preconditions: vec![Precondition::MirrorRequestVersionIs {
            id: 1,
            version: Some(1),
        }],
        writes: vec![Write::PutMirrorRequest(request(1, "k1", 3))],
    };
    assert!(matches!(
        s.commit(repo, skip).await,
        Err(StoreError::Invalid(_))
    ));
    let rekey = MetaTxn {
        preconditions: vec![Precondition::MirrorRequestVersionIs {
            id: 1,
            version: Some(1),
        }],
        writes: vec![Write::PutMirrorRequest(request(1, "other", 2))],
    };
    assert!(matches!(
        s.commit(repo, rekey).await,
        Err(StoreError::Invalid(_))
    ));
    let slot = MetaTxn {
        preconditions: Vec::new(),
        writes: vec![Write::SetLineSlot {
            line: "main".into(),
            slot: LineSlot::Batch,
            request: None,
        }],
    };
    assert!(matches!(
        s.commit(repo, slot).await,
        Err(StoreError::Invalid(_))
    ));
    // A CAS update moves the row and clears the slot together.
    let mut next = request(1, "k1", 2);
    next.state = MirrorRequestState::Done;
    next.batch = Some(BatchState::Settled);
    let settle = MetaTxn {
        preconditions: vec![
            Precondition::MirrorRequestVersionIs {
                id: 1,
                version: Some(1),
            },
            Precondition::LineSlotIs {
                line: "main".into(),
                slot: LineSlot::Batch,
                request: Some(1),
            },
        ],
        writes: vec![
            Write::PutMirrorRequest(next.clone()),
            Write::SetLineSlot {
                line: "main".into(),
                slot: LineSlot::Batch,
                request: None,
            },
        ],
    };
    assert!(matches!(
        s.commit(repo, settle).await,
        Ok(TxnOutcome::Committed(_))
    ));
    assert_eq!(
        s.get_mirror_request(repo, 1).await.expect("get"),
        Some(next)
    );
    assert_eq!(
        s.line_slot(repo, "main", LineSlot::Batch)
            .await
            .expect("slot"),
        None
    );
    // Pages: filtered by state, both orders, exclusive cursors.
    for id in 2..=5 {
        let mut r = request(id, &format!("k{id}"), 1);
        r.state = MirrorRequestState::Pending;
        assert!(matches!(
            s.commit(repo, create(r, false)).await,
            Ok(TxnOutcome::Committed(_))
        ));
    }
    let page = |q: MirrorRequestQuery| {
        let s = &s;
        async move {
            s.list_mirror_requests(repo, q)
                .await
                .expect("list")
                .into_iter()
                .map(|r| r.id)
                .collect::<Vec<_>>()
        }
    };
    let pending = vec![MirrorRequestState::Pending];
    assert_eq!(
        page(MirrorRequestQuery {
            states: pending.clone(),
            oldest_first: true,
            limit: 2,
            ..Default::default()
        })
        .await,
        vec![2, 3]
    );
    assert_eq!(
        page(MirrorRequestQuery {
            states: pending.clone(),
            oldest_first: true,
            after: Some(3),
            limit: 2,
            ..Default::default()
        })
        .await,
        vec![4, 5]
    );
    assert_eq!(
        page(MirrorRequestQuery {
            kind: Some(MirrorRequestKind::CutBatch),
            line: Some("main".into()),
            after: Some(5),
            limit: 2,
            ..Default::default()
        })
        .await,
        vec![4, 3]
    );
    assert_eq!(s.last_mirror_request_id(repo).await.expect("last"), 5);
}

#[test]
fn requests_are_cas_only_and_read_back_on_both_stores() {
    block_on(scenario(&MemoryStore::new()));
    let dir = TempDir::new("mirror-requests");
    let store = SqliteMetaStore::open(SqliteConfig::new(dir.db())).expect("open");
    block_on(scenario(&store));
}
