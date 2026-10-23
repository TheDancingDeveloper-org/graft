//! G3.2's concurrency races, explored systematically (design §8.6, "Crash
//! and concurrency safety"). Each race runs a write against a sweep pass on
//! one thread, with the stores yielding at every call, under every
//! schedule of two families:
//!
//! - *split at k*: one task makes k store calls, the other runs to the end,
//!   then the first finishes (the lost-update shape: a read, a whole
//!   concurrent write, then the commit built on the read);
//! - *cyclic*: every alternation pattern up to five steps long, repeated.
//!
//! The races:
//!
//! - adoption racing collection: the adopted record is a draft, or the
//!   adoption fails `Collected`, never both;
//! - a new child racing its parent's collection: the child pins the parent,
//!   or fails `Collected`;
//! - a write naming a blob racing its delete: the write keeps it, or fails
//!   `MissingObjects`; it never succeeds over a deleted frame;
//! - a write deriving a tree racing its delete: the write keeps it, or
//!   waits and puts it again;
//! - a re-upload racing a delete: the frame is held and stored, or the
//!   upload is told to retry.
//!
//! After every schedule the brute-force check of design §8.6 holds.

mod common;

use common::*;
use graft_model::ChangesetId;
use graft_proto::types::*;
use graft_proto::{ApiError, ObjectApi, RepoApi, WorkspaceApi};
use graft_store::RecordState;
use graft_store::memory::MemoryStore;
use std::future::Future;
use std::task::{Context, Poll, Waker};

/// Runs `a` and `b`: `first` (a when `a_first`) makes `k` steps, the other
/// runs to the end, then the first finishes.
fn split<A: Future, B: Future>(a: A, b: B, k: usize, a_first: bool) -> (A::Output, B::Output) {
    let mut a = std::pin::pin!(a);
    let mut b = std::pin::pin!(b);
    let mut cx = Context::from_waker(Waker::noop());
    let (mut out_a, mut out_b) = (None, None);
    let mut poll_a = |out: &mut Option<A::Output>| {
        if out.is_none()
            && let Poll::Ready(v) = a.as_mut().poll(&mut cx)
        {
            *out = Some(v);
        }
    };
    let mut cx_b = Context::from_waker(Waker::noop());
    let mut poll_b = |out: &mut Option<B::Output>| {
        if out.is_none()
            && let Poll::Ready(v) = b.as_mut().poll(&mut cx_b)
        {
            *out = Some(v);
        }
    };
    for _ in 0..k {
        if a_first {
            poll_a(&mut out_a);
        } else {
            poll_b(&mut out_b);
        }
    }
    while out_a.is_none() || out_b.is_none() {
        if a_first {
            while out_b.is_none() {
                poll_b(&mut out_b);
            }
            poll_a(&mut out_a);
        } else {
            while out_a.is_none() {
                poll_a(&mut out_a);
            }
            poll_b(&mut out_b);
        }
    }
    (out_a.expect("a"), out_b.expect("b"))
}

#[derive(Clone, Debug)]
enum Schedule {
    Split { k: usize, write_first: bool },
    Cyclic(Vec<bool>),
}

fn schedules() -> Vec<Schedule> {
    let mut out = Vec::new();
    for k in 0..48 {
        for write_first in [true, false] {
            out.push(Schedule::Split { k, write_first });
        }
    }
    for len in 1..=5 {
        for bits in 0..(1u32 << len) {
            out.push(Schedule::Cyclic(
                (0..len).map(|i| bits >> i & 1 == 1).collect(),
            ));
        }
    }
    out
}

/// Races `write` against a sweep pass under `schedule`.
fn race<T>(
    w: &World<MemoryStore>,
    write: impl Future<Output = T>,
    schedule: &Schedule,
) -> (T, graft_gc::SweepReport) {
    w.yields(true);
    let sweep = w.gc.sweep(w.repo, &w.shape, false);
    let (out, swept) = match schedule {
        Schedule::Split { k, write_first } => split(write, sweep, *k, *write_first),
        Schedule::Cyclic(pattern) => interleave(write, sweep, pattern),
    };
    w.yields(false);
    (out, swept.expect("sweep"))
}

/// An abandoned changeset `x` on the line, past its expiry, and an open
/// workspace.
fn expired_abandoned(w: &World<MemoryStore>) -> ChangesetId {
    let root = w.seed();
    let x = w.commit(vec![root], "x", b"x1", None).expect("x").id;
    assert_eq!(w.record(x).state, RecordState::Abandoned);
    w.advance(RETENTION_PLUS);
    w.open("w").expect("open");
    x
}

#[test]
fn adoption_racing_collection_wins_or_fails_collected() {
    let (mut adopted, mut collected) = (0, 0);
    for schedule in schedules() {
        let w = memory();
        let x = expired_abandoned(&w);
        let adopt = w.engine.set_head(
            &w.cx,
            SetHead {
                workspace: w.wref("w"),
                expected: None,
                new: Some(x),
            },
        );
        let (result, _) = race(&w, adopt, &schedule);
        match result {
            Ok(_) => {
                assert_eq!(w.record(x).state, RecordState::Draft, "{schedule:?}");
                w.snapshot_of(x).expect("the adopted draft is whole");
                adopted += 1;
            }
            Err(ApiError::Collected { .. }) => {
                assert_eq!(w.record(x).state, RecordState::Collected, "{schedule:?}");
                collected += 1;
            }
            Err(e) => panic!("{schedule:?}: {e:?}"),
        }
        w.check().unwrap_or_else(|e| panic!("{schedule:?}: {e}"));
    }
    assert!(adopted > 0 && collected > 0, "both orders ran");
}

#[test]
fn a_child_racing_its_parents_collection_pins_it_or_fails_collected() {
    let (mut pinned, mut collected) = (0, 0);
    for schedule in schedules() {
        let w = memory();
        let x = expired_abandoned(&w);
        w.upload(&[b"y1"]).expect("upload");
        let child = w
            .engine
            .create_changeset(&w.cx, w.request(vec![x], vec![put("y", b"y1")], None));
        let (result, _) = race(&w, child, &schedule);
        match result {
            Ok(created) => {
                assert_ne!(w.record(x).state, RecordState::Collected, "{schedule:?}");
                assert_eq!(w.protection(x).pins, 1);
                w.snapshot_of(created.id).expect("the child is whole");
                pinned += 1;
            }
            Err(ApiError::Collected { .. }) => {
                assert_eq!(w.record(x).state, RecordState::Collected, "{schedule:?}");
                collected += 1;
            }
            Err(e) => panic!("{schedule:?}: {e:?}"),
        }
        w.check().unwrap_or_else(|e| panic!("{schedule:?}: {e}"));
    }
    assert!(pinned > 0 && collected > 0, "both orders ran");
}

/// A draft `a` whose blob `a1` (and root tree) were collected and doomed
/// for longer than the grace period, ready to delete; `w2` is open.
fn doomed_past_grace(w: &World<MemoryStore>, data: &[u8]) -> (ChangesetId, ChangesetId) {
    let root = w.seed();
    w.open("w").expect("open");
    let a = w.commit(vec![root], "a", data, Some("w")).expect("a").id;
    w.close("w").expect("close");
    w.advance(RETENTION_PLUS);
    assert_eq!(w.sweep().collected, 1);
    w.advance(GRACE_PLUS);
    w.open("w2").expect("open");
    (root, a)
}

#[test]
fn a_write_naming_a_blob_racing_its_delete_keeps_it_or_misses_it() {
    let (mut kept, mut missing) = (0, 0);
    let a1 = blob_id(b"a1");
    for schedule in schedules() {
        let w = memory();
        let (root, _) = doomed_past_grace(&w, b"a1");
        let write = w.engine.create_changeset(
            &w.cx,
            w.request(vec![root], vec![put("z", b"a1")], Some("w2")),
        );
        let (result, _) = race(&w, write, &schedule);
        match result {
            Ok(created) => {
                assert!(
                    w.has_frame(a1),
                    "{schedule:?}: acknowledged over a deleted frame"
                );
                w.snapshot_of(created.id).expect("whole");
                kept += 1;
            }
            Err(ApiError::MissingObjects { ids }) => {
                assert_eq!(ids, [a1]);
                missing += 1;
            }
            Err(e) => panic!("{schedule:?}: {e:?}"),
        }
        w.check().unwrap_or_else(|e| panic!("{schedule:?}: {e}"));
        // Whatever happened, a re-upload makes the blob good.
        w.upload(&[b"a1"]).expect("re-upload");
        w.open("w3").expect("open");
        w.create(vec![root], vec![put("q", b"a1")], Some("w3"))
            .expect("after the re-upload");
        w.check().unwrap_or_else(|e| panic!("{schedule:?}: {e}"));
    }
    assert!(kept > 0 && missing > 0, "both orders ran");
}

#[test]
fn a_write_deriving_a_tree_racing_its_delete_keeps_or_restores_it() {
    let mut whole = 0;
    for schedule in schedules() {
        let w = memory();
        // `a` puts the landed blob `b0` at a new path: its blob stays
        // held, only its root tree is doomed.
        let (root, a) = doomed_past_grace(&w, b"b0");
        let tree = w.snapshot_of(root).expect("root");
        let a_tree: Vec<_> = {
            let (content, _) = block_on(graft_store::MetaStore::changeset(&*w.meta, w.repo, a))
                .expect("read")
                .expect("stored");
            vec![
                graft_model::Changeset::decode(&content)
                    .expect("decode")
                    .root
                    .0,
            ]
        };
        assert!(!tree.contains(&a_tree[0]));
        // The same change on the same parent derives the same tree.
        let write = w.engine.create_changeset(
            &w.cx,
            w.request(vec![root], vec![put("a", b"b0")], Some("w2")),
        );
        let (result, _) = race(&w, write, &schedule);
        match result {
            Ok(created) => {
                w.snapshot_of(created.id).expect("whole");
                whole += 1;
            }
            // It waited on the delete as long as it may; the sweep ran no
            // further in this schedule.
            Err(ApiError::Unavailable { .. }) => {}
            Err(e) => panic!("{schedule:?}: {e:?}"),
        }
        w.check().unwrap_or_else(|e| panic!("{schedule:?}: {e}"));
    }
    assert!(whole > 0);
}

#[test]
fn a_re_upload_racing_a_delete_is_held_or_retried() {
    let (mut stored, mut retried) = (0, 0);
    let a1 = blob_id(b"a1");
    for schedule in schedules() {
        let w = memory();
        doomed_past_grace(&w, b"a1");
        let upload = w.engine.put_objects(
            &w.cx,
            PutObjects {
                repo: w.name.clone(),
                blobs: vec![graft_proto::Frame::encode(
                    graft_model::ObjectKind::Blob,
                    b"a1",
                )],
            },
        );
        let (result, _) = race(&w, upload, &schedule);
        match result {
            Ok(_) => {
                assert!(w.has_frame(a1), "{schedule:?}: stored");
                assert!(!w.holders_of(a1).is_empty(), "{schedule:?}: held");
                assert!(w.marks(a1).is_none(), "{schedule:?}: not doomed");
                stored += 1;
            }
            Err(ApiError::Unavailable { .. }) => retried += 1,
            Err(e) => panic!("{schedule:?}: {e:?}"),
        }
        w.check().unwrap_or_else(|e| panic!("{schedule:?}: {e}"));
        // The next sweep never deletes a frame an upload holds.
        w.advance(GRACE_PLUS);
        w.sweep();
        if !w.holders_of(a1).is_empty() {
            assert!(w.has_frame(a1), "{schedule:?}");
        }
    }
    assert!(stored > 0, "stored {stored}, retried {retried}");
}
