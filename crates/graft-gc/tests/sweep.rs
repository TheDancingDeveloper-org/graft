//! G3.2: the sweep (design §8.6), scenario by scenario, against the real
//! engine on the memory and SQLite stores.
//!
//! - Refusals: no `gc_protection` marker, deletion off, a second sweep (in
//!   this process or another, through the store lease).
//! - A closed workspace's drafts are collected after the retention window,
//!   and their objects deleted after the grace period; the reclaimed bytes
//!   are exactly the deleted frames.
//! - Revival during the grace period keeps an object; a reference after the
//!   delete is `MissingObjects`, and a re-upload makes it good.
//! - A parent with a live child is never collected; it goes in the same
//!   pass as its last child.
//! - A land candidate left by a crashed server is collected after its
//!   lease, and its draft is untouched; a land onto the same tip revives it.
//! - The lapse step closes lapsed workspaces as the engine does, a stack
//!   longer than `max_stack` included.
//! - A collected record keeps no content; `Collected` carries the bytes.
//! - The successor rule moves a superseded record's rows to its successor.
//! - Collection reads follow the changes collected, not the history depth.
//! - The dry run predicts the sweep; `--verify` catches a missing pin or
//!   row, and the sweep refuses to run on the mismatch.
//! - `graft_orphans{state="pinned"}` shows a pin leak.

mod common;

use common::*;
use graft_gc::{Gauges, GcError, Report};
use graft_model::{ChangesetId, ObjectId};
use graft_proto::types::Landed;
use graft_proto::{ApiError, RepoApi};
use graft_store::{MetaStore, RecordState, Write};

/// A draft `a1` at `a` in workspace `w`, over a seeded line.
fn draft<M: MetaStore + 'static>(w: &World<M>) -> (ChangesetId, ChangesetId) {
    let root = w.seed();
    w.open("w").expect("open");
    let a = w.commit(vec![root], "a", b"a1", Some("w")).expect("a").id;
    (root, a)
}

fn journal_kinds<M: MetaStore>(w: &World<M>) -> Vec<(String, String)> {
    block_on(w.meta.journal(w.repo, 1, 1000))
        .expect("journal")
        .into_iter()
        .map(|(_, kind, body)| (kind, String::from_utf8(body).expect("utf-8")))
        .collect()
}

macro_rules! on_both_stores {
    ($group:ident: $($name:ident),* $(,)?) => {
        mod $group {
            mod memory_store {
                $( #[test] fn $name() { super::super::$name(&super::super::memory()) } )*
            }
            mod sqlite_store {
                $( #[test] fn $name() { super::super::$name(&super::super::sqlite()) } )*
            }
        }
    };
}

// ---------------------------------------------------------------------------
// Refusals.

fn refuses_a_repository_without_the_marker<M: MetaStore + 'static>(w: &World<M>) {
    let (_, a) = draft(w);
    w.close("w").expect("close");
    w.advance(RETENTION_PLUS);
    commit_raw(&*w.meta, w.repo, vec![Write::SetGcProtection(false)]);
    let refused = block_on(w.gc.sweep(w.repo, &w.shape, false));
    assert_eq!(refused, Err(GcError::Unprotected));
    assert_eq!(w.record(a).state, RecordState::Abandoned, "nothing swept");
    // The dry run works either way: here it walks ancestry.
    let report = block_on(w.gc.dry_run(w.repo, w.now(), &w.shape)).expect("dry run");
    assert_eq!(report.changesets, 1);
}

fn refuses_until_the_operator_turns_deletion_on<M: MetaStore + 'static>(w: &World<M>) {
    let (_, a) = draft(w);
    w.close("w").expect("close");
    w.advance(RETENTION_PLUS);
    block_on(graft_gc::set_deletion(&*w.meta, w.repo, false, "ops")).expect("off");
    assert!(!block_on(graft_gc::deletion_enabled(&*w.meta, w.repo)).expect("read"));
    let refused = block_on(w.gc.sweep(w.repo, &w.shape, false));
    assert_eq!(refused, Err(GcError::DeletionOff));
    assert_eq!(w.record(a).state, RecordState::Abandoned, "nothing swept");
    let switched: Vec<_> = journal_kinds(w)
        .into_iter()
        .filter(|(kind, _)| kind == "AdminAction")
        .map(|(_, body)| body)
        .collect();
    assert_eq!(
        switched,
        [
            r#"{"what":"gc-deletion-on","actor":"tester"}"#,
            r#"{"what":"gc-deletion-off","actor":"ops"}"#,
        ]
    );
    block_on(graft_gc::set_deletion(&*w.meta, w.repo, true, "ops")).expect("on");
    assert_eq!(w.sweep().collected, 1);
}

fn a_new_repository_has_deletion_off<M: MetaStore + 'static>(w: &World<M>) {
    let other = block_on(w.meta.create_repo("fresh")).expect("repo");
    assert!(!block_on(graft_gc::deletion_enabled(&*w.meta, other)).expect("read"));
    assert_eq!(
        block_on(w.gc.sweep(other, &w.shape, false)),
        Err(GcError::DeletionOff)
    );
}

fn one_sweep_per_repository<M: MetaStore + 'static>(w: &World<M>) {
    w.seed();
    let held = block_on(w.gc.leases().try_acquire(&*w.meta, w.repo, w.now(), MINUTE))
        .expect("read")
        .expect("free");
    assert_eq!(
        block_on(w.gc.sweep(w.repo, &w.shape, false)),
        Err(GcError::Busy)
    );
    block_on(held.release(&*w.meta)).expect("release");
    w.sweep();
    assert!(!w.gc.leases().is_held(w.repo), "released after the pass");
}

/// The lease is in the store: a sweeper in another process (its own lease
/// set) is refused while it is held, and takes it once it lapses.
fn the_store_lease_stops_a_second_process<M: MetaStore + 'static>(w: &World<M>) {
    w.seed();
    let other = graft_gc::SweepLeases::with_owner("other-process");
    let held = block_on(other.try_acquire(&*w.meta, w.repo, w.now(), MINUTE))
        .expect("read")
        .expect("free");
    assert_eq!(
        block_on(w.gc.sweep(w.repo, &w.shape, false)),
        Err(GcError::Busy),
        "held by another process"
    );
    w.advance(MINUTE);
    w.sweep();
    drop(held);
    // And this pass gave it back: the other process takes it at once.
    assert!(
        block_on(other.try_acquire(&*w.meta, w.repo, w.now(), MINUTE))
            .expect("read")
            .is_some()
    );
}

on_both_stores!(
    refusals:
    refuses_a_repository_without_the_marker,
    refuses_until_the_operator_turns_deletion_on,
    a_new_repository_has_deletion_off,
    one_sweep_per_repository,
    the_store_lease_stops_a_second_process,
);

// ---------------------------------------------------------------------------
// Collection, grace, deletion, revival.

fn a_closed_draft_is_collected_then_deleted<M: MetaStore + 'static>(w: &World<M>) {
    let (root, a) = draft(w);
    let a_root = w.snapshot_of(a).expect("snapshot");
    let landed = w.snapshot_of(root).expect("snapshot");
    let only_a: Vec<ObjectId> = a_root.difference(&landed).copied().collect();
    assert_eq!(only_a.len(), 2, "a's root tree and blob a1");
    w.close("w").expect("close");

    // Within the retention window nothing goes.
    w.advance(DAY);
    let early = w.sweep();
    assert_eq!(early.collected, 0, "{early:?}");
    assert!(w.check().is_ok());

    w.advance(RETENTION_PLUS);
    let plan = block_on(w.gc.plan(w.repo, w.now(), &w.shape)).expect("plan");
    let swept = w.sweep();
    assert_eq!(swept.collected, 1, "{swept:?}");
    assert_eq!(swept.doomed, 2, "{swept:?}");
    assert_eq!(swept.deleted, 0, "the grace period holds them");
    assert_eq!(
        plan.report(),
        Report {
            changesets: 1,
            doomed: 2,
            deleted: 0,
            bytes: only_a.iter().map(|o| w.frames.sizes(w.repo)[o]).sum(),
        },
        "the dry run predicted the pass"
    );
    assert_eq!(w.record(a).state, RecordState::Collected);
    let (content, _) = block_on(w.meta.changeset(w.repo, a))
        .expect("read")
        .expect("tombstone");
    assert!(content.is_empty(), "a tombstone keeps no content");
    let bytes: u64 = only_a.iter().map(|o| w.frames.sizes(w.repo)[o]).sum();
    let collected: Vec<String> = journal_kinds(w)
        .into_iter()
        .filter(|(k, _)| k == "Collected")
        .map(|(_, b)| b)
        .collect();
    assert_eq!(
        collected,
        [format!(
            r#"{{"ids":["{a}"],"objects":2,"bytes":{bytes},"moved":0}}"#
        )],
        "Collected carries the doomed frames' bytes"
    );
    for o in &only_a {
        assert!(w.marks(*o).is_some(), "doomed");
        assert!(w.has_frame(*o), "not deleted yet");
    }
    w.check().expect("protection");

    w.advance(GRACE_PLUS);
    let sizes = w.frames.sizes(w.repo);
    let deleted = w.sweep();
    assert_eq!(deleted.deleted, 2, "{deleted:?}");
    assert_eq!(
        deleted.bytes,
        only_a.iter().map(|o| sizes[o]).sum::<u64>(),
        "reclaimed bytes are the deleted frames"
    );
    for o in &only_a {
        assert!(!w.has_frame(*o), "deleted");
        assert!(w.marks(*o).is_none(), "the mark is gone");
    }
    assert_eq!(w.snapshot_of(root).expect("whole"), landed);
    w.check().expect("protection");

    let kinds: Vec<String> = journal_kinds(w).into_iter().map(|(k, _)| k).collect();
    assert_eq!(kinds.iter().filter(|k| *k == "Collected").count(), 1);
    assert_eq!(kinds.iter().filter(|k| *k == "ObjectDeleted").count(), 2);
}

fn revival_during_grace_keeps_the_object<M: MetaStore + 'static>(w: &World<M>) {
    let (root, a) = draft(w);
    let a1 = blob_id(b"a1");
    let a_tree = w.snapshot_of(a).expect("whole");
    w.close("w").expect("close");
    w.advance(RETENTION_PLUS);
    assert_eq!(w.sweep().collected, 1);
    assert!(w.marks(a1).is_some(), "doomed");

    // Within the grace period a new draft names the blob by id, and
    // derives the same root tree: both are claimed and revived.
    w.open("w2").expect("open");
    let b = w
        .create(vec![root], vec![put("a", b"a1")], Some("w2"))
        .expect("revives")
        .id;
    assert!(w.marks(a1).is_none(), "revived");
    assert_eq!(a_tree, w.snapshot_of(b).expect("same tree"));
    w.advance(GRACE_PLUS);
    let swept = w.sweep();
    assert_eq!(swept.deleted, 0, "{swept:?}");
    assert!(w.has_frame(a1));
    w.snapshot_of(b).expect("b is whole");
    w.check().expect("protection");
}

fn a_deleted_blob_is_missing_until_uploaded_again<M: MetaStore + 'static>(w: &World<M>) {
    let (root, _) = draft(w);
    let a1 = blob_id(b"a1");
    w.close("w").expect("close");
    w.advance(RETENTION_PLUS);
    w.sweep();
    w.advance(GRACE_PLUS);
    assert_eq!(w.sweep().deleted, 2);
    assert!(!w.has_frame(a1));

    w.open("w2").expect("open");
    let missing = w.create(vec![root], vec![put("a", b"a1")], Some("w2"));
    assert!(
        matches!(&missing, Err(ApiError::MissingObjects { ids }) if ids.contains(&a1)),
        "{missing:?}"
    );
    w.upload(&[b"a1"]).expect("re-upload");
    let b = w
        .create(vec![root], vec![put("a", b"a1")], Some("w2"))
        .expect("after the re-upload")
        .id;
    assert!(w.snapshot_of(b).expect("whole").contains(&a1));
    w.check().expect("protection");
}

fn an_unclaimed_upload_is_collected<M: MetaStore + 'static>(w: &World<M>) {
    w.seed();
    let id = w.upload(&[b"stray"]).expect("upload")[0];
    w.advance(23 * HOUR);
    assert_eq!(w.sweep().doomed, 0, "the upload lease holds it");
    w.advance(2 * HOUR);
    let swept = w.sweep();
    assert_eq!(swept.doomed, 1, "{swept:?}");
    assert!(swept.expired >= 1);
    w.advance(GRACE_PLUS);
    assert_eq!(w.sweep().deleted, 1);
    assert!(!w.has_frame(id));
    w.check().expect("protection");
}

on_both_stores!(
    collection:
    a_closed_draft_is_collected_then_deleted,
    revival_during_grace_keeps_the_object,
    a_deleted_blob_is_missing_until_uploaded_again,
    an_unclaimed_upload_is_collected,
);

// ---------------------------------------------------------------------------
// Pins, candidates, lapses, successors.

/// `a` in `w1`, and `b` in `w2` merging it, so `b` pins `a`.
fn parent_and_child<M: MetaStore + 'static>(w: &World<M>) -> (ChangesetId, ChangesetId) {
    let root = w.seed();
    w.open("w1").expect("open");
    let a = w.commit(vec![root], "a", b"a1", Some("w1")).expect("a").id;
    w.open("w2").expect("open");
    let b = w
        .commit(vec![root, a], "b", b"b1", Some("w2"))
        .expect("b")
        .id;
    assert_eq!(w.protection(a).pins, 1);
    (a, b)
}

fn a_parent_with_a_live_child_is_never_collected<M: MetaStore + 'static>(w: &World<M>) {
    let (a, b) = parent_and_child(w);
    w.close("w1").expect("close");
    // Past a's expiry. w2's lease has lapsed, so the pass abandons b, but b
    // expires a retention after its lease ended: later than a.
    w.advance(7 * DAY + HOUR);
    let swept = w.sweep();
    assert_eq!((swept.collected, swept.abandoned), (0, 1), "{swept:?}");
    assert_eq!(w.record(a).state, RecordState::Abandoned);
    assert_eq!(w.record(b).state, RecordState::Abandoned);
    let gauges = block_on(w.gc.gauges(w.repo, w.now())).expect("gauges");
    assert_eq!(gauges.pinned, 1, "{gauges:?}");
    w.check().expect("protection");

    // The child goes, and its parent with it in the same pass.
    w.advance(RETENTION_PLUS);
    let swept = w.sweep();
    assert_eq!(swept.collected, 2, "{swept:?}");
    assert_eq!(w.record(a).state, RecordState::Collected);
    assert_eq!(w.record(b).state, RecordState::Collected);
    assert_eq!(
        block_on(w.gc.gauges(w.repo, w.now())).expect("gauges"),
        Gauges {
            doomed: swept.doomed,
            ..Gauges::default()
        }
    );
    w.check().expect("protection");
}

fn a_crashed_land_candidate_is_collected_after_its_lease<M: MetaStore + 'static>(w: &World<M>) {
    let (root, a) = draft(w);
    // The line moves under the draft, so the land writes a rebased
    // candidate; the server dies before its CAS.
    let moved = w.commit(vec![root], "b", b"b1", None).expect("b").id;
    w.update_ref(Some(root), moved).expect("move");
    w.engine_ctl
        .fail_ref_moves
        .store(true, std::sync::atomic::Ordering::SeqCst);
    let crashed = w.land(a);
    w.engine_ctl
        .fail_ref_moves
        .store(false, std::sync::atomic::Ordering::SeqCst);
    assert!(crashed.is_err(), "{crashed:?}");
    let drafts =
        block_on(w.meta.list_records(w.repo, RecordState::Draft, None, 100)).expect("list");
    let candidates: Vec<ChangesetId> = drafts
        .iter()
        .filter(|(_, r)| r.holder.as_deref().is_some_and(|h| h.starts_with("land:")))
        .map(|(id, _)| *id)
        .collect();
    assert_eq!(candidates.len(), 1, "{drafts:?}");
    let candidate = candidates[0];
    let tree = w.snapshot_of(candidate).expect("whole");

    w.advance(5 * MINUTE);
    assert_eq!(w.sweep().collected, 0, "the lease holds it");
    w.advance(6 * MINUTE);
    let swept = w.sweep();
    assert_eq!(swept.collected, 1, "{swept:?}");
    assert_eq!(w.record(candidate).state, RecordState::Collected);
    assert_eq!(w.record(a).state, RecordState::Draft, "the draft stays");
    w.advance(GRACE_PLUS);
    w.sweep();
    w.snapshot_of(a).expect("the draft is whole");
    w.snapshot_of(moved).expect("the line is whole");
    assert!(
        tree.iter().any(|o| !w.has_frame(*o)),
        "the candidate's own tree was deleted"
    );
    w.check().expect("protection");
    // Onto the same tip, a land derives the same candidate id, which is
    // now a tombstone: the land writes it again (a revival) and lands,
    // without waiting for the line to move.
    let same_tip = w.land(a);
    assert!(
        matches!(same_tip, Ok(Landed::Landed { .. })),
        "{same_tip:?}"
    );
    assert_eq!(w.record(candidate).state, RecordState::Landed);
    w.snapshot_of(candidate)
        .expect("the revived candidate is whole");
    w.check().expect("protection");
}

fn the_lapse_step_closes_lapsed_workspaces<M: MetaStore + 'static>(w: &World<M>) {
    let (_, a) = draft(w);
    let lease_until = block_on(w.meta.workspace(w.repo, "w"))
        .expect("read")
        .expect("open")
        .lease_until_ms;
    w.advance(3 * HOUR);
    let swept = w.sweep();
    assert_eq!((swept.lapsed, swept.abandoned), (1, 1), "{swept:?}");
    assert!(
        block_on(w.meta.workspace(w.repo, "w"))
            .expect("read")
            .is_none()
    );
    assert_eq!(w.record(a).state, RecordState::Abandoned);
    assert_eq!(
        w.protection(a).expires_at_ms,
        Some(lease_until + 7 * DAY),
        "expires a retention after the lease ended"
    );
    let closed: Vec<String> = journal_kinds(w)
        .into_iter()
        .filter(|(k, _)| k == "WorkspaceClosed")
        .map(|(_, b)| b)
        .collect();
    assert_eq!(
        closed,
        [r#"{"name":"w","reason":"lease-expired","abandoned":1}"#]
    );
    w.check().expect("protection");
}

/// The engine ending a lapsed workspace lazily (a commit to it) races the
/// sweep's lapse step: whatever the interleaving, it is closed once.
#[test]
fn a_lazy_close_and_the_lapse_step_close_a_workspace_once() {
    let schedules: [&[bool]; 6] = [
        &[true],
        &[false],
        &[true, false],
        &[false, true],
        &[false, false, false, true],
        &[true, true, true, false],
    ];
    for schedule in schedules {
        let w = memory();
        let (root, a) = draft(&w);
        w.advance(3 * HOUR);
        w.upload(&[b"late"]).expect("upload");
        w.yields(true);
        let commit = w.engine.create_changeset(
            &w.cx,
            w.request(vec![a], vec![put("late", b"late")], Some("w")),
        );
        let sweep = w.gc.sweep(w.repo, &w.shape, false);
        let (committed, swept) = interleave(commit, sweep, schedule);
        w.yields(false);
        assert!(
            matches!(
                committed,
                Err(ApiError::LeaseExpired { .. } | ApiError::NotFound { .. })
            ),
            "{schedule:?}: {committed:?}"
        );
        swept.expect("sweep");
        let closed = journal_kinds(&w)
            .into_iter()
            .filter(|(k, _)| k == "WorkspaceClosed")
            .count();
        assert_eq!(closed, 1, "{schedule:?}");
        assert_eq!(w.record(a).state, RecordState::Abandoned);
        w.snapshot_of(root).expect("whole");
        w.check().expect("protection");
    }
}

fn the_successor_keeps_a_superseded_records_objects<M: MetaStore + 'static>(w: &World<M>) {
    let (root, a) = draft(w);
    let moved = w.commit(vec![root], "b", b"b1", None).expect("b").id;
    w.update_ref(Some(root), moved).expect("move");
    let s = match w.land(a).expect("land") {
        Landed::Landed { landed, .. } => landed,
        other => panic!("{other:?}"),
    };
    assert_ne!(s, a);
    assert_eq!(w.record(a).state, RecordState::Superseded);
    assert_eq!(w.record(a).superseded_by, Some(s));
    // A write path that missed the successor's row for `a1` (a bug): the
    // successor rule is the second line of defence.
    let a1 = blob_id(b"a1");
    commit_raw(
        &*w.meta,
        w.repo,
        vec![Write::RemoveHolder {
            object: a1,
            holder: s.to_string(),
        }],
    );
    w.advance(RETENTION_PLUS);
    let swept = w.sweep();
    assert_eq!(swept.collected, 1, "{swept:?}");
    assert_eq!(swept.moved, 1, "a1 moved to its successor");
    assert!(w.holders_of(a1).contains(&s.to_string()));
    assert!(w.marks(a1).is_none());
    w.advance(GRACE_PLUS);
    w.sweep();
    w.snapshot_of(s).expect("the successor is whole");
    w.check().expect("protection");
}

/// A stack longer than `max_stack` is closed in parts, every draft of it
/// abandoned, across passes when the budget runs out part-way.
fn a_long_stack_is_closed_whole(budget: u32) {
    let w = World::new(
        std::sync::Arc::new(graft_store::memory::MemoryStore::new()),
        Frames::default(),
        None,
        graft_gc::GcConfig {
            max_stack: 2,
            pass_budget: budget,
            ..gc_config()
        },
    );
    let root = w.seed();
    w.open("w").expect("open");
    let mut stack = Vec::new();
    let mut at = root;
    for i in 0..5 {
        let data = format!("d{i}");
        at = w
            .commit(vec![at], &format!("f{i}"), data.as_bytes(), Some("w"))
            .expect("draft")
            .id;
        stack.push(at);
    }
    w.advance(3 * HOUR);
    let mut total = graft_gc::SweepReport::default();
    for _ in 0..8 {
        let r = w.sweep();
        add(&mut total, &r);
        if !r.more {
            break;
        }
    }
    assert_eq!((total.lapsed, total.abandoned), (1, 5), "{total:?}");
    for id in &stack {
        assert_eq!(w.record(*id).state, RecordState::Abandoned, "{id}");
    }
    assert!(
        block_on(w.meta.workspace(w.repo, "w"))
            .expect("read")
            .is_none()
    );
    let parts: Vec<(String, String)> = journal_kinds(&w)
        .into_iter()
        .filter(|(k, _)| k.starts_with("Workspace") && k != "WorkspaceOpened")
        .collect();
    assert_eq!(
        parts,
        [
            (
                "WorkspaceLapsing".to_owned(),
                r#"{"name":"w","abandoned":2}"#.to_owned()
            ),
            (
                "WorkspaceLapsing".to_owned(),
                r#"{"name":"w","abandoned":2}"#.to_owned()
            ),
            (
                "WorkspaceClosed".to_owned(),
                r#"{"name":"w","reason":"lease-expired","abandoned":1}"#.to_owned()
            ),
        ]
    );
    w.check().expect("protection");
}

#[test]
fn a_long_stack_is_closed_whole_in_one_pass() {
    a_long_stack_is_closed_whole(10_000);
}

#[test]
fn a_long_stack_is_closed_whole_across_passes() {
    a_long_stack_is_closed_whole(2);
}

on_both_stores!(
    pins:
    a_parent_with_a_live_child_is_never_collected,
    a_crashed_land_candidate_is_collected_after_its_lease,
    the_lapse_step_closes_lapsed_workspaces,
    the_successor_keeps_a_superseded_records_objects,
);

// ---------------------------------------------------------------------------
// The dry run and --verify.

fn verify_catches_a_missing_pin<M: MetaStore + 'static>(w: &World<M>) {
    let (a, b) = parent_and_child(w);
    commit_raw(&*w.meta, w.repo, vec![Write::SetPins { id: a, pins: 0 }]);
    w.close("w1").expect("close");
    w.advance(7 * DAY + HOUR);
    let v = block_on(w.gc.verify(w.repo, w.now(), &w.shape)).expect("verify");
    assert!(!v.matches());
    assert_eq!(v.unsafe_changesets, [a]);
    let refused = block_on(w.gc.sweep(w.repo, &w.shape, true));
    assert!(
        matches!(&refused, Err(GcError::Mismatch { changesets, .. }) if changesets == &[a]),
        "{refused:?}"
    );
    assert_eq!(w.record(a).state, RecordState::Abandoned, "nothing swept");
    assert_eq!(
        w.record(b).state,
        RecordState::Draft,
        "the lapse did not run"
    );
}

fn verify_catches_a_missing_row<M: MetaStore + 'static>(w: &World<M>) {
    let root = w.seed();
    w.open("w").expect("open");
    let b = w.commit(vec![root], "x", b"x1", Some("w")).expect("b").id;
    let x1 = blob_id(b"x1");
    commit_raw(
        &*w.meta,
        w.repo,
        vec![Write::RemoveHolder {
            object: x1,
            holder: b.to_string(),
        }],
    );
    w.advance(25 * HOUR);
    let v = block_on(w.gc.verify(w.repo, w.now(), &w.shape)).expect("verify");
    assert_eq!(v.unsafe_objects, [x1], "{v:?}");
    assert!(matches!(
        block_on(w.gc.sweep(w.repo, &w.shape, true)),
        Err(GcError::Mismatch { .. })
    ));
    assert!(w.marks(x1).is_none(), "nothing swept");
}

fn verify_passes_on_what_the_engine_writes<M: MetaStore + 'static>(w: &World<M>) {
    let (a, _) = parent_and_child(w);
    w.close("w1").expect("close");
    w.advance(7 * DAY + HOUR);
    let v = block_on(w.gc.verify(w.repo, w.now(), &w.shape)).expect("verify");
    assert!(v.matches(), "{v:?}");
    assert!(!v.plan.changesets.contains(&a));
    let swept = block_on(w.gc.sweep(w.repo, &w.shape, true)).expect("verified sweep");
    assert_eq!(swept.collected as usize, v.plan.changesets.len());
}

on_both_stores!(
    verify:
    verify_catches_a_missing_pin,
    verify_catches_a_missing_row,
    verify_passes_on_what_the_engine_writes,
);

// ---------------------------------------------------------------------------
// Cost.

/// The reads of a sweep that collects a three-draft stack, over a line of
/// `depth` landed changesets.
fn collection_reads(depth: usize) -> (u64, u64, u64) {
    let w = memory();
    let mut tip = w.seed();
    for i in 0..depth {
        let c = w
            .commit(vec![tip], "h", format!("h{i}").as_bytes(), None)
            .expect("history")
            .id;
        w.update_ref(Some(tip), c).expect("land");
        tip = c;
    }
    // Let the history's uploads expire and go first.
    w.advance(2 * DAY);
    w.drain();

    w.open("w").expect("open");
    let mut head = tip;
    for i in 0..3 {
        head = w
            .commit(
                vec![head],
                &format!("s/{i}"),
                format!("s{i}").as_bytes(),
                Some("w"),
            )
            .expect("stack")
            .id;
    }
    w.close("w").expect("close");
    w.advance(RETENTION_PLUS);
    w.gc_ctl.reset_counts();
    let swept = w.sweep();
    assert_eq!(swept.collected, 3, "{swept:?}");
    (w.gc_ctl.reads(), w.gc_ctl.object_reads(), swept.doomed)
}

#[test]
fn collection_reads_follow_the_changes_not_the_history() {
    let shallow = collection_reads(3);
    let deep = collection_reads(60);
    assert_eq!(shallow, deep, "(meta reads, object reads, doomed)");
    // A handful of reads per record collected and per object doomed.
    let (reads, object_reads, doomed) = deep;
    assert!(reads <= 3 * 12 + doomed * 4 + 20, "{reads} metadata reads");
    assert!(object_reads <= doomed * 2, "{object_reads} object reads");
}
