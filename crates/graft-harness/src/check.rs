//! The checker: the history and the final state against a model of the
//! line (§3.3).
//!
//! It reads the final state through a fresh engine with every fault off,
//! and the store underneath for objects and the journal. It is the one
//! place the harness walks history on purpose, as an oracle.

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::fmt;

use graft_engine::REBASED_FROM;
use graft_model::{ChangesetId, ObjectId, Tree};
use graft_proto::types::{ChangesetView, DraftState, GetChangesets, Landed, PathAt};
use graft_proto::{ApiError, Frame, RepoApi, WorkspaceApi};
use graft_store::{MetaStore, ObjectStore, RepoId};

use crate::exec::run_local;
use crate::scenario::{
    BASE_TOKEN, HarnessEngine, LINE, Op, Outcome, Plan, REPO, SHARED, caller, parse_private,
    parse_shared, private_path, token,
};

/// How far the checker follows `graft.rebased-from` to a root.
const ORIGIN_HOPS: usize = 256;

/// A broken property.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Failure {
    /// Which property (§3.3 numbering in the name).
    pub property: &'static str,
    /// What broke.
    pub detail: String,
}

impl fmt::Display for Failure {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}: {}", self.property, self.detail)
    }
}

/// What the checker found.
#[derive(Clone, Debug, Default)]
pub struct Report {
    /// Broken properties; empty when the run passed.
    pub failures: Vec<Failure>,
    /// Lands acknowledged with `Landed`.
    pub acked: usize,
    /// Intents on the line.
    pub landed_intents: usize,
    /// Empty lands (a re-land after a sync, allowed by the model).
    pub empty_lands: usize,
    /// Conflicts reported to clients.
    pub conflicts: usize,
    /// Conflicts the model does not expect (reported, not a failure).
    pub spurious_conflicts: usize,
    /// Intents given up after `MAX_TRIES`.
    pub gave_up: usize,
    /// Process restarts.
    pub restarts: usize,
    /// Error faults that fired.
    pub faults: usize,
    /// GC passes that finished.
    pub gc_passes: usize,
    /// Records the sweep collected.
    pub collected: usize,
}

impl Report {
    /// Whether every property held.
    pub fn passed(&self) -> bool {
        self.failures.is_empty()
    }

    fn fail(&mut self, property: &'static str, detail: impl Into<String>) {
        self.failures.push(Failure {
            property,
            detail: detail.into(),
        });
    }
}

struct Reader<'a> {
    engine: &'a HarnessEngine,
    views: HashMap<ChangesetId, Option<ChangesetView>>,
}

impl Reader<'_> {
    fn view(&mut self, id: ChangesetId) -> Option<ChangesetView> {
        if let Some(v) = self.views.get(&id) {
            return v.clone();
        }
        let got = run_local(self.engine.get_changesets(
            &caller("checker"),
            GetChangesets {
                repo: REPO.to_owned(),
                ids: vec![id],
            },
        ))
        .ok()
        .and_then(|mut c| c.found.pop());
        self.views.insert(id, got.clone());
        got
    }

    fn first_parent(&mut self, id: ChangesetId) -> Option<ChangesetId> {
        self.view(id)?.changeset.parents.first().copied()
    }

    fn rebased_from(&mut self, id: ChangesetId) -> Option<ChangesetId> {
        let v = self.view(id)?;
        ChangesetId::parse(v.changeset.metadata.get(REBASED_FROM)?).ok()
    }

    fn file(&self, at: ChangesetId, path: &str) -> Result<Vec<u8>, ApiError> {
        run_local(self.engine.read_file(
            &caller("checker"),
            PathAt {
                repo: REPO.to_owned(),
                at,
                path: graft_model::RepoPath::new(path).expect("path"),
            },
        ))
        .map(|f| f.data)
    }

    /// Whether `ancestor` is on `id`'s first-parent chain within `max` hops.
    fn first_parent_reaches(&mut self, id: ChangesetId, ancestor: ChangesetId, max: usize) -> bool {
        let mut cur = Some(id);
        for _ in 0..=max {
            match cur {
                Some(c) if c == ancestor => return true,
                Some(c) => cur = self.first_parent(c),
                None => return false,
            }
        }
        false
    }
}

/// Checks a finished run of `plan`.
pub fn check(plan: &Plan, out: &Outcome) -> Report {
    let mut report = Report::default();
    let engine = &*out.engine;
    let store = &*out.world.store;
    let repo = run_local(MetaStore::repo(store, REPO))
        .expect("repo read")
        .expect("repo");
    let mut rd = Reader {
        engine,
        views: HashMap::new(),
    };

    for op in &out.history {
        match op {
            Op::Restart => report.restarts += 1,
            Op::GaveUp { .. } => report.gave_up += 1,
            Op::Gc(Ok(())) => report.gc_passes += 1,
            Op::Land {
                result: Err(ApiError::Conflict { .. }),
                ..
            } => report.conflicts += 1,
            Op::Stuck => report.fail(
                "liveness",
                format!("still running after {} steps", out.steps),
            ),
            _ => {}
        }
    }
    report.faults = out.world.faults.fired().len();

    // The line: its first-parent chain from the tip to the root.
    let tip = run_local(store.get_ref(repo, LINE))
        .expect("ref read")
        .expect("line");
    let mut chain = Vec::new();
    let mut cur = tip.target;
    let limit = out.history.len() + 16;
    while cur != out.root {
        chain.push(cur);
        match rd.first_parent(cur) {
            Some(p) if chain.len() <= limit => cur = p,
            _ => {
                report.fail(
                    "2 lines move forward",
                    format!(
                        "the tip {} does not descend from the root by first parents",
                        tip.target
                    ),
                );
                break;
            }
        }
    }
    chain.reverse();
    let on_chain: HashMap<ChangesetId, usize> =
        chain.iter().enumerate().map(|(i, c)| (*c, i + 1)).collect();

    check_lands(plan, out, &mut rd, &chain, &on_chain, &mut report);
    check_journal(out, &mut rd, repo, &tip, &mut report);
    check_cas(out, &mut report);
    check_objects(store, repo, &mut rd, &chain, &mut report);
    report.collected = out.world.faults.collections().len();
    report.failures.extend(gc_safety(&out.world.faults));
    check_stranded(plan, out, &mut rd, tip.target, &mut report);
    report
}

/// Property 1 (no lost or duplicated land, retries agree) and 4 (the
/// content model).
fn check_lands(
    plan: &Plan,
    out: &Outcome,
    rd: &mut Reader<'_>,
    chain: &[ChangesetId],
    on_chain: &HashMap<ChangesetId, usize>,
    report: &mut Report,
) {
    let mut roots: HashMap<ChangesetId, (usize, usize)> = HashMap::new();
    for op in &out.history {
        if let Op::Root {
            client,
            intent,
            root,
            ..
        } = op
        {
            roots.insert(*root, (*client, *intent));
        }
    }

    // Each changeset on the line, traced to the root draft a client made.
    let mut origin: HashMap<ChangesetId, (usize, usize)> = HashMap::new();
    let mut by_intent: BTreeMap<(usize, usize), Vec<ChangesetId>> = BTreeMap::new();
    for &c in chain {
        let mut cur = c;
        let mut found = None;
        for _ in 0..ORIGIN_HOPS {
            if let Some(o) = roots.get(&cur) {
                found = Some(*o);
                break;
            }
            match rd.rebased_from(cur) {
                Some(src) => cur = src,
                None => break,
            }
        }
        let Some(o) = found else {
            report.fail(
                "1 no lost or duplicated land",
                format!("{c} on the line traces to no draft a client made"),
            );
            continue;
        };
        let view = rd.view(c);
        if view.as_ref().map(|v| v.state) != Some(DraftState::Landed) {
            report.fail(
                "1 no lost or duplicated land",
                format!(
                    "{c} is on the line but its record is {:?}",
                    view.map(|v| v.state)
                ),
            );
        }
        let empty = rd.view(c).is_some_and(|v| v.changeset.changes.is_empty());
        origin.insert(c, o);
        if empty {
            report.empty_lands += 1;
        } else {
            by_intent.entry(o).or_default().push(c);
        }
    }
    report.landed_intents = by_intent.len();
    for ((client, intent), lands) in &by_intent {
        if lands.len() > 1 {
            report.fail(
                "1 no lost or duplicated land",
                format!(
                    "intent {} landed {} times with changes: {lands:?}",
                    token(*client, *intent),
                    lands.len()
                ),
            );
        }
    }

    // Every acknowledged land is on the line, from its own intent, and every
    // answer for one head is the same.
    let mut answers: HashMap<ChangesetId, Landed> = HashMap::new();
    for op in &out.history {
        let Op::Land {
            client,
            intent,
            head,
            result: Ok(landed),
            ..
        } = op
        else {
            continue;
        };
        let Landed::Landed { landed: id, .. } = landed else {
            report.fail(
                "1 no lost or duplicated land",
                format!("{head} was queued on a direct line"),
            );
            continue;
        };
        report.acked += 1;
        if !on_chain.contains_key(id) {
            report.fail(
                "1 no lost or duplicated land",
                format!(
                    "acknowledged land {id} of {} is not on the line",
                    token(*client, *intent)
                ),
            );
        } else if origin.get(id).is_some_and(|o| *o != (*client, *intent)) {
            report.fail(
                "1 no lost or duplicated land",
                format!(
                    "land {id} answered to {} traces to another intent",
                    token(*client, *intent)
                ),
            );
        }
        match answers.get(head) {
            Some(first) if first != landed => report.fail(
                "1 no lost or duplicated land",
                format!("a retry of {head} got {landed:?}, the first answer was {first:?}"),
            ),
            Some(_) => {}
            None => {
                answers.insert(*head, landed.clone());
            }
        }
    }

    // Property 4: each region holds exactly the tokens of the intents that
    // landed on it.
    let Some(&tip) = chain.last() else {
        return;
    };
    let landed: BTreeSet<(usize, usize)> = by_intent.keys().copied().collect();
    match rd.file(tip, SHARED).ok().as_deref().and_then(parse_shared) {
        None => report.fail(
            "4 conflicts are never silent",
            "the shared file at the tip is out of layout",
        ),
        Some(regions) => {
            for (r, got) in regions.iter().enumerate() {
                let mut want = BTreeSet::from([BASE_TOKEN.to_owned()]);
                for &(c, i) in &landed {
                    if plan.clients[c][i].regions.contains(&r) {
                        want.insert(token(c, i));
                    }
                }
                if *got != want {
                    report.fail(
                        "4 conflicts are never silent",
                        format!("region {r} holds {got:?}, the model says {want:?}"),
                    );
                }
            }
        }
    }
    for (c, intents) in plan.clients.iter().enumerate() {
        let want: BTreeSet<String> = landed
            .iter()
            .filter(|(cc, i)| *cc == c && intents[*i].private)
            .map(|(cc, i)| token(*cc, *i))
            .collect();
        let got = rd
            .file(tip, &private_path(c))
            .map(|d| parse_private(&d))
            .unwrap_or_default();
        if got != want {
            report.fail(
                "4 conflicts are never silent",
                format!("{} holds {got:?}, the model says {want:?}", private_path(c)),
            );
        }
    }

    // Spurious conflicts: none of the lands between the draft's base and the
    // tip it conflicted with touched its regions.
    let parents: HashMap<ChangesetId, ChangesetId> = out
        .history
        .iter()
        .filter_map(|op| match op {
            Op::Root { root, parent, .. } => Some((*root, *parent)),
            _ => None,
        })
        .collect();
    for op in &out.history {
        let Op::Land {
            client,
            intent,
            head,
            result: Err(ApiError::Conflict { onto, .. }),
            ..
        } = op
        else {
            continue;
        };
        let (Some(base), Some(&to)) = (parents.get(head), on_chain.get(onto)) else {
            continue;
        };
        let from = if *base == out.root {
            0
        } else {
            on_chain.get(base).copied().unwrap_or(0)
        };
        let mine = &plan.clients[*client][*intent];
        let real = chain[from..to].iter().any(|c| {
            origin.get(c).is_some_and(|&(oc, oi)| {
                let theirs = &plan.clients[oc][oi];
                theirs.regions.iter().any(|r| mine.regions.contains(r))
                    || (oc == *client && theirs.private && mine.private)
            })
        });
        if !real {
            report.spurious_conflicts += 1;
        }
    }
}

/// Properties 2 and 3: the `RefMoved` chain of the line is gap-free and
/// moves forward along first parents, and the stored ref matches its end.
fn check_journal(
    out: &Outcome,
    rd: &mut Reader<'_>,
    repo: RepoId,
    tip: &graft_store::RefRecord,
    report: &mut Report,
) {
    let store = &*out.world.store;
    let mut from = 1;
    let mut prev: Option<(Option<ChangesetId>, u64)> = None;
    loop {
        let page = run_local(store.journal(repo, from, 1000)).expect("journal read");
        let Some(last) = page.last() else {
            break;
        };
        from = last.0 + 1;
        for (lsn, kind, body) in page {
            if kind != graft_engine::journal::REF_MOVED {
                continue;
            }
            let Ok(v) = serde_json::from_slice::<serde_json::Value>(&body) else {
                report.fail(
                    "2 lines move forward",
                    format!("RefMoved at {lsn} does not parse"),
                );
                continue;
            };
            if v["name"] != LINE {
                continue;
            }
            let id = |k: &str| v[k].as_str().and_then(|s| ChangesetId::parse(s).ok());
            let (old, new, version) = (id("old"), id("new"), v["version"].as_u64());
            let (Some(new), Some(version)) = (new, version) else {
                report.fail(
                    "2 lines move forward",
                    format!("RefMoved at {lsn} lacks new or version"),
                );
                continue;
            };
            let (want_old, want_version) = match prev {
                None => (None, 1),
                Some((p, ver)) => (p, ver + 1),
            };
            if version != want_version {
                report.fail(
                    "3 CAS only",
                    format!("RefMoved at {lsn} has version {version}, expected {want_version}"),
                );
            }
            if old != want_old {
                report.fail(
                    "3 CAS only",
                    format!("RefMoved at {lsn} moves from {old:?}, the line was at {want_old:?}"),
                );
            }
            if let Some(old) = old
                && !rd.first_parent_reaches(new, old, 64)
            {
                report.fail(
                    "2 lines move forward",
                    format!("RefMoved at {lsn}: {old} is not a first parent ancestor of {new}"),
                );
            }
            prev = Some((Some(new), version));
        }
    }
    match prev {
        Some((Some(new), version)) if (new, version) == (tip.target, tip.version) => {}
        other => report.fail(
            "3 CAS only",
            format!(
                "the stored line is at ({}, {}), the journal ends at {other:?}",
                tip.target, tip.version
            ),
        ),
    }
}

/// Property 3 at the store port: no two committed ref writes expected the
/// same version, and no transaction moved two refs.
fn check_cas(out: &Outcome, report: &mut Report) {
    let mut seen: HashSet<(String, Option<(ChangesetId, u64)>)> = HashSet::new();
    for txn in out.world.faults.txns() {
        if txn.refs.len() > 1 {
            report.fail(
                "3 CAS only",
                format!("one transaction moved {} refs", txn.refs.len()),
            );
        }
        for w in txn.refs {
            let Some(expected) = w.expected else {
                report.fail(
                    "3 CAS only",
                    format!("a write of {} carried no RefIs", w.name),
                );
                continue;
            };
            if !seen.insert((w.name.clone(), expected)) {
                report.fail(
                    "3 CAS only",
                    format!(
                        "two committed writes of {} both expected {expected:?}",
                        w.name
                    ),
                );
            }
        }
    }
}

/// Property 5: every object a landed changeset refers to is present and
/// hashes to its id.
fn check_objects(
    store: &graft_store::memory::MemoryStore,
    repo: RepoId,
    rd: &mut Reader<'_>,
    chain: &[ChangesetId],
    report: &mut Report,
) {
    let mut seen: HashSet<ObjectId> = HashSet::new();
    for &c in chain {
        let Some(view) = rd.view(c) else {
            continue;
        };
        let mut todo = vec![(view.changeset.root.0, true)];
        while let Some((id, is_tree)) = todo.pop() {
            if !seen.insert(id) {
                continue;
            }
            let bytes = match run_local(ObjectStore::get(store, repo, id)) {
                Ok(Some(b)) => b,
                _ => {
                    report.fail(
                        "5 objects before metadata",
                        format!("landed {c} refers to {id:?}, which is missing"),
                    );
                    continue;
                }
            };
            let Ok(payload) = Frame::from_bytes(&bytes).and_then(|f| f.decode_verified(id)) else {
                report.fail(
                    "5 objects before metadata",
                    format!("object {id:?} does not verify"),
                );
                continue;
            };
            if is_tree {
                match Tree::decode(&payload) {
                    Ok(tree) => {
                        todo.extend(tree.entries.into_iter().map(|e| (e.id, e.kind.is_tree())))
                    }
                    Err(_) => report.fail(
                        "5 objects before metadata",
                        format!("tree {id:?} does not decode"),
                    ),
                }
            }
        }
    }
}

/// Property 6 (safety): the sweep never collected a record that was landed,
/// pinned, or inside a live lease or expiry. Judged at the store port, from
/// the record and protection each collection replaced.
pub fn gc_safety(faults: &crate::faulty::Faults) -> Vec<Failure> {
    let mut out = Vec::new();
    for c in faults.collections() {
        let now = c.now_ms;
        let mut why = Vec::new();
        if c.record.state == graft_store::RecordState::Landed {
            why.push("it was landed".to_owned());
        }
        if c.protection.pins > 0 {
            why.push(format!("it had {} pins", c.protection.pins));
        }
        if let Some(e) = c.protection.expires_at_ms
            && now < e
        {
            why.push(format!("its expiry {e} was {} ms away", e - now));
        }
        if let Some(h) = c.record.holder.as_deref() {
            if let Some(rest) = h.strip_prefix("land:") {
                if let Some(end) = rest.split(':').next().and_then(|s| s.parse::<i64>().ok())
                    && now < end
                {
                    why.push(format!(
                        "its land-attempt lease {h} had {} ms left",
                        end - now
                    ));
                }
            } else if let Some(lease) = c.workspace_lease_ms
                && now <= lease
            {
                why.push(format!("its workspace {h} was live until {lease}"));
            }
        }
        if !why.is_empty() {
            out.push(Failure {
                property: "6 GC is safe",
                detail: format!("the sweep collected {} at {now}: {}", c.id, why.join("; ")),
            });
        }
    }
    out
}

/// Property 7: after the faults, every client's workspace answers and its
/// sync moves it to the tip.
fn check_stranded(
    plan: &Plan,
    out: &Outcome,
    rd: &mut Reader<'_>,
    tip: ChangesetId,
    report: &mut Report,
) {
    let engine = rd.engine;
    for c in 0..plan.clients.len() {
        let cx = caller(&format!("c{c}"));
        let wref = graft_proto::types::WorkspaceRef {
            repo: REPO.to_owned(),
            name: format!("w{c}"),
        };
        let lapsed =
            |e: &ApiError| matches!(e, ApiError::NotFound { .. } | ApiError::LeaseExpired { .. });
        match run_local(engine.get_workspace(&cx, wref.clone())) {
            Ok(ws) if ws.lease_until_ms < out.world.clock_now() => continue,
            Ok(_) => {}
            Err(e) if lapsed(&e) => continue,
            Err(e) => {
                report.fail("7 never stranded", format!("w{c}: status failed: {e:?}"));
                continue;
            }
        }
        let mut synced = run_local(engine.sync(&cx, wref.clone(), None));
        if let Err(ApiError::Conflict { .. }) = synced {
            // A conflict is an answer, not a dead end: drop the stack and
            // the workspace must move.
            let head = run_local(engine.get_workspace(&cx, wref.clone()))
                .ok()
                .and_then(|w| w.head);
            let dropped = run_local(engine.set_head(
                &cx,
                graft_proto::types::SetHead {
                    workspace: wref.clone(),
                    expected: head,
                    new: None,
                },
            ));
            if let Err(e) = dropped {
                report.fail(
                    "7 never stranded",
                    format!("w{c}: dropping a conflicted stack failed: {e:?}"),
                );
                continue;
            }
            synced = run_local(engine.sync(&cx, wref, None));
        }
        match synced {
            Ok(s) if s.workspace.base == tip => {}
            Ok(s) => report.fail(
                "7 never stranded",
                format!(
                    "w{c}: sync left the base at {}, the tip is {tip}",
                    s.workspace.base
                ),
            ),
            Err(e) if lapsed(&e) => {}
            Err(e) => report.fail("7 never stranded", format!("w{c}: sync failed: {e:?}")),
        }
    }
}

/// The objects `landed`'s tree refers to that are missing or corrupt
/// (property 5, for probes).
pub fn objects_present(
    world: &crate::scenario::World,
    engine: &HarnessEngine,
    landed: ChangesetId,
) -> Vec<String> {
    let store = &*world.store;
    let repo = run_local(MetaStore::repo(store, REPO))
        .expect("repo read")
        .expect("repo");
    let mut rd = Reader {
        engine,
        views: HashMap::new(),
    };
    let mut report = Report::default();
    check_objects(store, repo, &mut rd, &[landed], &mut report);
    report.failures.into_iter().map(|f| f.to_string()).collect()
}
