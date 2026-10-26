//! The v0.2 review indexes in the in-memory store: attestations by
//! changeset and by line position, land-checks rows, and the review queue.

#![cfg(feature = "testkit")]

use std::future::Future;
use std::pin::pin;
use std::task::{Context, Poll, Waker};

use graft_model::{ChangesetId, ObjectId};
use graft_store::memory::MemoryStore;
use graft_store::{
    AttestKind, AttestStatus, LandCheckState, MetaStore, MetaTxn, Precondition, RepoId, StoreError,
    TxnOutcome, Write,
};

/// Drives a future to completion; the memory store's futures are ready on
/// the first poll.
fn block_on<F: Future>(future: F) -> F::Output {
    let mut future = pin!(future);
    let mut cx = Context::from_waker(Waker::noop());
    loop {
        if let Poll::Ready(output) = future.as_mut().poll(&mut cx) {
            return output;
        }
    }
}

fn cs(n: u32) -> ChangesetId {
    let mut bytes = [0u8; 32];
    bytes[..4].copy_from_slice(&n.to_be_bytes());
    bytes[31] = 1;
    ChangesetId(ObjectId(bytes))
}

fn journal(kind: &str) -> Write {
    Write::Journal {
        kind: kind.to_owned(),
        body: Vec::new(),
    }
}

fn setup() -> (MemoryStore, RepoId) {
    let store = MemoryStore::new();
    let repo = block_on(store.create_repo("r")).unwrap();
    (store, repo)
}

fn commit(store: &MemoryStore, repo: RepoId, writes: Vec<Write>) -> TxnOutcome {
    block_on(store.commit(
        repo,
        MetaTxn {
            preconditions: Vec::new(),
            writes,
        },
    ))
    .unwrap()
}

/// The land write of changeset `n` at position `n` on `main`.
fn land(n: u32, checked: bool) -> Write {
    Write::PutLand {
        draft: cs(n + 1_000_000),
        line: "main".to_owned(),
        landed: cs(n),
        pos: Some(u64::from(n)),
        checked,
    }
}

fn attest(n: u32, kind: AttestKind, name: &str) -> Write {
    Write::PutAttestation {
        changeset: cs(n),
        line_pos: Some(("main".to_owned(), u64::from(n))),
        kind,
        name: name.to_owned(),
        status: AttestStatus::Pass,
        by: Some("ci".to_owned()),
        url: String::new(),
    }
}

/// Accept 1: a checked land and its pending row commit or fail together.
#[test]
fn checked_land_and_its_pending_row_commit_together() {
    let (store, repo) = setup();
    let outcome = commit(&store, repo, vec![land(1, true), journal("RefMoved")]);
    assert_eq!(outcome, TxnOutcome::Committed(vec![1]));
    let rows = block_on(store.land_checks(repo, "main", 0, 10)).unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!((rows[0].pos, rows[0].changeset), (1, cs(1)));
    assert_eq!(rows[0].state, LandCheckState::Pending);

    // Injected failure: a pending row already sits at position 2, so the
    // second write of the land transaction fails. Nothing of it is kept:
    // no land record, no journal entry, no landing in the review queue.
    let blocker = Write::PutLand {
        draft: cs(77),
        line: "main".to_owned(),
        landed: cs(78),
        pos: Some(2),
        checked: true,
    };
    commit(&store, repo, vec![blocker, journal("RefMoved")]);
    let err = block_on(store.commit(
        repo,
        MetaTxn {
            preconditions: Vec::new(),
            writes: vec![land(2, true), journal("RefMoved")],
        },
    ))
    .unwrap_err();
    assert!(matches!(err, StoreError::Invalid(_)));
    assert_eq!(
        block_on(store.land(repo, cs(1_000_002), "main")).unwrap(),
        None
    );
    assert_eq!(block_on(store.journal(repo, 1, 10)).unwrap().len(), 2);
    let rows = block_on(store.land_checks(repo, "main", 2, 10)).unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].changeset, cs(78));
    let queue = block_on(store.review_queue(repo, "main", 0, 10)).unwrap();
    assert!(queue.iter().all(|l| l.changeset != cs(2)));

    // A failed precondition writes neither the land nor its row.
    let outcome = block_on(store.commit(
        repo,
        MetaTxn {
            preconditions: vec![Precondition::ChangesetAbsent(cs(1_000_003))],
            writes: vec![land(3, true), journal("RefMoved")],
        },
    ))
    .unwrap();
    assert_eq!(outcome, TxnOutcome::Committed(vec![3]));
    let outcome = block_on(store.commit(
        repo,
        MetaTxn {
            preconditions: vec![Precondition::RefIs {
                name: "main".to_owned(),
                expected: Some((cs(9), 9)),
            }],
            writes: vec![land(4, true), journal("RefMoved")],
        },
    ))
    .unwrap();
    assert_eq!(outcome, TxnOutcome::PreconditionFailed(0));
    assert!(
        block_on(store.land_checks(repo, "main", 4, 10))
            .unwrap()
            .is_empty()
    );
    assert_eq!(
        block_on(store.land(repo, cs(1_000_004), "main")).unwrap(),
        None
    );
}

/// Accept 4: an unchecked line gets no land-checks row.
#[test]
fn unchecked_land_gets_no_land_checks_row() {
    let (store, repo) = setup();
    commit(&store, repo, vec![land(1, false), journal("RefMoved")]);
    assert_eq!(
        block_on(store.land(repo, cs(1_000_001), "main")).unwrap(),
        Some(cs(1))
    );
    assert!(
        block_on(store.land_checks(repo, "main", 0, 10))
            .unwrap()
            .is_empty()
    );
}

#[test]
fn attestation_and_land_check_update_share_the_attested_transaction() {
    let (store, repo) = setup();
    commit(&store, repo, vec![land(1, true), journal("RefMoved")]);
    let outcome = commit(
        &store,
        repo,
        vec![
            journal("Attested"),
            attest(1, AttestKind::Check, "ci/test"),
            Write::SetLandCheck {
                line: "main".to_owned(),
                pos: 1,
                state: LandCheckState::Good,
                revert: None,
            },
        ],
    );
    assert_eq!(outcome, TxnOutcome::Committed(vec![2]));
    let rows = block_on(store.attestations_by_changeset(repo, cs(1), None, 10)).unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!((rows[0].lsn, rows[0].name.as_str()), (2, "ci/test"));
    let checks = block_on(store.land_checks(repo, "main", 1, 10)).unwrap();
    assert_eq!(checks[0].state, LandCheckState::Good);

    // Without its journal entry an attestation is refused, and an update
    // of an absent row too.
    let lone = vec![attest(1, AttestKind::Check, "ci/lint")];
    assert!(
        block_on(store.commit(
            repo,
            MetaTxn {
                preconditions: vec![],
                writes: lone
            }
        ))
        .is_err()
    );
    let absent = vec![
        journal("Attested"),
        Write::SetLandCheck {
            line: "main".to_owned(),
            pos: 9,
            state: LandCheckState::Bad,
            revert: None,
        },
    ];
    assert!(
        block_on(store.commit(
            repo,
            MetaTxn {
                preconditions: vec![],
                writes: absent
            }
        ))
        .is_err()
    );
    assert_eq!(block_on(store.journal(repo, 1, 10)).unwrap().len(), 2);
}

/// Accept 2: range reads return exactly the rows in range, in position
/// order, across three pages.
#[test]
fn range_reads_page_in_position_order() {
    let (store, repo) = setup();
    // Positions 1..=12 landed and attested, in a scrambled order.
    let order = [7, 3, 12, 1, 9, 5, 11, 2, 8, 4, 10, 6];
    for n in order {
        commit(&store, repo, vec![land(n, true), journal("RefMoved")]);
        commit(
            &store,
            repo,
            vec![journal("Attested"), attest(n, AttestKind::Check, "ci")],
        );
    }
    // Another line's rows must not leak into the range.
    commit(
        &store,
        repo,
        vec![
            Write::PutLand {
                draft: cs(500),
                line: "other".to_owned(),
                landed: cs(501),
                pos: Some(4),
                checked: true,
            },
            journal("RefMoved"),
        ],
    );

    // Attestations at positions 3..=9, pages of 3: 3 pages (3, 3, 1).
    let mut seen = Vec::new();
    let mut after = None;
    let mut pages = 0;
    loop {
        let page = block_on(store.attestations_by_line(repo, "main", 3, 9, after, 3)).unwrap();
        if page.is_empty() {
            break;
        }
        pages += 1;
        let last = page.last().unwrap();
        after = Some((last.line_pos.as_ref().unwrap().1, last.lsn, last.seq));
        seen.extend(page.iter().map(|r| r.line_pos.as_ref().unwrap().1));
    }
    assert_eq!(pages, 3);
    assert_eq!(seen, vec![3, 4, 5, 6, 7, 8, 9]);

    // Land-checks rows from position 2, pages of 4: 3 pages (4, 4, 3).
    let mut seen = Vec::new();
    let mut from = 2;
    let mut pages = 0;
    loop {
        let page = block_on(store.land_checks(repo, "main", from, 4)).unwrap();
        if page.is_empty() {
            break;
        }
        pages += 1;
        from = page.last().unwrap().pos + 1;
        seen.extend(page.iter().map(|r| (r.pos, r.changeset)));
    }
    assert_eq!(pages, 3);
    assert_eq!(
        seen,
        (2..=12).map(|n| (u64::from(n), cs(n))).collect::<Vec<_>>()
    );
}

/// Accept 3: the review queue's cost follows the page, not the line.
#[test]
fn review_queue_cost_is_bounded_by_the_page() {
    let (store, repo) = setup();
    for n in 1..=200 {
        commit(&store, repo, vec![land(n, false), journal("RefMoved")]);
    }
    // Review all but the last ten landings.
    for n in 1..=190 {
        commit(
            &store,
            repo,
            vec![journal("Attested"), attest(n, AttestKind::Review, "review")],
        );
    }
    // A check attestation does not take a landing out of the queue.
    commit(
        &store,
        repo,
        vec![journal("Attested"), attest(195, AttestKind::Check, "ci")],
    );

    let before = store.rows_visited();
    let page = block_on(store.review_queue(repo, "main", 0, 4)).unwrap();
    assert_eq!(store.rows_visited() - before, 4);
    let got: Vec<ChangesetId> = page.iter().map(|l| l.changeset).collect();
    assert_eq!(got, (191..=194).map(cs).collect::<Vec<_>>());
    assert_eq!(page[0].pos, 191);

    let since = page.last().unwrap().lsn;
    let before = store.rows_visited();
    let rest = block_on(store.review_queue(repo, "main", since, 100)).unwrap();
    assert_eq!(store.rows_visited() - before, 6);
    assert_eq!(rest.len(), 6);
    assert!(rest.iter().all(|l| l.lsn > since));
}
