//! Ref CAS throughput micro-benchmark (work item G1.6: at least 1,000 ref
//! CAS operations per second sustained). Not a test; run it with
//!
//! ```sh
//! cargo run --release -p graft-store-sqlite --example cas_bench -- [clients] [seconds] [dir]
//! ```
//!
//! Each client moves its own ref in a loop (precondition `RefIs`, `SetRef`,
//! a `RefMoved` journal entry), awaiting each commit before the next, so
//! every operation is a durable, acknowledged CAS. The database goes in
//! `dir` (default: the system temp directory), which should be on the
//! volume the server would use.

use std::future::Future;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::task::{Context, Poll, Wake, Waker};
use std::thread;
use std::time::{Duration, Instant};

use graft_model::{ChangesetId, ObjectId};
use graft_store::{MetaStore, MetaTxn, Precondition, RefKind, RefRecord, TxnOutcome, Write};
use graft_store_sqlite::{SqliteConfig, SqliteMetaStore};

struct Unpark(thread::Thread);

impl Wake for Unpark {
    fn wake(self: Arc<Self>) {
        self.0.unpark();
    }
}

fn block_on<F: Future>(future: F) -> F::Output {
    let mut future = std::pin::pin!(future);
    let waker = Waker::from(Arc::new(Unpark(thread::current())));
    let mut cx = Context::from_waker(&waker);
    loop {
        if let Poll::Ready(output) = future.as_mut().poll(&mut cx) {
            return output;
        }
        thread::park();
    }
}

fn main() {
    let mut args = std::env::args().skip(1);
    let clients: u32 = args.next().map_or(16, |a| a.parse().expect("clients"));
    let seconds: u64 = args.next().map_or(5, |a| a.parse().expect("seconds"));
    let base = args.next().map_or_else(std::env::temp_dir, Into::into);
    let dir = base.join(format!("graft-cas-bench-{}", std::process::id()));
    std::fs::create_dir_all(&dir).expect("create dir");

    let store =
        Arc::new(SqliteMetaStore::open(SqliteConfig::new(dir.join("meta.sqlite"))).expect("open"));
    let repo = block_on(store.create_repo("bench")).expect("create repo");
    let done = Arc::new(AtomicBool::new(false));
    let ops = Arc::new(AtomicU64::new(0));
    let busy = Arc::new(AtomicU64::new(0));

    let started = Instant::now();
    let workers: Vec<_> = (0..clients)
        .map(|c| {
            let (store, done, ops, busy) = (store.clone(), done.clone(), ops.clone(), busy.clone());
            thread::spawn(move || {
                let name = format!("line/c{c}");
                let mut current: Option<(ChangesetId, u64)> = None;
                let mut n: u64 = 0;
                while !done.load(Ordering::Relaxed) {
                    n += 1;
                    let mut bytes = [0u8; 32];
                    bytes[..4].copy_from_slice(&c.to_be_bytes());
                    bytes[4..12].copy_from_slice(&n.to_be_bytes());
                    let target = ChangesetId(ObjectId(bytes));
                    let version = current.map_or(1, |(_, v)| v + 1);
                    let txn = MetaTxn {
                        preconditions: vec![Precondition::RefIs {
                            name: name.clone(),
                            expected: current,
                        }],
                        writes: vec![
                            Write::SetRef(RefRecord {
                                name: name.clone(),
                                target,
                                version,
                                kind: RefKind::Line,
                            }),
                            Write::Journal {
                                kind: "RefMoved".to_owned(),
                                body: name.as_bytes().to_vec(),
                            },
                        ],
                    };
                    match block_on(store.commit(repo, txn)) {
                        Ok(TxnOutcome::Committed(_)) => {
                            current = Some((target, version));
                            ops.fetch_add(1, Ordering::Relaxed);
                        }
                        Ok(other) => panic!("{name}: {other:?}"),
                        Err(graft_store::StoreError::Busy(ms)) => {
                            busy.fetch_add(1, Ordering::Relaxed);
                            thread::sleep(Duration::from_millis(ms));
                        }
                        Err(e) => panic!("{name}: {e:?}"),
                    }
                }
            })
        })
        .collect();

    let mut last = 0;
    for s in 1..=seconds {
        thread::sleep(Duration::from_secs(1));
        let now = ops.load(Ordering::Relaxed);
        println!("t={s}s  {} CAS/s", now - last);
        last = now;
    }
    done.store(true, Ordering::Relaxed);
    for w in workers {
        w.join().expect("worker");
    }
    let elapsed = started.elapsed().as_secs_f64();
    let total = ops.load(Ordering::Relaxed);
    println!(
        "{clients} clients, {total} CAS in {elapsed:.2}s = {:.0} CAS/s sustained ({} busy)",
        total as f64 / elapsed,
        busy.load(Ordering::Relaxed)
    );
    drop(store);
    let _ = std::fs::remove_dir_all(&dir);
}
