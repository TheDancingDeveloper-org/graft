//! The store reports its writer queue and every operation to an observer
//! (the server turns these into metrics).

mod common;

use std::sync::{Arc, Mutex};
use std::time::Duration;

use common::{TempDir, block_on};
use graft_store::{MetaStore, MetaTxn};
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
