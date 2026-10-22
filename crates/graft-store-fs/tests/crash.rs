//! The crash test (work item G1.5, design §6.6): a child process puts
//! 10,000 frames from several threads and acknowledges each put once it
//! returns; the parent kills it with `SIGKILL` at a random point, reopens
//! the store, and checks that
//!
//! - every acknowledged object is present and verifies against its id;
//! - no object file anywhere is torn (each one verifies);
//! - no temp file survives the reopen, and the counts match the disk.
//!
//! Twenty kills, all against the same root, so each reopen also cleans up
//! after a store that has already crashed.

#![cfg(unix)]

use std::collections::BTreeSet;
use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, Instant};

use graft_model::{ObjectId, ObjectKind};
use graft_proto::frame::Frame;
use graft_store::{ObjectStore, RepoId};
use graft_store_fs::{FsMode, FsObjectStore};
use graft_testkit::crash::{self, Delays, TempDir, block_on};

const TEST: &str = "acknowledged_puts_survive_kill_9";
const ITERATIONS: u32 = 20;
const PUTS: u32 = 10_000;
const WRITERS: u32 = 16;
const REPO: RepoId = RepoId(1);

/// The payload of put `n` in `iteration`: sizes from a few bytes to a few
/// KiB, so frames are both raw and gzip.
fn payload(iteration: u32, n: u32) -> Vec<u8> {
    let len = 16 + (n as usize * 7919) % 6000;
    let mut out = format!("crash {iteration} put {n} ").into_bytes();
    out.extend(b"graft ".iter().copied().cycle().take(len));
    out
}

/// The child: put every frame of this iteration from `WRITERS` threads.
fn write(root: &Path, iteration: u32) {
    let store = Arc::new(FsObjectStore::open(root, FsMode::Authoritative).expect("open"));
    let writers: Vec<_> = (0..WRITERS)
        .map(|w| {
            let store = store.clone();
            std::thread::spawn(move || {
                for n in (w..PUTS).step_by(WRITERS as usize) {
                    let payload = payload(iteration, n);
                    let id = ObjectId::hash(ObjectKind::Blob, &payload);
                    let frame = Frame::encode(ObjectKind::Blob, &payload).to_bytes();
                    block_on(store.put(REPO, id, frame)).expect("put");
                    crash::ack(&id.to_hex());
                }
            })
        })
        .collect();
    for w in writers {
        w.join().expect("writer");
    }
}

/// Every object file and temp file under the root.
fn scan(root: &Path) -> (BTreeSet<ObjectId>, usize) {
    let (mut ids, mut temps) = (BTreeSet::new(), 0);
    let objects = root.join(REPO.0.to_string()).join("objects");
    for shard in std::fs::read_dir(objects).expect("shards") {
        let shard = shard.expect("shard");
        let prefix = shard.file_name().into_string().expect("utf-8");
        for file in std::fs::read_dir(shard.path()).expect("files") {
            let name = file
                .expect("file")
                .file_name()
                .into_string()
                .expect("utf-8");
            if name.ends_with(".tmp") {
                temps += 1;
            } else {
                ids.insert(ObjectId::from_hex(&format!("{prefix}{name}")).expect("id"));
            }
        }
    }
    (ids, temps)
}

/// Reopens the store and checks it against everything acknowledged so far.
/// Objects in `verified` were checked after an earlier kill; files are
/// immutable, so only the new ones are read.
fn verify(root: &Path, acked: &BTreeSet<ObjectId>, verified: &mut BTreeSet<ObjectId>) {
    let store = FsObjectStore::open(root, FsMode::Authoritative).expect("reopen");
    let (on_disk, temps) = scan(root);
    assert_eq!(temps, 0, "reopen removed every temp file");
    assert!(
        on_disk.is_superset(acked),
        "{} acknowledged objects are missing",
        acked.difference(&on_disk).count()
    );
    for &id in on_disk.difference(verified) {
        let frame = block_on(store.get(REPO, id))
            .unwrap_or_else(|e| panic!("object {} is torn: {e:?}", id.to_hex()))
            .expect("present");
        assert!(!frame.is_empty());
    }
    let stats = block_on(store.stats()).expect("stats");
    assert_eq!(stats.objects, on_disk.len() as u64, "counts match the disk");
    *verified = on_disk;
}

#[test]
fn acknowledged_puts_survive_kill_9() {
    if let Some(child) = crash::child() {
        return write(&child.dir, child.iteration);
    }
    let dir = TempDir::new("fs-crash");
    // A fixed seed for the kill delays, so a failure reproduces.
    let mut delays = Delays::new(0x9e37_79b9_7f4a_7c15);
    let mut acked = BTreeSet::new();
    let mut verified = BTreeSet::new();
    let mut kills = 0;
    let started = Instant::now();
    for iteration in 1..=ITERATIONS {
        let delay = delays.next(Duration::ZERO, Duration::from_millis(1000));
        let run = crash::run_child(TEST, dir.path(), iteration, delay);
        kills += u32::from(run.killed);
        for ack in &run.acks {
            acked.insert(ObjectId::from_hex(ack).expect("acknowledged id"));
        }
        verify(dir.path(), &acked, &mut verified);
    }
    eprintln!(
        "crash test: {kills}/{ITERATIONS} kills, {} acknowledged puts, {:?}",
        acked.len(),
        started.elapsed()
    );
    assert!(kills >= ITERATIONS / 2, "most runs were killed mid-write");
}
