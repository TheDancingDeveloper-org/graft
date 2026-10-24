//! Hand-placed probes: exact clock ticks and single faults the random
//! adversary would only find by luck.
//!
//! - [`lease_boundary`]: what the engine and the sweep do at the exact end
//!   of a workspace lease and of a land-attempt lease (the hooks review's
//!   open question, WI-2345).
//! - [`sync_after_lost_ack`]: what a client sees when a land's ack is lost
//!   and it syncs before landing again (§3.3 property 1, "retry after a lost
//!   ack": the note asks the first worker step to pin this down).

use std::collections::BTreeSet;
use std::rc::Rc;

use graft_model::{Change, ChangesetId, EntryKind, ObjectId, ObjectKind, RepoPath};
use graft_proto::types::{
    DraftState, GetChangesets, Landed, OpenWorkspace, PutObjects, ResolveRef, WorkspaceRef,
};
use graft_proto::{ApiError, CallContext, Frame, ObjectApi, RepoApi, WorkspaceApi};
use graft_store::{MetaStore, RecordState};

use crate::check::gc_safety;
use crate::exec::run_local;
use crate::faulty::{FaultKind, FaultRates};
use crate::scenario::{
    EngineHook, HarnessEngine, LEASE_MS, LINE, Plan, REPO, SHARED, Shape, World, caller,
    commit_req, land_req, parse_shared, render_shared,
};

/// What a probe found.
#[derive(Clone, Debug, Default)]
pub struct Findings {
    /// Broken expectations.
    pub failures: Vec<String>,
    /// What happened, step by step.
    pub notes: Vec<String>,
}

fn quiet_world(hook: Option<EngineHook>) -> World {
    let shape = Shape {
        clients: 0,
        intents: 0,
        events: 0,
        rates: FaultRates::none(),
    };
    World::new(&Plan::generate(0, &shape), hook)
}

fn path(s: &str) -> RepoPath {
    RepoPath::new(s).expect("probe path")
}

/// Commits `files` on `parent`, into `workspace` when given.
fn commit(
    e: &HarnessEngine,
    cx: &CallContext,
    parent: ChangesetId,
    files: &[(&str, Vec<u8>)],
    workspace: Option<&str>,
) -> Result<ChangesetId, ApiError> {
    run_local(async {
        e.put_objects(
            cx,
            PutObjects {
                repo: REPO.to_owned(),
                blobs: files
                    .iter()
                    .map(|(_, d)| Frame::encode(ObjectKind::Blob, d))
                    .collect(),
            },
        )
        .await?;
        let mut changes: Vec<Change> = files
            .iter()
            .map(|(p, d)| Change::Put {
                path: path(p),
                kind: EntryKind::File,
                id: ObjectId::hash(ObjectKind::Blob, d),
            })
            .collect();
        changes.sort_by(|a, b| a.path().cmp(b.path()));
        let req = commit_req(vec![parent], changes, workspace.map(str::to_owned), "probe");
        Ok(e.create_changeset(cx, req).await?.id)
    })
}

fn open(
    e: &HarnessEngine,
    cx: &CallContext,
    name: &str,
) -> Result<graft_proto::types::WorkspaceView, ApiError> {
    run_local(e.open_workspace(
        cx,
        OpenWorkspace {
            repo: REPO.to_owned(),
            name: Some(name.to_owned()),
            line: LINE.to_owned(),
            at: None,
            lease_ms: LEASE_MS,
        },
    ))
}

fn tip(e: &HarnessEngine) -> ChangesetId {
    run_local(e.resolve_ref(
        &caller("probe"),
        ResolveRef {
            repo: REPO.to_owned(),
            name: LINE.to_owned(),
        },
    ))
    .expect("tip")
    .target
}

fn state(e: &HarnessEngine, id: ChangesetId) -> Option<DraftState> {
    let got = run_local(e.get_changesets(
        &caller("probe"),
        GetChangesets {
            repo: REPO.to_owned(),
            ids: vec![id],
        },
    ))
    .ok()?;
    got.found.first().map(|v| v.state)
}

fn sweep(e: &HarnessEngine) -> Result<u64, ApiError> {
    run_local(e.gc_sweep(&caller("gc"), REPO, false)).map(|r| r.collected)
}

/// The shared file with `token` added to region `r`, on `at`.
fn shared_with(e: &HarnessEngine, at: ChangesetId, r: usize, token: &str) -> Vec<u8> {
    let data = run_local(e.read_file(
        &caller("probe"),
        graft_proto::types::PathAt {
            repo: REPO.to_owned(),
            at,
            path: path(SHARED),
        },
    ))
    .expect("shared")
    .data;
    let mut regions: Vec<BTreeSet<String>> = parse_shared(&data).expect("layout");
    regions[r].insert(token.to_owned());
    render_shared(&regions)
}

/// Land-attempt drafts the store holds: `(id, lease end)`.
fn land_held(world: &World) -> Vec<(ChangesetId, i64)> {
    let store = &*world.store;
    run_local(async {
        let repo = MetaStore::repo(store, REPO)
            .await
            .expect("read")
            .expect("repo");
        store
            .list_records(repo, RecordState::Draft, None, 1000)
            .await
            .expect("drafts")
            .into_iter()
            .filter_map(|(id, r)| {
                let end = r
                    .holder?
                    .strip_prefix("land:")?
                    .split(':')
                    .next()?
                    .parse()
                    .ok()?;
                Some((id, end))
            })
            .collect()
    })
}

/// Probes both lease ends at the exact tick. Without planted bugs it must
/// find nothing; it reports what the engine and sweep did in `notes`.
///
/// 1. A workspace at `now == lease end` still takes a commit (the engine
///    lapses a lease strictly after its end).
/// 2. A sweep at that tick leaves the workspace's draft alone.
/// 3. A land rebases the workspace's stack, stores its candidates under a
///    land-attempt lease and crashes before its CAS. A sweep one ms before
///    the lease ends must not collect them.
/// 4. At the exact end the sweep may collect them. After the delete grace
///    has passed too, the land retried must still land, with every object
///    present.
pub fn lease_boundary(hook: Option<EngineHook>) -> Findings {
    let mut f = Findings::default();
    let mut world = quiet_world(hook);
    let mut engine: Rc<HarnessEngine> = world.engine();
    world.setup(&engine);
    let me = caller("c0");
    let other = caller("c1");

    let ws = open(&engine, &me, "w0").expect("open");
    let end = ws.lease_until_ms;
    let d1 = commit(
        &engine,
        &me,
        ws.base,
        &[("own/c0.txt", b"one\n".to_vec())],
        Some("w0"),
    )
    .expect("first draft");
    world.clock.advance(end - world.clock_now());
    f.notes
        .push(format!("clock at the workspace lease end {end}"));

    // 1. The engine at the exact end.
    let d2 = match commit(
        &engine,
        &me,
        d1,
        &[("own/c0.txt", b"one\ntwo\n".to_vec())],
        Some("w0"),
    ) {
        Ok(id) => id,
        Err(e) => {
            f.failures.push(format!(
                "at now == lease end the engine refused a commit to the workspace: {e:?}"
            ));
            // Carry on with the one draft, to probe the sweep too.
            d1
        }
    };
    f.notes
        .push(format!("the stack head at now == lease end is {d2}"));

    // 2. The sweep at the exact end.
    match sweep(&engine) {
        Ok(n) => f
            .notes
            .push(format!("sweep at the workspace lease end collected {n}")),
        Err(e) => f
            .failures
            .push(format!("sweep at the workspace lease end failed: {e:?}")),
    }
    for d in BTreeSet::from([d1, d2]) {
        if state(&engine, d) != Some(DraftState::Draft) {
            f.failures.push(format!(
                "at now == lease end the sweep moved draft {d} to {:?}",
                state(&engine, d)
            ));
        }
    }

    // 3. A land whose candidates outlive a crash before the CAS.
    let base = tip(&engine);
    let moved = commit(
        &engine,
        &other,
        base,
        &[(SHARED, shared_with(&engine, base, 0, "c1.0"))],
        None,
    )
    .expect("other");
    if let Err(e) = run_local(engine.land(&other, land_req(moved))) {
        f.failures.push(format!("moving the line failed: {e:?}"));
        return f;
    }
    world.faults.at_next_ref_write(FaultKind::CrashBefore);
    let first = run_local(engine.land(&me, land_req(d2)));
    f.notes
        .push(format!("land through a crash before the CAS: {first:?}"));
    world.faults.restart();
    engine = world.engine();
    let held = land_held(&world);
    if held.is_empty() {
        f.failures
            .push("the crashed land left no land-attempt candidate".to_owned());
        return f;
    }
    let lease_end = held.iter().map(|(_, e)| *e).min().expect("held");
    f.notes
        .push(format!("{} candidates held until {lease_end}", held.len()));
    world.clock.advance(lease_end - 1 - world.clock_now());
    let swept = sweep(&engine);
    f.notes.push(format!("sweep at lease end - 1: {swept:?}"));
    for (id, _) in &held {
        if state(&engine, *id) != Some(DraftState::Draft) {
            f.failures.push(format!(
                "one ms before its land-attempt lease ends the sweep moved {id} to {:?}",
                state(&engine, *id)
            ));
        }
    }

    // 4. The exact end, then the retry.
    world.clock.advance(1);
    let swept = sweep(&engine);
    let collected: Vec<_> = held
        .iter()
        .filter(|(id, _)| state(&engine, *id).is_none())
        .map(|(id, _)| *id)
        .collect();
    f.notes.push(format!(
        "sweep at the exact lease end: {swept:?}; {} of {} candidates collected",
        collected.len(),
        held.len()
    ));
    // Past the delete grace, the collected candidates' frames may go too.
    world.clock.advance(15 * 60 * 1000 + 1);
    let swept = sweep(&engine);
    f.notes
        .push(format!("sweep past the delete grace: {swept:?}"));
    match run_local(engine.land(&me, land_req(d2))) {
        Ok(Landed::Landed { landed, .. }) => {
            let revived = held.iter().any(|(id, _)| *id == landed);
            f.notes.push(format!(
                "the retried land landed {landed} (an earlier candidate: {revived})"
            ));
            if tip(&engine) != landed {
                f.failures
                    .push("the retried land is not the tip".to_owned());
            }
            let objects = crate::check::objects_present(&world, &engine, landed);
            f.failures.extend(objects);
        }
        other => f.failures.push(format!(
            "the land retried after the lease end failed: {other:?}"
        )),
    }
    f.failures
        .extend(gc_safety(&world.faults).into_iter().map(|x| x.to_string()));
    f
}

/// Pins down what a client sees after a lost land ack, then a sync, then a
/// land again, for a land that fast-forwarded and one that rebased.
pub fn sync_after_lost_ack() -> Findings {
    let mut f = Findings::default();
    for rebase in [false, true] {
        let mut world = quiet_world(None);
        let engine = world.engine();
        world.setup(&engine);
        let me = caller("c0");
        let ws = open(&engine, &me, "w0").expect("open");
        let draft = commit(
            &engine,
            &me,
            ws.base,
            &[("own/c0.txt", b"one\n".to_vec())],
            Some("w0"),
        )
        .expect("draft");
        if rebase {
            let other = caller("c1");
            let base = tip(&engine);
            let moved = commit(
                &engine,
                &other,
                base,
                &[(SHARED, shared_with(&engine, base, 0, "c1.0"))],
                None,
            )
            .expect("other");
            run_local(engine.land(&other, land_req(moved))).expect("move the line");
        }
        let case = if rebase { "rebased" } else { "fast-forward" };
        world.faults.at_next_ref_write(FaultKind::LostAck);
        let lost = run_local(engine.land(&me, land_req(draft)));
        let landed_tip = tip(&engine);
        let synced = run_local(engine.sync(
            &me,
            WorkspaceRef {
                repo: REPO.to_owned(),
                name: "w0".to_owned(),
            },
            None,
        ));
        let head = synced.as_ref().ok().and_then(|s| s.workspace.head);
        f.notes.push(format!(
            "{case}: the lost-ack land answered {lost:?}; sync answered head {head:?} ({})",
            if synced.is_ok() { "ok" } else { "error" }
        ));
        let retry = run_local(engine.land(&me, land_req(head.unwrap_or(draft))));
        match &retry {
            Ok(Landed::Landed { landed, .. })
                if *landed == landed_tip && tip(&engine) == landed_tip =>
            {
                f.notes.push(format!(
                    "{case}: landing again returned the first land, the line did not move"
                ));
            }
            other => f
                .failures
                .push(format!("{case}: landing again answered {other:?}")),
        }
    }
    f
}
