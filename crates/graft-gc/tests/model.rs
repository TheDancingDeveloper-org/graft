//! G3.2's model test: random interleavings of create, upload, land, close,
//! adopt, rebase, sync, the clock and GC, against the real engine.
//!
//! An operation either runs alone or races a sweep pass: both tasks run on
//! one thread, the stores yield at every call, and a random schedule picks
//! which task runs next, so a sweep's transactions fall between a write's
//! reads, claims and commit in every order. After every step:
//!
//! - the brute-force check of design §8.6 holds: no non-collected
//!   changeset (landed ones included) reaches a missing frame or a doomed
//!   object, every object it reaches has a row on its chain, and pins equal
//!   the live children;
//! - a sweep that runs alone does exactly what the dry run planned, and
//!   `--verify` finds nothing unsafe.
//!
//! At the end everything is closed and expired, and the sweep runs until it
//! settles. The frames it deleted are exactly the held objects no landed
//! changeset reaches, and the bytes it reports are theirs.

mod common;

use std::collections::{BTreeMap, HashSet};

use common::*;
use graft_model::ChangesetId;
use graft_proto::types::*;
use graft_proto::{ApiError, RepoApi, WorkspaceApi};
use graft_store::MetaStore;
use proptest::prelude::*;

const PATHS: &[&str] = &["a", "b", "d/c", "d/e/f", "g"];
const WORKSPACES: &[&str] = &["w0", "w1"];

#[derive(Clone, Debug)]
enum Op {
    Commit {
        ws: usize,
        path: usize,
    },
    Merge {
        ws: usize,
        other: usize,
        path: usize,
    },
    Rebase {
        ws: usize,
    },
    Sync {
        ws: usize,
    },
    Land {
        ws: usize,
    },
    Close {
        ws: usize,
    },
    Drop {
        ws: usize,
    },
    Direct {
        path: usize,
    },
    Adopt {
        ws: usize,
    },
    /// Uploads blob `u<n>`; a repeat re-puts it (and claims it if doomed).
    Upload {
        n: u8,
    },
    /// Commits blob `u<n>` by id, without uploading it: a claim, a revival,
    /// or `MissingObjects`.
    Name {
        ws: usize,
        n: u8,
        path: usize,
    },
    Advance {
        by: u8,
    },
    Gc,
}

fn engine_op() -> impl Strategy<Value = Op> {
    let ws = 0..WORKSPACES.len();
    let path = 0..PATHS.len();
    prop_oneof![
        8 => (ws.clone(), path.clone()).prop_map(|(ws, path)| Op::Commit { ws, path }),
        2 => (ws.clone(), ws.clone(), path.clone())
            .prop_map(|(ws, other, path)| Op::Merge { ws, other, path }),
        2 => ws.clone().prop_map(|ws| Op::Rebase { ws }),
        2 => ws.clone().prop_map(|ws| Op::Sync { ws }),
        4 => ws.clone().prop_map(|ws| Op::Land { ws }),
        2 => ws.clone().prop_map(|ws| Op::Close { ws }),
        1 => ws.clone().prop_map(|ws| Op::Drop { ws }),
        2 => path.clone().prop_map(|path| Op::Direct { path }),
        2 => ws.clone().prop_map(|ws| Op::Adopt { ws }),
        3 => (0..4u8).prop_map(|n| Op::Upload { n }),
        3 => (ws, 0..4u8, path).prop_map(|(ws, n, path)| Op::Name { ws, n, path }),
    ]
}

#[derive(Clone, Debug)]
enum Step {
    Alone(Op),
    /// An engine operation racing a sweep pass, by this schedule.
    Race(Op, Vec<bool>),
}

fn step() -> impl Strategy<Value = Step> {
    prop_oneof![
        6 => engine_op().prop_map(Step::Alone),
        3 => (0..5u8).prop_map(|by| Step::Alone(Op::Advance { by })),
        3 => Just(Step::Alone(Op::Gc)),
        4 => (engine_op(), prop::collection::vec(any::<bool>(), 1..9))
            .prop_map(|(op, schedule)| Step::Race(op, schedule)),
    ]
}

fn advance_ms(by: u8) -> i64 {
    match by {
        0 => 5 * MINUTE,
        1 => GRACE_PLUS,
        2 => 3 * HOUR,
        3 => 25 * HOUR,
        _ => RETENTION_PLUS,
    }
}

/// What the run remembers between steps.
#[derive(Default)]
struct State {
    direct: Option<ChangesetId>,
}

struct Run<'w, M> {
    w: &'w World<M>,
}

impl<M: MetaStore + 'static> Run<'_, M> {
    /// A live workspace `ws`: reopened when it was closed or lapsed.
    async fn ensure(&self, ws: usize) -> WorkspaceView {
        let w = self.w;
        let name = WORKSPACES[ws];
        match w.engine.get_workspace(&w.cx, w.wref(name)).await {
            Ok(view) if view.lease_until_ms >= w.now() => view,
            _ => {
                let _ = w.engine.close_workspace(&w.cx, w.wref(name), false).await;
                w.engine
                    .open_workspace(
                        &w.cx,
                        OpenWorkspace {
                            repo: w.name.clone(),
                            name: Some(name.to_owned()),
                            line: LINE.to_owned(),
                            at: None,
                            lease_ms: 0,
                        },
                    )
                    .await
                    .expect("open")
            }
        }
    }

    async fn tip(&self) -> ChangesetId {
        let w = self.w;
        w.engine
            .resolve_ref(
                &w.cx,
                ResolveRef {
                    repo: w.name.clone(),
                    name: LINE.to_owned(),
                },
            )
            .await
            .expect("line")
            .target
    }

    async fn commit_in(
        &self,
        ws: usize,
        parents: Vec<ChangesetId>,
        path: usize,
        data: &[u8],
        upload: bool,
    ) -> Result<Created, ApiError> {
        let w = self.w;
        if upload {
            w.upload_async(&[data]).await?;
        }
        w.engine
            .create_changeset(
                &w.cx,
                w.request(parents, vec![put(PATHS[path], data)], Some(WORKSPACES[ws])),
            )
            .await
    }

    /// Runs one engine operation, ignoring the errors the engine may
    /// rightly return (a conflict, a stale head, a lapsed lease, a missing
    /// or collected object).
    async fn op(&self, op: &Op, n: usize, state: &mut State) {
        let w = self.w;
        let data = format!("v{n}").into_bytes();
        match *op {
            Op::Commit { ws, path } => {
                let view = self.ensure(ws).await;
                let parent = view.head.unwrap_or(view.base);
                let _ = self.commit_in(ws, vec![parent], path, &data, true).await;
            }
            Op::Merge { ws, other, path } => {
                let view = self.ensure(ws).await;
                let second = self.ensure(other).await.head;
                if let Some(second) = second.filter(|_| other != ws) {
                    let parent = view.head.unwrap_or(view.base);
                    let _ = self
                        .commit_in(ws, vec![parent, second], path, &data, true)
                        .await;
                }
            }
            Op::Rebase { ws } => {
                if let Some(head) = self.ensure(ws).await.head {
                    let onto = self.tip().await;
                    let _ = w
                        .engine
                        .rebase(
                            &w.cx,
                            Rebase {
                                repo: w.name.clone(),
                                changeset: head,
                                onto,
                                merge: MergeMode::File,
                                dry_run: false,
                            },
                        )
                        .await;
                }
            }
            Op::Sync { ws } => {
                self.ensure(ws).await;
                let _ = w.engine.sync(&w.cx, w.wref(WORKSPACES[ws]), None).await;
            }
            Op::Land { ws } => {
                if let Some(head) = self.ensure(ws).await.head {
                    let _ = w
                        .engine
                        .land(
                            &w.cx,
                            Land {
                                repo: w.name.clone(),
                                head,
                                line: LINE.to_owned(),
                                squash: false,
                                merge: MergeMode::File,
                                deadline_ms: None,
                            },
                        )
                        .await;
                }
            }
            Op::Close { ws } => {
                self.ensure(ws).await;
                let _ = w
                    .engine
                    .close_workspace(&w.cx, w.wref(WORKSPACES[ws]), false)
                    .await;
            }
            Op::Drop { ws } => {
                let view = self.ensure(ws).await;
                let _ = w
                    .engine
                    .set_head(
                        &w.cx,
                        SetHead {
                            workspace: w.wref(WORKSPACES[ws]),
                            expected: view.head,
                            new: None,
                        },
                    )
                    .await;
            }
            Op::Direct { path } => {
                let _ = w.upload_async(&[&data]).await;
                let tip = self.tip().await;
                state.direct = w
                    .engine
                    .create_changeset(
                        &w.cx,
                        w.request(vec![tip], vec![put(PATHS[path], &data)], None),
                    )
                    .await
                    .ok()
                    .map(|c| c.id);
            }
            Op::Adopt { ws } => {
                let view = self.ensure(ws).await;
                if let Some(id) = state.direct {
                    let _ = w
                        .engine
                        .set_head(
                            &w.cx,
                            SetHead {
                                workspace: w.wref(WORKSPACES[ws]),
                                expected: view.head,
                                new: Some(id),
                            },
                        )
                        .await;
                }
            }
            Op::Upload { n } => {
                let _ = w.upload_async(&[format!("u{n}").as_bytes()]).await;
            }
            Op::Name { ws, n, path } => {
                let view = self.ensure(ws).await;
                let parent = view.head.unwrap_or(view.base);
                let blob = format!("u{n}").into_bytes();
                let _ = self.commit_in(ws, vec![parent], path, &blob, false).await;
            }
            Op::Advance { by } => w.advance(advance_ms(by)),
            Op::Gc => {}
        }
    }
}

/// A sweep alone: it does what the dry run planned, and verify passes.
fn planned_sweep<M: MetaStore + 'static>(
    w: &World<M>,
) -> Result<graft_gc::SweepReport, TestCaseError> {
    let v = block_on(w.gc.verify(w.repo, w.now(), &w.shape)).expect("verify");
    prop_assert!(v.matches(), "verify: {v:?}");
    let plan = v.plan;
    let swept = w.sweep();
    prop_assert_eq!(
        swept.abandoned as usize,
        plan.abandoned.len(),
        "{:?}",
        swept
    );
    prop_assert_eq!(
        swept.collected as usize,
        plan.changesets.len(),
        "{:?}",
        swept
    );
    prop_assert_eq!(swept.doomed as usize, plan.doomed.len(), "{:?}", swept);
    prop_assert_eq!(
        swept.deleted + swept.recovered,
        plan.deletable,
        "{:?}",
        swept
    );
    prop_assert_eq!(swept.skipped, 0);
    Ok(swept)
}

/// What the sweeps of a run did: alone, racing a write, and in the final
/// drain.
#[derive(Debug, Default)]
struct Totals {
    alone: graft_gc::SweepReport,
    raced: graft_gc::SweepReport,
    drained: graft_gc::SweepReport,
}

fn run_steps(steps: &[Step]) -> Result<Totals, TestCaseError> {
    let w = memory();
    w.seed();
    let run = Run { w: &w };
    let mut state = State::default();
    let mut totals = Totals::default();
    for (n, step) in steps.iter().enumerate() {
        match step {
            Step::Alone(Op::Gc) => add(&mut totals.alone, &planned_sweep(&w)?),
            Step::Alone(op) => block_on(run.op(op, n, &mut state)),
            Step::Race(op, schedule) => {
                w.yields(true);
                let (_, swept) = interleave(
                    run.op(op, n, &mut state),
                    w.gc.sweep(w.repo, &w.shape, false),
                    schedule,
                );
                w.yields(false);
                let swept = swept.map_err(|e| TestCaseError::fail(format!("sweep: {e}")))?;
                add(&mut totals.raced, &swept);
            }
        }
        if let Err(e) = w.check() {
            return Err(TestCaseError::fail(format!("after step {n} {step:?}: {e}")));
        }
    }

    // Close and expire everything, then sweep until it settles.
    for ws in WORKSPACES {
        let _ = w.close(ws);
    }
    w.advance(RETENTION_PLUS);
    let before = w.frames.sizes(w.repo);
    let held: HashSet<_> = before
        .keys()
        .copied()
        .filter(|o| !w.holders_of(*o).is_empty() || w.marks(*o).is_some())
        .collect();
    let drained = w.drain();
    w.check().map_err(TestCaseError::fail)?;
    let after = w.frames.sizes(w.repo);
    let reach = landed_reach(&*w.meta, &w.frames, w.repo).map_err(TestCaseError::fail)?;
    let deleted: BTreeMap<_, _> = before
        .iter()
        .filter(|(o, _)| !after.contains_key(o))
        .map(|(o, s)| (*o, *s))
        .collect();
    let model: BTreeMap<_, _> = before
        .iter()
        .filter(|(o, _)| held.contains(o) && !reach.contains(o))
        .map(|(o, s)| (*o, *s))
        .collect();
    prop_assert_eq!(&deleted, &model, "deleted frames");
    prop_assert_eq!(
        drained.bytes,
        model.values().sum::<u64>(),
        "reclaimed bytes"
    );
    // What is left unreachable was never held by anything.
    for o in after.keys().filter(|o| !reach.contains(o)) {
        prop_assert!(w.holders_of(*o).is_empty() && w.marks(*o).is_none());
    }
    totals.drained = drained;
    Ok(totals)
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(96))]

    #[test]
    fn the_sweep_never_deletes_what_is_held(steps in prop::collection::vec(step(), 1..40)) {
        run_steps(&steps)?;
    }
}

/// A fixed long run, so every operation and race shape appears at least
/// once whatever proptest draws.
#[test]
fn a_long_fixed_run_holds() {
    let mut steps = Vec::new();
    let ops = [
        Op::Commit { ws: 0, path: 0 },
        Op::Commit { ws: 1, path: 1 },
        Op::Merge {
            ws: 1,
            other: 0,
            path: 2,
        },
        Op::Upload { n: 0 },
        Op::Name {
            ws: 0,
            n: 0,
            path: 3,
        },
        Op::Direct { path: 4 },
        Op::Adopt { ws: 1 },
        Op::Rebase { ws: 0 },
        Op::Sync { ws: 1 },
        Op::Land { ws: 0 },
        Op::Close { ws: 1 },
        Op::Drop { ws: 0 },
    ];
    for (i, op) in ops.iter().cycle().take(60).enumerate() {
        let schedule: Vec<bool> = (0..(i % 7 + 1)).map(|k| (i + k) % 3 == 0).collect();
        steps.push(Step::Race(op.clone(), schedule));
        steps.push(Step::Alone(Op::Advance { by: (i % 5) as u8 }));
        if i % 4 == 0 {
            steps.push(Step::Alone(Op::Gc));
        }
    }
    let totals = run_steps(&steps).expect("holds");
    // The run did what it is for: sweeps collected and deleted, alone and
    // while racing writes.
    assert!(totals.alone.collected > 0, "{totals:?}");
    assert!(totals.raced.collected > 0, "{totals:?}");
    assert!(totals.raced.deleted > 0, "{totals:?}");
}
