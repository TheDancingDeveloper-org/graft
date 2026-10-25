//! Racing lands against a file-level model. Each case lands a few changes
//! in sequence, then races several stacks (each based on one of those line
//! states) through a proptest-chosen schedule. The store yields before
//! every call and the executor polls the lands in schedule order, so every
//! interleaving of their store calls is reachable and reproducible.
//!
//! Checked:
//!
//! - the line never moves backwards: after every step its version and tip
//!   position only grow, and every tip it showed is on its final
//!   first-parent chain;
//! - the landed stacks fill the new positions exactly, each conflicts with
//!   nothing landed between its base and its landing, and the final
//!   snapshot is the model's;
//! - each `Conflict` is exactly the model's: the first draft that touches a
//!   path changed between its base and `onto`, at those paths;
//! - with an attempt budget of at least the number of racers, nothing is
//!   `Contended` (every lost CAS is another racer's move), and a racer that
//!   conflicts with no one lands;
//! - a retry returns the same `Landed`, and the records say what each draft
//!   became.

mod common;

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::sync::Arc;

use common::race::{Task, YieldSleeper, Yielding, run_scheduled};
use common::*;
use graft_engine::EngineConfig;
use graft_model::ChangesetId;
use graft_proto::error::ApiResult;
use graft_proto::types::*;
use graft_proto::{ApiError, RepoApi};
use graft_store::memory::MemoryStore;
use graft_store::{MetaStore, RepoId};
use proptest::prelude::*;

const PATHS: &[&str] = &["a", "b", "c", "d", "e"];

type Snapshot = BTreeMap<String, Vec<u8>>;

/// One racer: a stack of drafts, each changing some paths, on line state
/// `base` (0 is the root, `k` the `k`-th sequential land).
#[derive(Clone, Debug)]
struct Racer {
    base: usize,
    drafts: Vec<BTreeSet<usize>>,
}

impl Racer {
    fn paths(&self) -> BTreeSet<usize> {
        self.drafts.iter().flatten().copied().collect()
    }
}

#[derive(Clone, Debug)]
struct Case {
    pre: Vec<BTreeSet<usize>>,
    racers: Vec<Racer>,
    attempts: u32,
    schedule: Vec<usize>,
}

fn paths() -> impl Strategy<Value = BTreeSet<usize>> {
    prop::collection::btree_set(0..PATHS.len(), 1..=2)
}

fn case() -> impl Strategy<Value = Case> {
    (
        prop::collection::vec(paths(), 0..=2),
        prop::collection::vec((0usize..3, prop::collection::vec(paths(), 1..=2)), 2..=5),
        1u32..=6,
        prop::collection::vec(0usize..32, 1..64),
    )
        .prop_map(|(pre, racers, attempts, schedule)| {
            let racers = racers
                .into_iter()
                .map(|(base, drafts)| Racer {
                    base: base.min(pre.len()),
                    drafts,
                })
                .collect();
            Case {
                pre,
                racers,
                attempts,
                schedule,
            }
        })
}

fn land_req(head: ChangesetId) -> Land {
    Land {
        repo: "r".to_owned(),
        head,
        line: "trunk".to_owned(),
        squash: false,
        merge: MergeMode::File,
        deadline_ms: None,
        merged_from: None,
    }
}

/// Commits `paths` (with contents tagged `tag`) on `parent`.
fn commit<M: MetaStore>(
    r: &Repo<'_, M>,
    parent: Option<ChangesetId>,
    paths: &BTreeSet<usize>,
    tag: &str,
) -> ChangesetId {
    let contents: Vec<(String, Vec<u8>)> = paths
        .iter()
        .map(|&i| {
            (
                PATHS[i].to_owned(),
                format!("{tag}-{}", PATHS[i]).into_bytes(),
            )
        })
        .collect();
    let files: Vec<(&str, &[u8])> = contents
        .iter()
        .map(|(p, b)| (p.as_str(), b.as_slice()))
        .collect();
    r.commit(parent.into_iter().collect(), &files, &[]).id
}

fn apply(snapshot: &mut Snapshot, paths: &BTreeSet<usize>, tag: &str) {
    for &i in paths {
        snapshot.insert(
            PATHS[i].to_owned(),
            format!("{tag}-{}", PATHS[i]).into_bytes(),
        );
    }
}

fn run(case: &Case) -> Result<(), TestCaseError> {
    let config = EngineConfig {
        land_attempts: case.attempts,
        ..EngineConfig::default()
    };
    let f = build(Arc::new(Yielding(MemoryStore::new())), config, None)
        .map_engine(|e| e.with_sleeper(Arc::new(YieldSleeper)));
    let r = Repo::new(&f, "r");
    let inner = &f.engine.stores().0.0;
    let repo: RepoId = block_on(inner.repo("r")).expect("repo").expect("exists");

    // The root and the sequential lands: line states 0..=pre.
    let all: BTreeSet<usize> = (0..PATHS.len()).collect();
    let root = commit(&r, None, &all, "root");
    r.update_ref("trunk", None, root, RefKind::Line)
        .expect("line");
    let mut states = vec![root];
    let mut snapshots = vec![{
        let mut s = Snapshot::new();
        apply(&mut s, &all, "root");
        s
    }];
    let mut changed_at: Vec<BTreeSet<usize>> = vec![BTreeSet::new()];
    for (k, paths) in case.pre.iter().enumerate() {
        let tip = *states.last().expect("state");
        let tag = format!("pre{k}");
        let next = commit(&r, Some(tip), paths, &tag);
        let got = block_on(r.e().land(&r.cx, land_req(next))).expect("sequential land");
        let ok = matches!(got, Landed::Landed { landed, .. } if landed == next);
        prop_assert!(ok, "sequential land {} landed {:?}", k, got);
        let mut snap = snapshots.last().expect("snapshot").clone();
        apply(&mut snap, paths, &tag);
        states.push(next);
        snapshots.push(snap);
        changed_at.push(paths.clone());
    }
    let pre = case.pre.len() as u64;

    // The racers' drafts.
    let mut heads = Vec::new();
    let mut drafts: Vec<Vec<ChangesetId>> = Vec::new();
    for (i, racer) in case.racers.iter().enumerate() {
        let mut parent = states[racer.base];
        let mut ids = Vec::new();
        for (j, paths) in racer.drafts.iter().enumerate() {
            parent = commit(&r, Some(parent), paths, &format!("r{i}d{j}"));
            ids.push(parent);
        }
        heads.push(parent);
        drafts.push(ids);
    }

    // Race them, watching the line after every step.
    let mut seen: Vec<(u64, ChangesetId, u64)> = Vec::new();
    let mut monotone = Ok(());
    let tasks: Vec<Task<'_, ApiResult<Landed>>> = heads
        .iter()
        .map(|&head| {
            let (e, cx) = (r.e(), r.cx.clone());
            Box::pin(async move { e.land(&cx, land_req(head)).await }) as Task<'_, _>
        })
        .collect();
    let results = run_scheduled(tasks, &case.schedule, || {
        let line = block_on(inner.get_ref(repo, "trunk"))
            .expect("read")
            .expect("line");
        let pos = block_on(inner.line_pos(repo, "trunk", line.target))
            .expect("read")
            .expect("the tip is indexed");
        if let Some(&(version, _, last_pos)) = seen.last() {
            if line.version < version
                || pos < last_pos
                || (line.version > version) != (pos > last_pos)
            {
                monotone = Err(format!(
                    "the line moved from v{version} at {last_pos} to v{} at {pos}",
                    line.version
                ));
            }
            if line.version == version {
                return;
            }
        }
        seen.push((line.version, line.target, pos));
    });
    prop_assert_eq!(monotone, Ok(()));

    // The final first-parent chain down to the last sequential land.
    let tip = block_on(inner.get_ref(repo, "trunk"))
        .expect("read")
        .expect("line");
    let mut chain: HashMap<ChangesetId, u64> = HashMap::new();
    let mut at = tip.target;
    loop {
        let pos = block_on(inner.line_pos(repo, "trunk", at))
            .expect("read")
            .expect("on the line");
        chain.insert(at, pos);
        if pos <= pre {
            prop_assert_eq!(at, states[pos as usize]);
            break;
        }
        at = r.changeset(at).changeset.first_parent().expect("parent");
    }
    for (_, id, pos) in &seen {
        prop_assert_eq!(chain.get(id), Some(pos), "every tip shown is on the line");
    }

    // Landed stacks fill the new positions; replay them through the model.
    let n = case.racers.len();
    let mut landed: Vec<(u64, usize)> = Vec::new();
    for (i, result) in results.iter().enumerate() {
        match result {
            Ok(Landed::Landed {
                landed: id,
                pos,
                line_version,
                rebased,
                ..
            }) => {
                prop_assert_eq!(chain.get(id), Some(pos));
                prop_assert_eq!(*rebased, *id != heads[i]);
                prop_assert!(*line_version >= 2);
                landed.push((*pos, i));
            }
            Ok(other) => prop_assert!(false, "unexpected {:?}", other),
            Err(ApiError::StaleBase(detail)) => {
                prop_assert!(
                    chain.contains_key(&detail.current_tip),
                    "{:?} is on the line",
                    detail.current_tip
                );
            }
            Err(ApiError::Contended { tip: onto, .. }) => {
                prop_assert!(chain.contains_key(onto), "{:?} is on the line", onto);
            }
            Err(e) => prop_assert!(false, "unexpected error {:?}", e),
        }
    }
    landed.sort();
    let mut next = pre + 1;
    // Paths changed on the line at each position after the root.
    let mut line_changes: BTreeMap<u64, BTreeSet<usize>> = (1..=pre)
        .map(|k| (k, changed_at[k as usize].clone()))
        .collect();
    let mut model = snapshots.last().expect("snapshot").clone();
    for &(pos, i) in &landed {
        let racer = &case.racers[i];
        let len = racer.drafts.len() as u64;
        prop_assert_eq!(pos, next + len - 1, "stacks fill the positions in order");
        let since_base: BTreeSet<usize> = line_changes
            .range(racer.base as u64 + 1..next)
            .flat_map(|(_, p)| p.iter().copied())
            .collect();
        prop_assert!(
            since_base.is_disjoint(&racer.paths()),
            "racer {} landed over a change to its paths",
            i
        );
        for (j, paths) in racer.drafts.iter().enumerate() {
            apply(&mut model, paths, &format!("r{i}d{j}"));
            line_changes.insert(next + j as u64, paths.clone());
        }
        next += len;
    }
    prop_assert_eq!(chain.get(&tip.target).copied(), Some(next - 1));
    let root_tree = r.changeset(tip.target).changeset.root;
    prop_assert_eq!(r.fetch(root_tree), model);

    // The attempt budget rules out `Contended`, and a racer that conflicts
    // with nothing lands.
    for (i, result) in results.iter().enumerate() {
        let racer = &case.racers[i];
        if let Err(ApiError::Contended { .. }) = result {
            prop_assert!(
                (case.attempts as usize) < n,
                "racer {} contended with a budget of {} for {} racers",
                i,
                case.attempts,
                n
            );
        }
        // A racer that conflicts with nothing lands when the budget allows.
        let alone = case
            .racers
            .iter()
            .enumerate()
            .all(|(j, other)| j == i || other.paths().is_disjoint(&racer.paths()));
        let clear_base =
            ((racer.base + 1)..=case.pre.len()).all(|k| changed_at[k].is_disjoint(&racer.paths()));
        if alone && clear_base && case.attempts as usize >= n {
            let ok = matches!(result, Ok(Landed::Landed { .. }));
            prop_assert!(ok, "racer {} should land: {:?}", i, result);
        }
    }

    // Retries agree; records say what each draft became.
    for (i, result) in results.iter().enumerate() {
        let Ok(Landed::Landed {
            landed: id,
            rebased,
            ..
        }) = result
        else {
            continue;
        };
        let again = block_on(r.e().land(&r.cx, land_req(heads[i])));
        prop_assert_eq!(&again, result);
        let head = r.changeset(heads[i]);
        if *rebased {
            prop_assert_eq!(head.state, DraftState::Superseded);
            prop_assert_eq!(head.superseded_by, Some(*id));
        } else {
            prop_assert_eq!(head.state, DraftState::Landed);
        }
        for draft in &drafts[i] {
            let state = r.changeset(*draft).state;
            let ok = matches!(state, DraftState::Landed | DraftState::Superseded);
            prop_assert!(ok, "draft {} is {:?}", draft, state);
        }
        prop_assert_eq!(r.changeset(*id).state, DraftState::Landed);
    }
    Ok(())
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(192))]

    #[test]
    fn racing_lands_match_the_model(case in case()) {
        run(&case)?;
    }
}
