//! Runs the conformance suite against the in-memory store (work item G1.2).

#![cfg(feature = "testkit")]

use std::future::Future;
use std::pin::pin;
use std::task::{Context, Poll, Waker};

use graft_store::memory::MemoryStore;

/// Drives a future to completion. The memory store never waits, so its
/// futures are ready on the first poll, and no executor is needed.
fn block_on<F: Future>(future: F) -> F::Output {
    let mut future = pin!(future);
    let mut cx = Context::from_waker(Waker::noop());
    loop {
        if let Poll::Ready(output) = future.as_mut().poll(&mut cx) {
            return output;
        }
    }
}

#[test]
fn memory_store_passes_the_conformance_suite() {
    block_on(graft_store::conformance::run_all(|| {
        let store = MemoryStore::new();
        (store.clone(), store)
    }));
}
