//! The dry-run report matches a brute-force computation over random
//! histories: workspaces with leases, drafts, landed, superseded and
//! abandoned changesets, successor links, shared and uploaded objects, and
//! stored frames. The model below recomputes every rule from the whole
//! history, so it shares no code with the planner beyond the rules.

use std::collections::{BTreeSet, HashSet};
use std::future::Future;
use std::task::{Context, Poll, Waker};
use std::time::Duration;

use graft_gc::{GcConfig, Report, Shape, dry_run_report};
use graft_model::{ChangesetId, ObjectId};
use graft_store::memory::MemoryStore;
use graft_store::{
    ChangesetRecord, MetaStore, MetaTxn, ObjectStore, RecordState, StoreResult, TxnOutcome,
    WorkspaceRecord, Write,
};
use proptest::prelude::*;

fn block_on<F: Future>(future: F) -> F::Output {
    let mut future = std::pin::pin!(future);
    let mut cx = Context::from_waker(Waker::noop());
    loop {
        if let Poll::Ready(out) = future.as_mut().poll(&mut cx) {
            return out;
        }
    }
}

/// The test's changeset content: parent count, parents, object count,
/// introduced objects. The engine decodes real changesets; this shape reads
/// the same facts back out of the bytes.
struct Links;

fn encode(parents: &[ChangesetId], introduced: &[ObjectId]) -> Vec<u8> {
    let mut out = vec![parents.len() as u8];
    for p in parents {
        out.extend_from_slice(&p.0.0);
    }
    out.push(introduced.len() as u8);
    for o in introduced {
        out.extend_from_slice(&o.0);
    }
    out
}

fn read_ids(bytes: &[u8]) -> Vec<ObjectId> {
    bytes
        .as_chunks::<32>()
        .0
        .iter()
        .map(|c| ObjectId(*c))
        .collect()
}

impl Shape for Links {
    fn parents(&self, content: &[u8]) -> StoreResult<Vec<ChangesetId>> {
        let n = content[0] as usize;
        Ok(read_ids(&content[1..1 + 32 * n])
            .into_iter()
            .map(ChangesetId)
            .collect())
    }

    async fn introduced(
        &self,
        _repo: graft_store::RepoId,
        _id: ChangesetId,
        content: &[u8],
    ) -> StoreResult<Vec<ObjectId>> {
        let n = content[0] as usize;
        let rest = &content[1 + 32 * n..];
        Ok(read_ids(&rest[1..]))
    }
}

fn cs(i: usize) -> ChangesetId {
    ChangesetId(ObjectId([i as u8 + 1; 32]))
}

fn obj(o: usize) -> ObjectId {
    ObjectId([200 + o as u8; 32])
}

#[derive(Clone, Debug)]
struct Node {
    state: RecordState,
    parents: Vec<usize>,
    /// Workspace index (`== workspaces.len()` means a workspace that does not
    /// exist). Only drafts hold one.
    holder: usize,
    successor: Option<usize>,
    received: i64,
    introduced: Vec<usize>,
}

#[derive(Clone, Debug)]
struct History {
    nodes: Vec<Node>,
    leases: Vec<i64>,
    /// Per object: an upload row that expires at this time.
    uploads: Vec<Option<i64>>,
    /// Per object: the stored frame length, or `None` when absent.
    frames: Vec<Option<usize>>,
    now: i64,
    retention: i64,
    batch: u32,
}

fn history(
    raw_nodes: Vec<(u8, Vec<u8>, u8, u8, u16, u8)>,
    raw_leases: Vec<u16>,
    raw_uploads: Vec<Option<u16>>,
    raw_frames: Vec<u8>,
    now: u16,
    retention: u8,
    batch: u8,
) -> History {
    let n = raw_nodes.len();
    let ws_count = raw_leases.len();
    let nodes = raw_nodes
        .into_iter()
        .enumerate()
        .map(|(i, (state, parents, holder, succ, received, intro))| {
            let state = match state % 4 {
                0 => RecordState::Draft,
                1 => RecordState::Landed,
                2 => RecordState::Superseded,
                _ => RecordState::Abandoned,
            };
            let mut parents: Vec<usize> = if i == 0 {
                Vec::new()
            } else {
                parents.iter().map(|p| *p as usize % i).collect()
            };
            parents.sort_unstable();
            parents.dedup();
            let successor = (state == RecordState::Superseded && i + 1 < n && succ % 2 == 0)
                .then(|| i + 1 + (succ as usize / 2) % (n - i - 1));
            Node {
                state,
                parents,
                holder: holder as usize % (ws_count + 1),
                successor,
                received: received as i64 % 301,
                introduced: (0..6).filter(|b| intro >> b & 1 == 1).collect(),
            }
        })
        .collect();
    History {
        nodes,
        leases: raw_leases.into_iter().map(|l| l as i64 % 301).collect(),
        uploads: raw_uploads
            .into_iter()
            .map(|u| u.map(|e| e as i64 % 301))
            .collect(),
        frames: raw_frames
            .into_iter()
            .map(|f| (f != 0).then_some(f as usize % 20 + 1))
            .collect(),
        now: now as i64 % 301,
        retention: retention as i64 % 101,
        batch: 1 + batch as u32 % 3,
    }
}

fn ws_name(i: usize) -> String {
    format!("w{i}")
}

/// Holder of a draft, or `None` when it has no workspace.
fn draft_ws(h: &History, node: &Node) -> Option<usize> {
    (node.state == RecordState::Draft && node.holder < h.leases.len()).then_some(node.holder)
}

/// Writes the history into a fresh store.
fn populate(h: &History) -> (MemoryStore, graft_store::RepoId) {
    let store = MemoryStore::new();
    let repo = block_on(store.create_repo("gc")).expect("repo");
    let mut writes = Vec::new();
    for (i, node) in h.nodes.iter().enumerate() {
        let parents: Vec<ChangesetId> = node.parents.iter().map(|p| cs(*p)).collect();
        let introduced: Vec<ObjectId> = node.introduced.iter().map(|o| obj(*o)).collect();
        let holder = match node.state {
            RecordState::Draft => Some(match draft_ws(h, node) {
                Some(w) => format!("ws:{}", ws_name(w)),
                None => "ws:ghost".to_owned(),
            }),
            _ => None,
        };
        writes.push(Write::PutChangeset {
            id: cs(i),
            content: encode(&parents, &introduced),
            record: ChangesetRecord {
                lsn: 0,
                received_at_ms: node.received,
                state: node.state,
                holder,
                superseded_by: node.successor.map(cs),
            },
        });
        for o in &introduced {
            writes.push(Write::AddHolder {
                object: *o,
                holder: cs(i).to_string(),
            });
        }
    }
    for (w, lease) in h.leases.iter().enumerate() {
        writes.push(Write::PutWorkspace(WorkspaceRecord {
            name: ws_name(w),
            owner: "o".to_owned(),
            session: None,
            line: "main".to_owned(),
            base: cs(0),
            head: None,
            lease_until_ms: *lease,
            work_item: None,
        }));
    }
    for (o, exp) in h.uploads.iter().enumerate() {
        if let Some(exp) = exp {
            writes.push(Write::AddHolder {
                object: obj(o),
                holder: format!("upload:{exp}:u"),
            });
        }
    }
    let outcome = block_on(store.commit(
        repo,
        MetaTxn {
            preconditions: Vec::new(),
            writes,
        },
    ))
    .expect("commit");
    assert!(matches!(outcome, TxnOutcome::Committed(_)));
    for (o, len) in h.frames.iter().enumerate() {
        if let Some(len) = len {
            block_on(store.put(repo, obj(o), vec![o as u8; *len])).expect("put");
        }
    }
    (store, repo)
}

/// The brute-force answer: every rule evaluated over the whole history.
fn brute(h: &History) -> Report {
    let retention = h.retention;
    let is_candidate = |i: usize| -> bool {
        let node = &h.nodes[i];
        match node.state {
            RecordState::Draft => {
                draft_ws(h, node).is_some_and(|w| h.leases[w] + retention <= h.now)
            }
            RecordState::Superseded | RecordState::Abandoned => node.received + retention <= h.now,
            RecordState::Landed | RecordState::Collected => false,
        }
    };
    let live_drafts: Vec<usize> = (0..h.nodes.len())
        .filter(|i| h.nodes[*i].state == RecordState::Draft && !is_candidate(*i))
        .collect();

    // Ancestors of live drafts, not walking past landed changesets.
    let mut ancestors = HashSet::new();
    let mut stack = live_drafts;
    while let Some(i) = stack.pop() {
        if !ancestors.insert(i) {
            continue;
        }
        if h.nodes[i].state == RecordState::Landed {
            continue;
        }
        stack.extend(h.nodes[i].parents.iter().copied());
    }

    let successor_reaches_live = |i: usize| -> bool {
        let mut next = h.nodes[i].successor;
        let mut seen = HashSet::new();
        while let Some(s) = next {
            if !is_candidate(s) || !seen.insert(s) {
                return true;
            }
            next = h.nodes[s].successor;
        }
        false
    };

    let collectable: Vec<bool> = (0..h.nodes.len())
        .map(|i| {
            is_candidate(i)
                && !(ancestors.contains(&i)
                    || (h.nodes[i].state == RecordState::Superseded && successor_reaches_live(i)))
        })
        .collect();
    let mut report = Report {
        changesets: collectable.iter().filter(|c| **c).count() as u64,
        ..Report::default()
    };

    let objects: BTreeSet<usize> = (0..h.nodes.len())
        .filter(|i| collectable[*i])
        .flat_map(|i| h.nodes[i].introduced.iter().copied())
        .collect();
    for o in objects {
        let mut rows: Vec<bool> = Vec::new();
        for (j, node) in h.nodes.iter().enumerate() {
            if node.introduced.contains(&o) {
                rows.push(collectable[j]);
            }
        }
        if let Some(exp) = h.uploads[o] {
            rows.push(exp <= h.now);
        }
        if rows.is_empty() || !rows.iter().all(|gone| *gone) {
            continue;
        }
        if let Some(len) = h.frames[o] {
            report.doomed += 1;
            report.bytes += len as u64;
        }
    }
    report
}

fn history_strategy() -> impl Strategy<Value = History> {
    (
        prop::collection::vec(
            (
                any::<u8>(),
                prop::collection::vec(any::<u8>(), 0..=2),
                any::<u8>(),
                any::<u8>(),
                any::<u16>(),
                any::<u8>(),
            ),
            1..=10,
        ),
        prop::collection::vec(any::<u16>(), 0..=3),
        prop::collection::vec(prop::option::of(any::<u16>()), 6..=6),
        prop::collection::vec(any::<u8>(), 6..=6),
        any::<u16>(),
        any::<u8>(),
        any::<u8>(),
    )
        .prop_map(|(nodes, leases, uploads, frames, now, retention, batch)| {
            history(nodes, leases, uploads, frames, now, retention, batch)
        })
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(512))]

    #[test]
    fn dry_run_matches_brute_force(h in history_strategy()) {
        let (store, repo) = populate(&h);
        let config = GcConfig {
            draft_retention: Duration::from_millis(h.retention as u64),
            batch: h.batch,
            ..GcConfig::default()
        };
        let planned = block_on(dry_run_report(&store, &store, &config, repo, h.now, &Links))
            .expect("dry run");
        prop_assert_eq!(planned, brute(&h));
    }
}

fn node(state: RecordState, received: i64, introduced: &[usize]) -> Node {
    Node {
        state,
        parents: Vec::new(),
        holder: 0,
        successor: None,
        received,
        introduced: introduced.to_vec(),
    }
}

fn plan(h: &History) -> Report {
    let (store, repo) = populate(h);
    let config = GcConfig {
        draft_retention: Duration::from_millis(h.retention as u64),
        batch: h.batch,
        ..GcConfig::default()
    };
    block_on(dry_run_report(&store, &store, &config, repo, h.now, &Links)).expect("dry run")
}

fn simple(nodes: Vec<Node>, leases: Vec<i64>, frames: Vec<Option<usize>>, now: i64) -> History {
    History {
        uploads: vec![None; frames.len()],
        nodes,
        leases,
        frames,
        now,
        retention: 10,
        batch: 2,
    }
}

#[test]
fn an_old_abandoned_changeset_dooms_its_objects() {
    let h = simple(
        vec![node(RecordState::Abandoned, 0, &[0, 1])],
        vec![],
        vec![Some(5), Some(7)],
        100,
    );
    assert_eq!(
        plan(&h),
        Report {
            changesets: 1,
            doomed: 2,
            deleted: 0,
            bytes: 12
        }
    );
    assert_eq!(plan(&h), brute(&h));
}

#[test]
fn a_shared_object_with_a_live_holder_is_kept() {
    // Changeset 1 is a live draft in a workspace leased past now, and it
    // also introduced object 0, which abandoned changeset 0 introduced too.
    let mut live = node(RecordState::Draft, 0, &[0]);
    live.holder = 0;
    let h = simple(
        vec![node(RecordState::Abandoned, 0, &[0, 1]), live],
        vec![1000],
        vec![Some(5), Some(7)],
        100,
    );
    assert_eq!(
        plan(&h),
        Report {
            changesets: 1,
            doomed: 1,
            deleted: 0,
            bytes: 7
        }
    );
    assert_eq!(plan(&h), brute(&h));
}

#[test]
fn a_superseded_changeset_is_kept_while_its_successor_is_live() {
    let mut old = node(RecordState::Superseded, 0, &[0]);
    old.successor = Some(1);
    let h = simple(
        vec![old, node(RecordState::Landed, 0, &[])],
        vec![],
        vec![Some(3)],
        100,
    );
    assert_eq!(
        plan(&h),
        Report {
            changesets: 0,
            doomed: 0,
            deleted: 0,
            bytes: 0
        }
    );
    assert_eq!(plan(&h), brute(&h));
}

#[test]
fn an_ancestor_of_a_live_draft_is_kept() {
    let mut child = node(RecordState::Draft, 0, &[]);
    child.parents = vec![0];
    child.holder = 0;
    let h = simple(
        vec![node(RecordState::Abandoned, 0, &[0]), child],
        vec![1000],
        vec![Some(4)],
        100,
    );
    assert_eq!(plan(&h).changesets, 0);
    assert_eq!(plan(&h), brute(&h));
}

#[test]
fn an_expired_upload_dooms_only_with_its_changeset_and_a_live_upload_keeps() {
    let mut h = simple(
        vec![node(RecordState::Abandoned, 0, &[0, 1])],
        vec![],
        vec![Some(3), Some(4)],
        50,
    );
    h.uploads = vec![Some(50), Some(51)];
    assert_eq!(
        plan(&h),
        Report {
            changesets: 1,
            doomed: 1,
            deleted: 0,
            bytes: 3
        }
    );
    assert_eq!(plan(&h), brute(&h));
}
