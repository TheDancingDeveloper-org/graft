//! Seeds, replay and shrinking (§2): a failing run is cut down by dropping
//! events, intents, clients and faults while it still fails.

use crate::check::{Report, check};
use crate::faulty::WrapperBug;
use crate::scenario::{EngineHook, Op, Plan, run};

/// The environment variable a replay reads its seed from.
pub const SEED_ENV: &str = "GRAFT_HARNESS_SEED";

/// The one-line command that replays `seed` through `test`.
pub fn replay_command(test_target: &str, test: &str, seed: u64) -> String {
    format!(
        "{SEED_ENV}={seed} cargo test -p graft-harness --test {test_target} -- --exact {test} --nocapture"
    )
}

/// The seed to replay, when one is set.
pub fn replay_seed() -> Option<u64> {
    std::env::var(SEED_ENV).ok()?.parse().ok()
}

/// Runs and checks `plan`.
pub fn run_and_check(plan: &Plan, hook: Option<EngineHook>) -> (Report, Vec<Op>) {
    let out = run(plan, hook);
    let report = check(plan, &out);
    (report, out.history)
}

/// The smallest plan found that still fails, with its report and history.
#[derive(Debug)]
pub struct Shrunk {
    /// The plan.
    pub plan: Plan,
    /// Its report.
    pub report: Report,
    /// Its history.
    pub history: Vec<Op>,
    /// Runs the shrinker spent.
    pub runs: usize,
}

/// Shrinks a failing `plan`, spending at most `budget` runs. `hook` must
/// build the same engine for every run (a fresh planted-bugs handle each
/// time if it plants one).
pub fn shrink(plan: &Plan, hook: &dyn Fn() -> Option<EngineHook>, budget: usize) -> Shrunk {
    let (report, history) = run_and_check(plan, hook());
    assert!(!report.passed(), "shrink a failing plan");
    let mut best = Shrunk {
        plan: plan.clone(),
        report,
        history,
        runs: 1,
    };
    let try_plan = |candidate: Plan, best: &mut Shrunk| -> bool {
        if best.runs >= budget {
            return false;
        }
        best.runs += 1;
        let (report, history) = run_and_check(&candidate, hook());
        if report.passed() {
            return false;
        }
        best.plan = candidate;
        best.report = report;
        best.history = history;
        true
    };
    loop {
        let mut progressed = false;
        // Drop whole clients, from the last.
        let mut c = best.plan.clients.len();
        while c > 1 {
            c -= 1;
            if c < best.plan.clients.len() {
                let mut p = best.plan.clone();
                p.clients.remove(c);
                progressed |= try_plan(p, &mut best);
            }
        }
        // Drop intents, from the end of each client.
        for c in 0..best.plan.clients.len() {
            let mut i = best.plan.clients[c].len();
            while i > 0 {
                i -= 1;
                if i < best.plan.clients[c].len() {
                    let mut p = best.plan.clone();
                    p.clients[c].remove(i);
                    progressed |= try_plan(p, &mut best);
                }
            }
        }
        // Drop adversary events.
        let mut e = best.plan.events.len();
        while e > 0 {
            e -= 1;
            if e < best.plan.events.len() {
                let mut p = best.plan.clone();
                p.events.remove(e);
                progressed |= try_plan(p, &mut best);
            }
        }
        // Switch off the faults that fired, one at a time.
        let fired: Vec<u64> = {
            let out = run(&best.plan, hook());
            out.world.faults.fired().iter().map(|f| f.key).collect()
        };
        for key in fired {
            if best.plan.disabled.contains(&key) {
                continue;
            }
            let mut p = best.plan.clone();
            p.disabled.insert(key);
            progressed |= try_plan(p, &mut best);
        }
        if !progressed || best.runs >= budget {
            return best;
        }
    }
}

/// A failure message: the seed, the replay command, the broken properties
/// and the shrunk history.
pub fn explain(seed: u64, replay: &str, bugs: &[WrapperBug], shrunk: &Shrunk) -> String {
    let mut out = format!(
        "adversarial harness: seed {seed} failed (wrapper bugs {bugs:?})\nreplay: {replay}\n"
    );
    for f in &shrunk.report.failures {
        out.push_str(&format!("  {f}\n"));
    }
    out.push_str(&format!(
        "shrunk in {} runs to {} clients, {} intents, {} events, {} faults switched off:\n",
        shrunk.runs,
        shrunk.plan.clients.len(),
        shrunk.plan.clients.iter().map(Vec::len).sum::<usize>(),
        shrunk.plan.events.len(),
        shrunk.plan.disabled.len()
    ));
    for op in &shrunk.history {
        out.push_str(&format!("  {op:?}\n"));
    }
    out
}
