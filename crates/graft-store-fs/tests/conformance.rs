//! Runs the conformance suite against the filesystem store (work item G1.5).
//!
//! The suite stores opaque bytes, not frames, so read verification is off
//! here; `tests/store.rs` covers it with real frames.

#![cfg(unix)]

use std::cell::Cell;

use graft_store::memory::MemoryStore;
use graft_store_fs::{FsMode, FsObjectStore};
use graft_testkit::crash::{TempDir, block_on};

fn run(mode: FsMode) {
    run_with(mode, true);
}

fn run_with(mode: FsMode, sync_writes: bool) {
    let base = TempDir::new("fs-conformance");
    let next = Cell::new(0u32);
    block_on(graft_store::conformance::run_all(|| {
        next.set(next.get() + 1);
        let root = base.path().join(next.get().to_string());
        let objects = FsObjectStore::open(root, mode)
            .expect("open")
            .verify_reads(false)
            .sync_writes(sync_writes);
        (objects, MemoryStore::new())
    }));
}

#[test]
fn authoritative_store_passes_the_conformance_suite() {
    run(FsMode::Authoritative);
}

#[test]
fn lru_store_under_its_bound_passes_the_conformance_suite() {
    run(FsMode::Lru { max_bytes: 1 << 20 });
}

#[test]
fn unsynced_lru_store_passes_the_conformance_suite() {
    run_with(FsMode::Lru { max_bytes: 1 << 20 }, false);
}
