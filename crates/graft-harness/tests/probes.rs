//! Hand-placed probes: the exact-tick lease boundaries, and what a client
//! sees after a lost land ack and a sync, and racing merges of adjacent
//! lines, a delete against an edit, and binary files.

use graft_harness::probe::{lease_boundary, racing_merges, sync_after_lost_ack};

/// At the exact end of a workspace lease and of a land-attempt lease,
/// nothing held is collected early and a retried land still lands.
#[test]
fn exact_tick_lease_boundaries() {
    let f = lease_boundary(None);
    for n in &f.notes {
        eprintln!("{n}");
    }
    assert!(f.failures.is_empty(), "{:#?}", f.failures);
}

/// A lost ack, a sync, and a land again return the first land.
#[test]
fn sync_after_a_lost_ack_returns_the_first_land() {
    let f = sync_after_lost_ack();
    for n in &f.notes {
        eprintln!("{n}");
    }
    assert!(f.failures.is_empty(), "{:#?}", f.failures);
}

/// Adjacent lines, a delete against an edit, and binary files: the second
/// land merges cleanly or conflicts, never silently.
#[test]
fn racing_merges_land_cleanly_or_conflict() {
    let f = racing_merges();
    for n in &f.notes {
        eprintln!("{n}");
    }
    assert!(f.failures.is_empty(), "{:#?}", f.failures);
}
