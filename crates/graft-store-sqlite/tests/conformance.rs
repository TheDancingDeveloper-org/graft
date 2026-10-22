//! Runs the port conformance suite against the SQLite store, plus the
//! store's own behaviour: backpressure and reopening.

mod common;

use std::cell::RefCell;
use std::task::{Context, Poll, Waker};
use std::time::{Duration, Instant};

use common::{Objects, TempDir, block_on};
use graft_model::{ChangesetId, ObjectId};
use graft_store::{
    MetaStore, MetaTxn, Precondition, RefKind, RefRecord, StoreError, TxnOutcome, Write,
};
use graft_store_sqlite::{SqliteConfig, SqliteMetaStore};
use rusqlite::Connection;

fn open(dir: &TempDir) -> SqliteMetaStore {
    SqliteMetaStore::open(SqliteConfig::new(dir.db())).expect("open store")
}

#[test]
fn sqlite_store_passes_the_conformance_suite() {
    let dirs = RefCell::new(Vec::new());
    block_on(graft_store::conformance::run_all(|| {
        let dir = TempDir::new("conformance");
        let store = open(&dir);
        dirs.borrow_mut().push(dir);
        (Objects::default(), store)
    }));
}

fn cas(name: &str, expected: Option<(ChangesetId, u64)>, target: ChangesetId) -> MetaTxn {
    let version = expected.map_or(1, |(_, v)| v + 1);
    MetaTxn {
        preconditions: vec![Precondition::RefIs {
            name: name.to_owned(),
            expected,
        }],
        writes: vec![
            Write::SetRef(RefRecord {
                name: name.to_owned(),
                target,
                version,
                kind: RefKind::Line,
            }),
            Write::Journal {
                kind: "RefMoved".to_owned(),
                body: name.as_bytes().to_vec(),
            },
        ],
    }
}

fn cs(n: u8) -> ChangesetId {
    ChangesetId(ObjectId([n; 32]))
}

#[test]
fn acknowledged_writes_survive_a_reopen() {
    let dir = TempDir::new("reopen");
    let repo = {
        let store = open(&dir);
        let repo = block_on(store.create_repo("r")).expect("create");
        let out = block_on(store.commit(repo, cas("line/main", None, cs(1)))).expect("commit");
        assert_eq!(out, TxnOutcome::Committed(vec![1]));
        repo
    };
    let store = open(&dir);
    assert_eq!(block_on(store.repo("r")).expect("repo"), Some(repo));
    let r = block_on(store.get_ref(repo, "line/main"))
        .expect("read")
        .expect("present");
    assert_eq!((r.target, r.version), (cs(1), 1));
    let out = block_on(store.commit(repo, cas("line/main", Some((cs(1), 1)), cs(2)))).expect("cas");
    assert_eq!(
        out,
        TxnOutcome::Committed(vec![2]),
        "LSNs continue after reopen"
    );
}

#[test]
fn a_full_writer_queue_is_busy() {
    let dir = TempDir::new("busy");
    let mut config = SqliteConfig::new(dir.db());
    config.writer_queue = 1;
    config.retry_after_ms = 25;
    let store = open_with(config);
    let repo = block_on(store.create_repo("r")).expect("create");
    let taken_before = store.writer_batches_taken();

    // Hold the database's write lock, so the writer stalls on its first job
    // and the queue behind it fills.
    let blocker = Connection::open(dir.db()).expect("open blocker");
    blocker.execute_batch("BEGIN IMMEDIATE").expect("lock");

    let names = ["line/a", "line/b", "line/c"];
    let mut futures: Vec<_> = names
        .iter()
        .map(|n| Box::pin(store.commit(repo, cas(n, None, cs(1)))))
        .collect();
    let mut cx = Context::from_waker(Waker::noop());

    // The first job goes to the writer, which takes it off the queue before
    // it stalls. Only then is the queue empty, so fill it after that: the
    // second job queues and the third is refused.
    assert!(futures[0].as_mut().poll(&mut cx).is_pending());
    wait_until_taken(&store, taken_before + 1);

    let mut busy = 0;
    let mut pending = vec![0];
    for (i, f) in futures.iter_mut().enumerate().skip(1) {
        match f.as_mut().poll(&mut cx) {
            Poll::Ready(Err(StoreError::Busy(ms))) => {
                assert_eq!(ms, 25, "the configured retry hint");
                busy += 1;
            }
            Poll::Ready(other) => panic!("unexpected early result: {other:?}"),
            Poll::Pending => pending.push(i),
        }
    }
    // One job in the writer's hands and one in the queue at most.
    assert!(busy >= 1, "a full queue refuses at once");
    assert!(pending.len() <= 2);

    blocker.execute_batch("ROLLBACK").expect("unlock");
    for i in pending {
        let out = block_on(&mut futures[i]).expect("queued commit");
        assert!(matches!(out, TxnOutcome::Committed(_)));
    }
}

fn open_with(config: SqliteConfig) -> SqliteMetaStore {
    SqliteMetaStore::open(config).expect("open store")
}

/// Waits until the writer has taken at least `count` batches off its queue.
fn wait_until_taken(store: &SqliteMetaStore, count: u64) {
    let deadline = Instant::now() + Duration::from_secs(10);
    while store.writer_batches_taken() < count {
        assert!(Instant::now() < deadline, "the writer never took the job");
        std::thread::sleep(Duration::from_millis(1));
    }
}

#[test]
fn concurrent_writers_get_gap_free_lsns() {
    let dir = TempDir::new("concurrent");
    let store = std::sync::Arc::new(open(&dir));
    let repo = block_on(store.create_repo("r")).expect("create");
    let threads: Vec<_> = (0..8u8)
        .map(|t| {
            let store = store.clone();
            std::thread::spawn(move || {
                let name = format!("line/t{t}");
                let mut expected = None;
                let mut lsns = Vec::new();
                for v in 1..=50u8 {
                    let out =
                        block_on(store.commit(repo, cas(&name, expected, cs(v)))).expect("commit");
                    let TxnOutcome::Committed(l) = out else {
                        panic!("{name}: {out:?}");
                    };
                    lsns.extend(l);
                    expected = Some((cs(v), u64::from(v)));
                }
                lsns
            })
        })
        .collect();
    let mut all: Vec<u64> = threads
        .into_iter()
        .flat_map(|t| t.join().expect("thread"))
        .collect();
    all.sort_unstable();
    assert_eq!(all, (1..=400).collect::<Vec<_>>());
    let journal = block_on(store.journal(repo, 1, 1000)).expect("journal");
    assert_eq!(journal.len(), 400);
}
