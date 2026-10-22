//! G2.1 model test: random sequences of workspace operations against a
//! reference model, on both stores. After every operation the engine's
//! workspaces and records must equal the model's, every draft must have a
//! holder whose stack contains it, and the gauges must equal a brute-force
//! count.

mod common;

use std::collections::BTreeMap;

use common::*;
use graft_engine::{EngineConfig, Gauges};
use graft_model::ChangesetId;
use graft_proto::error::ApiResult;
use graft_proto::types::*;
use graft_proto::{CallContext, RepoApi, WorkspaceApi};
use graft_store::{MetaStore, RecordState};
use proptest::prelude::*;

const NAMES: [&str; 3] = ["a", "b", "c"];
const CONTENTS: [&[u8]; 3] = [b"1", b"2", b"3"];
const MAX_STACK: u32 = 4;
const MAX_LEASE: u64 = 10_000;

#[derive(Clone, Debug)]
enum Op {
    Open {
        ws: usize,
        lease: u64,
    },
    /// Into a workspace: on its head (or base), or on a known changeset.
    Commit {
        ws: usize,
        on_head: bool,
        pick: usize,
        content: usize,
    },
    /// Without a workspace, on a known changeset.
    Loose {
        pick: usize,
        content: usize,
    },
    SetHead {
        ws: usize,
        stale: bool,
        new: Option<usize>,
    },
    Renew {
        ws: usize,
        lease: u64,
    },
    Close {
        ws: usize,
    },
    Tick {
        ms: i64,
    },
    Expire,
    /// Fast-forward `main` to a workspace head.
    Land {
        ws: usize,
    },
}

fn op() -> impl Strategy<Value = Op> {
    let ws = 0..NAMES.len();
    let pick = any::<prop::sample::Index>().prop_map(|i| i.index(1 << 16));
    prop_oneof![
        2 => (ws.clone(), 1..MAX_LEASE * 2).prop_map(|(ws, lease)| Op::Open { ws, lease }),
        4 => (ws.clone(), prop::bool::weighted(0.8), pick.clone(), 0..CONTENTS.len())
            .prop_map(|(ws, on_head, pick, content)| Op::Commit { ws, on_head, pick, content }),
        1 => (pick.clone(), 0..CONTENTS.len()).prop_map(|(pick, content)| Op::Loose { pick, content }),
        3 => (ws.clone(), prop::bool::weighted(0.2), prop::option::weighted(0.8, pick))
            .prop_map(|(ws, stale, new)| Op::SetHead { ws, stale, new }),
        1 => (ws.clone(), 1..MAX_LEASE).prop_map(|(ws, lease)| Op::Renew { ws, lease }),
        1 => ws.clone().prop_map(|ws| Op::Close { ws }),
        2 => (1..MAX_LEASE as i64 / 4).prop_map(|ms| Op::Tick { ms }),
        1 => Just(Op::Expire),
        1 => ws.prop_map(|ws| Op::Land { ws }),
    ]
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct MWs {
    base: ChangesetId,
    head: Option<ChangesetId>,
    lease_until: i64,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct MRec {
    state: RecordState,
    holder: Option<String>,
    superseded_by: Option<ChangesetId>,
}

/// The reference model.
struct Model {
    now: i64,
    tip: ChangesetId,
    ws: BTreeMap<String, MWs>,
    recs: BTreeMap<ChangesetId, MRec>,
    parent: BTreeMap<ChangesetId, Option<ChangesetId>>,
    /// Known ids, in creation order, for picking.
    ids: Vec<ChangesetId>,
    /// `(parent, content)` → id: the ids are content-addressed.
    made: BTreeMap<(ChangesetId, usize), ChangesetId>,
    /// The content of `f` in each changeset (none for the root's).
    content: BTreeMap<ChangesetId, usize>,
}

impl Model {
    /// A put that changes nothing is not a valid change.
    fn changes(&self, parent: ChangesetId, content: usize) -> bool {
        self.content.get(&parent) != Some(&content)
    }

    fn lapsed(&self, name: &str) -> bool {
        self.ws.get(name).is_some_and(|w| w.lease_until < self.now)
    }

    fn held_by(&self, id: ChangesetId, name: &str) -> bool {
        self.recs.get(&id).is_some_and(|r| {
            r.state == RecordState::Draft && r.holder.as_deref() == Some(&format!("ws:{name}"))
        })
    }

    /// The stack of `head` over `base`, top first; `None` unless it stands
    /// on `base` within `MAX_STACK`.
    fn stack(&self, base: ChangesetId, head: Option<ChangesetId>) -> Option<Vec<ChangesetId>> {
        let mut out = Vec::new();
        let mut cur = head;
        while let Some(id) = cur {
            if id == base {
                return (!out.is_empty() || head.is_none()).then_some(out);
            }
            if out.len() == MAX_STACK as usize {
                return None;
            }
            out.push(id);
            cur = self.parent[&id];
        }
        head.is_none().then_some(out)
    }

    fn set(
        &mut self,
        id: ChangesetId,
        state: RecordState,
        holder: Option<String>,
        by: Option<ChangesetId>,
    ) {
        self.recs.insert(
            id,
            MRec {
                state,
                holder,
                superseded_by: by,
            },
        );
    }

    /// Ends a workspace: its held drafts become abandoned.
    fn end(&mut self, name: &str) {
        let w = self.ws.remove(name).expect("present");
        for id in self.stack(w.base, w.head).expect("valid stack") {
            if self.held_by(id, name) {
                self.set(id, RecordState::Abandoned, None, None);
            }
        }
    }

    /// A live workspace, ending a lapsed one; `false` if there is none.
    fn live(&mut self, name: &str) -> bool {
        if self.lapsed(name) {
            self.end(name);
            return false;
        }
        self.ws.contains_key(name)
    }

    fn lease(&self, ms: u64) -> i64 {
        self.now + ms.min(MAX_LEASE) as i64
    }

    fn pick(&self, i: usize) -> ChangesetId {
        self.ids[i % self.ids.len()]
    }
}

struct Run<'f, M> {
    repo: Repo<'f, M>,
    cx: CallContext,
    m: Model,
}

impl<'f, M: MetaStore> Run<'f, M> {
    fn wref(&self, ws: usize) -> WorkspaceRef {
        WorkspaceRef {
            repo: "m".to_owned(),
            name: NAMES[ws].to_owned(),
        }
    }

    fn commit(&self, parent: ChangesetId, content: usize, ws: Option<&str>) -> ApiResult<Created> {
        let mut req = commit_req("m", vec![parent], vec![put("f", CONTENTS[content])]);
        req.workspace = ws.map(str::to_owned);
        block_on(self.repo.e().create_changeset(&self.cx, req))
    }

    /// Records a new id the engine returned, or checks a known one.
    fn learn(&mut self, parent: ChangesetId, content: usize, id: ChangesetId) {
        let known = *self.m.made.entry((parent, content)).or_insert(id);
        assert_eq!(known, id, "ids are content-addressed");
        if let std::collections::btree_map::Entry::Vacant(slot) = self.m.parent.entry(id) {
            slot.insert(Some(parent));
            self.m.content.insert(id, content);
            self.m.ids.push(id);
        }
    }

    /// Applies `op` to the engine and the model; returns whether the engine
    /// succeeded and whether the model says it should.
    fn apply(&mut self, op: &Op) -> (bool, bool) {
        let e = self.repo.e();
        match *op {
            Op::Open { ws, lease } => {
                let name = NAMES[ws];
                let got = block_on(e.open_workspace(
                    &self.cx,
                    OpenWorkspace {
                        repo: "m".to_owned(),
                        name: Some(name.to_owned()),
                        line: "main".to_owned(),
                        at: None,
                        lease_ms: lease,
                    },
                ));
                if self.m.lapsed(name) {
                    self.m.end(name);
                }
                if !self.m.ws.contains_key(name) {
                    let w = MWs {
                        base: self.m.tip,
                        head: None,
                        lease_until: self.m.lease(lease),
                    };
                    self.m.ws.insert(name.to_owned(), w);
                }
                (got.is_ok(), true)
            }
            Op::Commit {
                ws,
                on_head,
                pick,
                content,
            } => {
                let name = NAMES[ws];
                let parent = match self.m.ws.get(name) {
                    Some(w) if on_head => w.head.unwrap_or(w.base),
                    _ => self.m.pick(pick),
                };
                let got = self.commit(parent, content, Some(name));
                if !self.m.live(name) || !self.m.changes(parent, content) {
                    return (got.is_ok(), false);
                }
                let w = self.m.ws[name].clone();
                let at_head = parent == w.head.unwrap_or(w.base);
                let deep = w.head.is_some()
                    && self.m.stack(w.base, w.head).expect("valid").len() >= MAX_STACK as usize;
                let expect = match self.m.made.get(&(parent, content)) {
                    // A retry of our own draft.
                    Some(id) if self.m.held_by(*id, name) => true,
                    Some(id) => {
                        self.m.recs.get(id).map(|r| r.state) == Some(RecordState::Abandoned)
                            && at_head
                            && !deep
                    }
                    None => at_head && !deep,
                };
                if let Ok(c) = &got {
                    self.learn(parent, content, c.id);
                }
                if expect {
                    let id = got.as_ref().expect("model says ok").id;
                    if !self.m.held_by(id, name) {
                        self.m
                            .set(id, RecordState::Draft, Some(format!("ws:{name}")), None);
                        self.m.ws.get_mut(name).expect("ws").head = Some(id);
                    }
                }
                (got.is_ok(), expect)
            }
            Op::Loose { pick, content } => {
                let parent = self.m.pick(pick);
                let got = self.commit(parent, content, None);
                let valid = self.m.changes(parent, content);
                if let Ok(c) = &got {
                    if !self.m.recs.contains_key(&c.id) {
                        self.m.set(c.id, RecordState::Abandoned, None, None);
                    }
                    self.learn(parent, content, c.id);
                }
                (got.is_ok(), valid)
            }
            Op::SetHead { ws, stale, new } => {
                let name = NAMES[ws];
                let current = self.m.ws.get(name).and_then(|w| w.head);
                let expected = if stale {
                    match current {
                        Some(_) => None,
                        None => Some(self.m.tip),
                    }
                } else {
                    current
                };
                let new = new.map(|i| self.m.pick(i));
                let got = block_on(e.set_head(
                    &self.cx,
                    SetHead {
                        workspace: self.wref(ws),
                        expected,
                        new,
                    },
                ));
                if !self.m.live(name) {
                    return (got.is_ok(), false);
                }
                let w = self.m.ws[name].clone();
                if w.head != expected {
                    return (got.is_ok(), false);
                }
                if new == w.head {
                    return (got.is_ok(), true);
                }
                let old = self.m.stack(w.base, w.head).expect("valid");
                let Some(fresh) = self.m.stack(w.base, new) else {
                    return (got.is_ok(), false);
                };
                if new.is_some() && fresh.is_empty() {
                    return (got.is_ok(), false);
                }
                let joins: Vec<ChangesetId> = fresh
                    .iter()
                    .copied()
                    .filter(|id| !old.contains(id) && !self.m.held_by(*id, name))
                    .collect();
                if joins
                    .iter()
                    .any(|id| self.m.recs[id].state != RecordState::Abandoned)
                {
                    return (got.is_ok(), false);
                }
                for id in joins {
                    self.m
                        .set(id, RecordState::Draft, Some(format!("ws:{name}")), None);
                }
                let successor = new.filter(|n| !old.contains(n));
                for id in old {
                    if !fresh.contains(&id) && self.m.held_by(id, name) {
                        match successor {
                            Some(n) => self.m.set(id, RecordState::Superseded, None, Some(n)),
                            None => self.m.set(id, RecordState::Abandoned, None, None),
                        }
                    }
                }
                self.m.ws.get_mut(name).expect("ws").head = new;
                (got.is_ok(), true)
            }
            Op::Renew { ws, lease } => {
                let got = block_on(e.renew_lease(&self.cx, self.wref(ws), lease));
                let name = NAMES[ws];
                if !self.m.live(name) {
                    return (got.is_ok(), false);
                }
                let until = self.m.lease(lease);
                self.m.ws.get_mut(name).expect("ws").lease_until = until;
                (got.is_ok(), true)
            }
            Op::Close { ws } => {
                let got = block_on(e.close_workspace(&self.cx, self.wref(ws), false));
                if self.m.ws.contains_key(NAMES[ws]) {
                    self.m.end(NAMES[ws]);
                }
                (got.is_ok(), true)
            }
            Op::Tick { ms } => {
                self.repo.f.clock.advance(ms);
                self.m.now += ms;
                (true, true)
            }
            Op::Expire => {
                let got = block_on(e.expire_leases("m", 0));
                let lapsed: Vec<String> = self
                    .m
                    .ws
                    .keys()
                    .filter(|n| self.m.lapsed(n))
                    .cloned()
                    .collect();
                let n = lapsed.len() as u32;
                for name in lapsed {
                    self.m.end(&name);
                }
                assert_eq!(got.as_ref().ok(), Some(&n));
                (got.is_ok(), true)
            }
            Op::Land { ws } => {
                let Some(head) = self.m.ws.get(NAMES[ws]).and_then(|w| w.head) else {
                    return (true, true);
                };
                let got = self
                    .repo
                    .update_ref("main", Some(self.m.tip), head, RefKind::Line);
                // A fast-forward: the tip is on the head's first-parent chain.
                let mut chain = Vec::new();
                let mut cur = Some(head);
                while let Some(id) = cur.filter(|id| *id != self.m.tip) {
                    chain.push(id);
                    cur = self.m.parent[&id];
                }
                if cur.is_none() {
                    return (got.is_ok(), false);
                }
                for id in chain {
                    let by = self.m.recs[&id].superseded_by;
                    self.m.set(id, RecordState::Landed, None, by);
                }
                self.m.tip = head;
                (got.is_ok(), true)
            }
        }
    }

    /// The engine equals the model, every draft is held by a workspace whose
    /// stack contains it, and the gauges equal a brute-force count.
    fn check(&self) {
        let e = self.repo.e();
        for (i, name) in NAMES.iter().enumerate() {
            let got = block_on(e.get_workspace(&self.cx, self.wref(i))).ok();
            let got = got.map(|v| MWs {
                base: v.base,
                head: v.head,
                lease_until: v.lease_until_ms,
            });
            assert_eq!(got.as_ref(), self.m.ws.get(*name), "workspace {name}");
        }
        let (meta, _) = e.stores();
        let repo = block_on(meta.repo("m")).expect("repo").expect("exists");
        let mut brute = Gauges::default();
        for (id, want) in &self.m.recs {
            let (_, rec) = block_on(meta.changeset(repo, *id))
                .expect("read")
                .expect("stored");
            let got = MRec {
                state: rec.state,
                holder: rec.holder.clone(),
                superseded_by: rec.superseded_by,
            };
            assert_eq!(&got, want, "record {id}");
            match rec.state {
                RecordState::Draft => {
                    brute.held += 1;
                    let name = rec.holder.as_deref().and_then(|h| h.strip_prefix("ws:"));
                    let w = name
                        .and_then(|n| self.m.ws.get(n))
                        .expect("a draft has a holder");
                    let stack = self.m.stack(w.base, w.head).expect("valid");
                    assert!(stack.contains(id), "draft {id} is in its holder's stack");
                }
                RecordState::Abandoned => brute.abandoned += 1,
                RecordState::Superseded => brute.superseded += 1,
                RecordState::Landed | RecordState::Collected => {}
            }
        }
        for w in self.m.ws.values() {
            if w.lease_until < self.m.now {
                brute.lapsed += 1;
            } else {
                brute.active += 1;
            }
        }
        assert_eq!(block_on(e.gauges("m")).expect("gauges"), brute);
    }
}

fn run<M: MetaStore>(f: &Fixture<M>, ops: &[Op]) -> Result<(), TestCaseError> {
    let repo = Repo::new(f, "m");
    repo.upload(&CONTENTS);
    let root = repo.commit(vec![], &[("f", b"0")], &[]).id;
    repo.update_ref("main", None, root, RefKind::Line)
        .expect("main");
    let cx = repo.cx.clone();
    let mut model = Model {
        now: 1_000_000,
        tip: root,
        ws: BTreeMap::new(),
        recs: BTreeMap::new(),
        parent: BTreeMap::new(),
        ids: vec![root],
        made: BTreeMap::new(),
        content: BTreeMap::new(),
    };
    model.parent.insert(root, None);
    model.set(root, RecordState::Landed, None, None);
    let mut r = Run { repo, cx, m: model };
    for (i, op) in ops.iter().enumerate() {
        let (got, want) = r.apply(op);
        prop_assert_eq!(got, want, "op {} {:?}", i, op);
        r.check();
    }
    Ok(())
}

fn config() -> EngineConfig {
    EngineConfig {
        max_stack: MAX_STACK,
        max_lease_ms: MAX_LEASE,
        ..EngineConfig::default()
    }
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(128))]

    #[test]
    fn memory_store_matches_the_model(ops in prop::collection::vec(op(), 1..60)) {
        run(&memory_with(config()), &ops)?;
    }
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(24))]

    #[test]
    fn sqlite_store_matches_the_model(ops in prop::collection::vec(op(), 1..40)) {
        run(&sqlite_with(config()), &ops)?;
    }
}
