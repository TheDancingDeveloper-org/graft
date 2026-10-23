//! G3.2's crash harness, deterministic half: a sweep pass that does every
//! step (lapse, collect with a successor and an unwinding stack, expire an
//! upload, delete what an earlier pass doomed) is crashed at every store
//! call it makes, in turn. After each crash:
//!
//! - the stores are consistent: the brute-force check of design §8.6 holds
//!   before any recovery;
//! - start-up recovery and further passes reach exactly the state an
//!   uncrashed sweep reaches.
//!
//! Some crash lands between the `deleting` mark and the frame delete; that
//! case is also checked on its own: a write that names the object meanwhile
//! is told it is missing, recovery finishes the delete, and a re-upload
//! makes it good. (The process-kill half is `kill.rs`.)

mod common;

use std::collections::{BTreeMap, BTreeSet};

use common::*;
use graft_model::{ChangesetId, ObjectId};
use graft_proto::ApiError;
use graft_proto::types::Landed;
use graft_store::memory::MemoryStore;
use graft_store::{DoomMark, MetaStore, RecordState};

/// A repository where one pass has every kind of work. Returns the blob
/// the delete step takes.
fn busy(w: &World<MemoryStore>) -> ObjectId {
    let root = w.seed();
    // Doomed by an earlier pass, deleted by this one.
    w.open("old").expect("open");
    w.commit(vec![root], "old", b"old1", Some("old"))
        .expect("old");
    w.close("old").expect("close");
    w.advance(RETENTION_PLUS);
    assert_eq!(w.sweep().collected, 1);

    // A closed stack that unwinds from the top.
    w.open("w0").expect("open");
    let a = w
        .commit(vec![root], "s/a", b"a1", Some("w0"))
        .expect("a")
        .id;
    w.commit(vec![a], "s/b", b"b1", Some("w0")).expect("b");
    w.close("w0").expect("close");
    // A superseded draft whose successor landed.
    w.open("w1").expect("open");
    let c = w.commit(vec![root], "c", b"c1", Some("w1")).expect("c").id;
    let moved = w.commit(vec![root], "m", b"m1", None).expect("m").id;
    w.update_ref(Some(root), moved).expect("move");
    assert!(matches!(w.land(c), Ok(Landed::Landed { .. })));
    // A stray upload, and a workspace that will lapse.
    w.upload(&[b"stray"]).expect("upload");
    w.open("w2").expect("open");
    w.commit(vec![w.tip()], "l", b"l1", Some("w2")).expect("l");
    w.advance(RETENTION_PLUS);
    blob_id(b"old1")
}

/// What a settled repository looks like.
#[derive(Debug, PartialEq, Eq)]
struct Settled {
    frames: BTreeSet<ObjectId>,
    records: BTreeMap<ChangesetId, RecordState>,
    marks: usize,
}

fn settled(w: &World<MemoryStore>) -> Settled {
    w.check().expect("protection");
    Settled {
        frames: w.frames.sizes(w.repo).into_keys().collect(),
        records: all_records(&*w.meta, w.repo)
            .into_iter()
            .map(|s| (s.id, s.record.state))
            .collect(),
        marks: deleting_or_doomed(w).len(),
    }
}

fn deleting_or_doomed(w: &World<MemoryStore>) -> Vec<(ObjectId, DoomMark)> {
    block_on(w.meta.old_doomed(w.repo, i64::MAX, None, 1000)).expect("marks")
}

#[test]
fn a_crash_at_any_call_of_a_pass_recovers_to_the_same_state() {
    let reference = {
        let w = memory();
        busy(&w);
        w.gc_ctl.reset_counts();
        let pass = w.sweep();
        assert!(
            pass.lapsed > 0
                && pass.collected >= 3
                && pass.moved > 0
                && pass.expired > 0
                && pass.deleted > 0,
            "the pass does every kind of work: {pass:?}"
        );
        let calls = w.gc_ctl.calls();
        w.drain();
        (calls, settled(&w))
    };
    let (calls, want) = reference;

    let mut between_mark_and_delete = 0;
    for n in 0..calls {
        let w = memory();
        busy(&w);
        w.gc_ctl.crash_at(n);
        let crashed = block_on(w.gc.sweep(w.repo, &w.shape, false));
        w.gc_ctl.no_crash();
        assert!(crashed.is_err(), "call {n} of {calls} crashes the pass");
        assert!(!w.gc.leases().is_held(w.repo), "the lease went with it");
        w.check()
            .unwrap_or_else(|e| panic!("after a crash at call {n}: {e}"));
        if deleting_or_doomed(&w)
            .iter()
            .any(|(o, m)| m.deleting && w.has_frame(*o))
        {
            between_mark_and_delete += 1;
        }
        // Start-up, then the sweeps carry on.
        block_on(w.gc.recover(w.repo)).expect("recover");
        assert!(
            deleting_or_doomed(&w).iter().all(|(_, m)| !m.deleting),
            "recovery finished every delete"
        );
        w.drain();
        assert_eq!(settled(&w), want, "after a crash at call {n}");
    }
    assert!(
        between_mark_and_delete > 0,
        "some crash fell between the deleting mark and the frame delete"
    );
}

#[test]
fn a_crash_between_the_deleting_mark_and_the_frame_delete() {
    // Find the first call that leaves a `deleting` mark over a stored
    // frame: the crash between the mark's transaction and the delete.
    let mut n = 0;
    let (w, object) = loop {
        let w = memory();
        let old = busy(&w);
        w.gc_ctl.crash_at(n);
        let crashed = block_on(w.gc.sweep(w.repo, &w.shape, false));
        w.gc_ctl.no_crash();
        assert!(crashed.is_err(), "no crash point found");
        let marked: Vec<ObjectId> = deleting_or_doomed(&w)
            .into_iter()
            .filter(|(o, m)| m.deleting && w.has_frame(*o))
            .map(|(o, _)| o)
            .collect();
        if marked.contains(&old) {
            break (w, old);
        }
        n += 1;
    };
    w.check().expect("consistent");

    // A write naming the object meanwhile cannot claim it.
    let root = w.tip();
    w.open("late").expect("open");
    let named = w.create(vec![root], vec![put("old", b"old1")], Some("late"));
    assert!(
        matches!(&named, Err(ApiError::MissingObjects { ids }) if ids == &[object]),
        "{named:?}"
    );
    // Nor can an upload of it until the delete is done.
    assert!(matches!(
        w.upload(&[b"old1"]),
        Err(ApiError::Unavailable { .. })
    ));

    // Start-up recovery finishes the delete.
    assert_eq!(block_on(w.gc.recover(w.repo)).expect("recover"), 1);
    assert!(!w.has_frame(object));
    assert!(w.marks(object).is_none());
    w.check().expect("consistent");

    // And a re-upload makes it good.
    w.upload(&[b"old1"]).expect("re-upload");
    let created = w
        .create(vec![root], vec![put("old", b"old1")], Some("late"))
        .expect("after the re-upload");
    assert!(w.snapshot_of(created.id).expect("whole").contains(&object));
    w.drain();
    w.check().expect("consistent");
}
