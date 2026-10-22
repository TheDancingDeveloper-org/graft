//! The store reports its writer queue and every operation to an observer
//! (the server turns these into metrics).

mod common;

use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

use common::{TempDir, block_on};
use graft_store::{MetaStore, MetaTxn, StoreError};
use graft_store_sqlite::{SqliteConfig, SqliteMetaStore, StoreObserver};

#[derive(Debug, Default)]
struct Recorder {
    depths: Mutex<Vec<usize>>,
    ops: Mutex<Vec<&'static str>>,
}

impl StoreObserver for Recorder {
    fn writer_queue_depth(&self, depth: usize) {
        self.depths.lock().unwrap().push(depth);
    }

    fn operation(&self, op: &'static str, _elapsed: Duration) {
        self.ops.lock().unwrap().push(op);
    }
}

#[test]
fn writes_and_reads_are_reported_with_their_names() {
    let dir = TempDir::new("observer");
    let rec = Arc::new(Recorder::default());
    let observer: Arc<dyn StoreObserver> = rec.clone();
    let store = SqliteMetaStore::open_observed(SqliteConfig::new(dir.db()), Some(observer))
        .expect("open store");

    block_on(async {
        let repo = store.create_repo("r").await.expect("create");
        store
            .commit(repo, MetaTxn::default())
            .await
            .expect("commit");
        store.repo("r").await.expect("lookup");
    });

    let ops = rec.ops.lock().unwrap().clone();
    assert_eq!(ops, ["create_repo", "commit", "repo"]);
    // Each queued write moves the depth up and then back down, so the last
    // report is an empty queue and the peak is one job.
    let depths = rec.depths.lock().unwrap().clone();
    assert_eq!(depths, [1, 0, 1, 0]);
}

#[test]
fn an_unobserved_store_still_works() {
    let dir = TempDir::new("unobserved");
    let store = SqliteMetaStore::open(SqliteConfig::new(dir.db())).expect("open store");
    block_on(async {
        store.create_repo("r").await.expect("create");
    });
}

/// Keeps only the last depth reported, as a gauge does, and is slow to
/// record a non-empty queue, so a report that arrives out of order shows.
#[derive(Debug, Default)]
struct Gauge(Mutex<Option<usize>>);

impl StoreObserver for Gauge {
    fn writer_queue_depth(&self, depth: usize) {
        if depth > 0 {
            thread::sleep(Duration::from_millis(1));
        }
        *self.0.lock().unwrap() = Some(depth);
    }

    fn operation(&self, _op: &'static str, _elapsed: Duration) {}
}

#[test]
fn the_last_depth_reported_is_zero_after_a_burst_that_overfills_the_queue() {
    let dir = TempDir::new("observer-concurrent");
    let gauge = Arc::new(Gauge::default());
    let observer: Arc<dyn StoreObserver> = gauge.clone();
    // A one-job queue, so the burst below overfills it and some writes are
    // refused as busy: a refused write reports its depth twice, from the
    // caller's thread, racing the writer's own reports.
    let mut config = SqliteConfig::new(dir.db());
    config.writer_queue = 1;
    let store =
        Arc::new(SqliteMetaStore::open_observed(config, Some(observer)).expect("open store"));
    let repo = block_on(store.create_repo("r")).expect("create");

    // A stale report shows only when it happens to land last, so repeat
    // the burst.
    for round in 0..5 {
        let writers: Vec<_> = (0..16)
            .map(|_| {
                let store = store.clone();
                thread::spawn(move || {
                    for _ in 0..20 {
                        match block_on(store.commit(repo, MetaTxn::default())) {
                            Ok(_) | Err(StoreError::Busy(_)) => {}
                            Err(e) => panic!("commit: {e:?}"),
                        }
                    }
                })
            })
            .collect();
        for w in writers {
            w.join().expect("writer thread");
        }
        // Every write has been answered or refused, so the queue is empty.
        assert_eq!(*gauge.0.lock().unwrap(), Some(0), "round {round}");
    }
}
