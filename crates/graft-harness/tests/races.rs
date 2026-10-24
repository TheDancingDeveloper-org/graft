//! Racing lands under injected faults, at fixed seeds (the merge-queue
//! budget, §3.5). A failure prints its seed, a replay command and a shrunk
//! history.

use graft_harness::faulty::FaultRates;
use graft_harness::scenario::{Plan, Shape};
use graft_harness::shrink::{explain, replay_command, replay_seed, run_and_check, shrink};

fn assert_passes(seed: u64, shape: &Shape) -> graft_harness::Report {
    let plan = Plan::generate(seed, shape);
    let (report, _) = run_and_check(&plan, None);
    if !report.passed() {
        let shrunk = shrink(&plan, &|| None, 150);
        panic!(
            "{}",
            explain(seed, &replay_command("races", "replay", seed), &[], &shrunk)
        );
    }
    report
}

/// Clients racing with delays only: everything lands or conflicts cleanly.
#[test]
fn interleaving_only() {
    let shape = Shape {
        rates: FaultRates::delays(),
        events: 0,
        ..Shape::default()
    };
    for seed in 1..=10 {
        let r = assert_passes(seed, &shape);
        assert_eq!(r.gave_up, 0, "seed {seed}: {r:?}");
        assert_eq!(r.landed_intents, 16, "seed {seed}: {r:?}");
    }
}

/// N clients x M faults: errors, lost acks, crashes at commits, lease
/// expiry, GC mid-land and a full disk.
#[test]
fn hostile_fixed_seeds() {
    let shape = Shape::default();
    let mut totals = graft_harness::Report::default();
    for seed in 1..=120 {
        let r = assert_passes(seed, &shape);
        totals.acked += r.acked;
        totals.restarts += r.restarts;
        totals.faults += r.faults;
        totals.conflicts += r.conflicts;
        totals.spurious_conflicts += r.spurious_conflicts;
        totals.gc_passes += r.gc_passes;
        totals.collected += r.collected;
        totals.gave_up += r.gave_up;
    }
    eprintln!("hostile totals: {totals:?}");
    assert!(totals.restarts > 0, "the seeds crash the process");
    assert!(totals.faults > 0 && totals.gc_passes > 0, "{totals:?}");
}

/// More clients and intents, fewer seeds.
#[test]
fn hostile_wide() {
    let shape = Shape {
        clients: 8,
        intents: 5,
        events: 30,
        ..Shape::default()
    };
    for seed in 1000..1010 {
        assert_passes(seed, &shape);
    }
}

/// The same seed gives the same history.
#[test]
fn a_seed_replays_exactly() {
    for seed in [3, 11, 16, 40] {
        let plan = Plan::generate(seed, &Shape::default());
        let (a, ha) = run_and_check(&plan, None);
        let (b, hb) = run_and_check(&plan, None);
        assert_eq!(ha, hb, "seed {seed}");
        assert_eq!(a.failures, b.failures);
    }
}

/// Replays `GRAFT_HARNESS_SEED` (from a failure message); a no-op without it.
#[test]
fn replay() {
    let Some(seed) = replay_seed() else {
        return;
    };
    let plan = Plan::generate(seed, &Shape::default());
    let (report, history) = run_and_check(&plan, None);
    for op in &history {
        eprintln!("{op:?}");
    }
    assert!(report.passed(), "seed {seed}: {:#?}", report.failures);
}

/// On-demand soak: `GRAFT_HARNESS_SOAK=<seeds>` runs that many seeds from
/// `GRAFT_HARNESS_SEED` (default 1); a no-op without it.
#[test]
fn soak() {
    let Some(n) = std::env::var("GRAFT_HARNESS_SOAK")
        .ok()
        .and_then(|s| s.parse::<u64>().ok())
    else {
        return;
    };
    let from = replay_seed().unwrap_or(1);
    for seed in from..from + n {
        assert_passes(seed, &Shape::default());
    }
}
