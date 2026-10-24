//! The harness bites (§3.4): each planted bug is caught within the seed
//! budget, the failing seed replays to the same failure, and the plan
//! shrinks.

use std::rc::Rc;

use graft_engine::{PlantedBug, PlantedBugs};
use graft_harness::faulty::{FaultRates, WrapperBug};
use graft_harness::scenario::{EngineHook, Plan, Shape};
use graft_harness::shrink::{explain, replay_command, replay_seed, run_and_check, shrink};

/// A hook that plants `bug` on a fresh handle for each run.
fn planting(bug: Option<PlantedBug>) -> impl Fn() -> Option<EngineHook> {
    move || {
        let bug = bug?;
        let bugs = PlantedBugs::default();
        bugs.plant(bug);
        let hook: EngineHook = Rc::new(move |e| e.with_planted_bugs(bugs.clone()));
        Some(hook)
    }
}

/// Runs seeds until one fails; checks it replays, shrinks it, and returns
/// the properties it broke. With `GRAFT_HARNESS_SEED` set (the printed
/// replay command) it runs that seed only.
fn caught(
    name: &str,
    shape: &Shape,
    wrapper: &[WrapperBug],
    engine: Option<PlantedBug>,
    seeds: u64,
) -> Vec<&'static str> {
    let hook = planting(engine);
    let range = match replay_seed() {
        Some(s) => s..=s,
        None => 1..=seeds,
    };
    for seed in range.clone() {
        let mut plan = Plan::generate(seed, shape);
        plan.bugs = wrapper.to_vec();
        let (report, _) = run_and_check(&plan, hook());
        if report.passed() {
            continue;
        }
        let (again, _) = run_and_check(&plan, hook());
        assert_eq!(
            again.failures, report.failures,
            "{name}: seed {seed} replays"
        );
        let shrunk = shrink(&plan, &hook, 60);
        eprintln!(
            "{name} ({engine:?}) caught:\n{}",
            explain(
                seed,
                &replay_command("planted", name, seed),
                wrapper,
                &shrunk
            )
        );
        let mut props: Vec<&'static str> = report.failures.iter().map(|f| f.property).collect();
        props.dedup();
        return props;
    }
    panic!("{name}: no seed in {range:?} caught the planted bug");
}

fn racing() -> Shape {
    Shape {
        clients: 5,
        intents: 4,
        events: 0,
        rates: FaultRates::delays(),
    }
}

#[test]
fn a_ref_write_that_skips_the_cas_is_caught() {
    let props = caught(
        "a_ref_write_that_skips_the_cas_is_caught",
        &racing(),
        &[WrapperBug::NonCasRef],
        None,
        20,
    );
    assert!(
        props
            .iter()
            .any(|p| p.starts_with('3') || p.starts_with('1')),
        "{props:?}"
    );
}

#[test]
fn an_acknowledged_put_that_was_discarded_is_caught() {
    let props = caught(
        "an_acknowledged_put_that_was_discarded_is_caught",
        &racing(),
        &[WrapperBug::DroppedPut],
        None,
        20,
    );
    assert!(props.contains(&"5 objects before metadata"), "{props:?}");
}

#[test]
fn a_skipped_idempotency_check_is_caught() {
    let props = caught(
        "a_skipped_idempotency_check_is_caught",
        &racing(),
        &[],
        Some(PlantedBug::SkipIdempotency),
        20,
    );
    assert!(props.contains(&"1 no lost or duplicated land"), "{props:?}");
}

#[test]
fn metadata_before_objects_is_caught() {
    let shape = Shape {
        rates: FaultRates {
            crash: 0.08,
            ..FaultRates::hostile()
        },
        ..Shape::default()
    };
    let props = caught(
        "metadata_before_objects_is_caught",
        &shape,
        &[],
        Some(PlantedBug::MetadataBeforeObjects),
        60,
    );
    assert!(props.contains(&"5 objects before metadata"), "{props:?}");
}

#[test]
fn gc_ignoring_the_land_holder_is_caught() {
    let props = caught(
        "gc_ignoring_the_land_holder_is_caught",
        &Shape::default(),
        &[],
        Some(PlantedBug::IgnoreLandHolder),
        60,
    );
    assert!(props.contains(&"6 GC is safe"), "{props:?}");
}

/// The lease end compared one tick early is caught by the exact-tick probe.
#[test]
fn a_lease_boundary_one_tick_early_is_caught() {
    let clean = graft_harness::probe::lease_boundary(None);
    assert!(clean.failures.is_empty(), "{:#?}", clean.failures);
    let f = graft_harness::probe::lease_boundary(planting(Some(PlantedBug::LeaseBoundary))());
    eprintln!("lease boundary probe with the bug: {:#?}", f.failures);
    assert!(
        f.failures.iter().any(|x| x.contains("lease end")),
        "{:#?}",
        f.failures
    );
}

/// The probe catches the sweep ignoring the land holder too, at one tick.
#[test]
fn the_probe_catches_gc_ignoring_the_land_holder() {
    let f = graft_harness::probe::lease_boundary(planting(Some(PlantedBug::IgnoreLandHolder))());
    eprintln!(
        "lease boundary probe with IgnoreLandHolder: {:#?}",
        f.failures
    );
    assert!(
        f.failures.iter().any(|x| x.contains("6 GC is safe")),
        "{:#?}",
        f.failures
    );
}
