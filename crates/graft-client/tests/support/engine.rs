//! The real engine as the server for client and CLI tests: graft-engine over
//! the in-memory store, with faults and races a test injects.
//!
//! - Every call runs as one principal ([`EngineRepo::set_scopes`] narrows
//!   its scopes), whatever the transport says.
//! - [`EngineRepo::fail`] makes the next call of an operation fail, before
//!   the engine runs it ([`Fault::Before`]) or after, with the engine's
//!   answer lost ([`Fault::After`]): a reply dropped by the network.
//! - [`EngineRepo::race`] lands another writer's change on the line just
//!   before the next `Land`, so the land loses its first CAS and the engine
//!   rebases, or conflicts.
//! - `Revert` and `Stack` are G3.7 in the engine, so this wrapper answers
//!   them with a small model over the engine's own calls: the result is
//!   created without a workspace, and a stack operation then moves the
//!   head with `SetHead`. File-level only, as the engine's rebase is.
//!
//! The CLI tests include this file by path, so both crates test against the
//! same server.

#![allow(dead_code, unreachable_pub)]

use std::collections::{BTreeMap, HashMap, VecDeque};
use std::sync::{Arc, Mutex};

use graft_engine::{Engine, EngineConfig, LineConfig};
use graft_model::{
    Author, AuthorKind, Change, Changeset, ChangesetId, EntryKind, Metadata, ObjectId, ObjectKind,
    RepoPath, Timestamp, Tree, TreeId,
};
use graft_proto::api::{AdminApi, ObjectApi, RepoApi, WorkspaceApi};
use graft_proto::error::{ApiResult, ConflictPath};
use graft_proto::types::*;
use graft_proto::{ApiError, CallContext, Frame, Principal, PrincipalKind};
use graft_store::memory::MemoryStore;
use graft_testkit::clock::FakeClock;

/// How an injected fault fires.
#[derive(Clone, Debug)]
pub enum Fault {
    /// The call fails without running.
    Before(ApiError),
    /// The call runs, and its answer is replaced by this error.
    After(ApiError),
}

/// Another writer's change, landed on the line before the next `Land`:
/// paths to put (`Some`) or delete (`None`).
pub type Race = Vec<(String, Option<Vec<u8>>)>;

type Store = MemoryStore;

/// The engine and what the test injects.
pub struct EngineRepo {
    engine: Engine<Store, Store>,
    clock: Arc<FakeClock>,
    name: String,
    principal: Mutex<Principal>,
    faults: Mutex<HashMap<&'static str, VecDeque<Fault>>>,
    races: Mutex<VecDeque<Race>>,
    calls: Mutex<Vec<&'static str>>,
}

/// The checks decision D6 requires on `main`.
pub const D6_CHECKS: [&str; 2] = ["ci/build", "ci/test"];

fn path(s: &str) -> RepoPath {
    RepoPath::new(s).expect("valid test path")
}

fn writer() -> Author {
    Author {
        name: "Other Writer".to_owned(),
        email: "other@example.com".to_owned(),
        kind: AuthorKind::Human,
    }
}

impl EngineRepo {
    /// A repository `name` whose `main` starts at a root holding `files`,
    /// with the engine's default line configuration (decision D22: every
    /// line, `main` included, is `direct`).
    pub async fn new(name: &str, files: &[(&str, &[u8])]) -> Arc<EngineRepo> {
        Self::with_config(name, EngineConfig::default(), files).await
    }

    /// The same, with `main` opted into decision D6: `attested` on
    /// `ci/build` and `ci/test`, post-land with auto-revert.
    pub async fn d6(name: &str, files: &[(&str, &[u8])]) -> Arc<EngineRepo> {
        let config = EngineConfig {
            lines: vec![LineConfig::d6("main")],
            ..EngineConfig::default()
        };
        Self::with_config(name, config, files).await
    }

    /// The same, with `main` a `direct` line.
    pub async fn direct(name: &str, files: &[(&str, &[u8])]) -> Arc<EngineRepo> {
        let config = EngineConfig {
            lines: vec![LineConfig::direct("main")],
            ..EngineConfig::default()
        };
        Self::with_config(name, config, files).await
    }

    /// The same, with an engine configuration.
    pub async fn with_config(
        name: &str,
        config: EngineConfig,
        files: &[(&str, &[u8])],
    ) -> Arc<EngineRepo> {
        let store = Arc::new(MemoryStore::new());
        let clock = Arc::new(FakeClock::at(1_700_000_000_000));
        let engine = Engine::new(store.clone(), store, clock.clone(), config);
        let repo = EngineRepo {
            engine,
            clock,
            name: name.to_owned(),
            principal: Mutex::new(Principal {
                subject: "tester".to_owned(),
                kind: PrincipalKind::Human,
                session: None,
                work_item: None,
                scopes: vec![format!("repo:{name}:admin")],
            }),
            faults: Mutex::default(),
            races: Mutex::default(),
            calls: Mutex::default(),
        };
        let cx = repo.cx();
        repo.engine
            .create_repo(&cx, name.to_owned())
            .await
            .expect("create repo");
        let root = repo.write(Vec::new(), files_race(files)).await;
        repo.engine
            .update_ref(
                &cx,
                UpdateRef {
                    repo: name.to_owned(),
                    name: "main".to_owned(),
                    expected: None,
                    new: root,
                    kind: RefKind::Line,
                },
            )
            .await
            .expect("create main");
        Arc::new(repo)
    }

    /// The repository name.
    pub fn name(&self) -> &str {
        &self.name
    }

    /// The fake clock leases are measured on.
    pub fn clock(&self) -> &FakeClock {
        &self.clock
    }

    /// The caller's scopes from now on.
    pub fn set_scopes(&self, scopes: &[&str]) {
        self.principal.lock().expect("principal").scopes =
            scopes.iter().map(|s| (*s).to_owned()).collect();
    }

    /// Makes the next call of `op` (`"Land"`, `"Sync"`, ...) fail. Faults
    /// queue per operation and fire one per call.
    pub fn fail(&self, op: &'static str, fault: Fault) {
        self.faults
            .lock()
            .expect("faults")
            .entry(op)
            .or_default()
            .push_back(fault);
    }

    /// Drops every fault not yet fired.
    pub fn clear_faults(&self) {
        self.faults.lock().expect("faults").clear();
    }

    /// Lands `race` on `main` as another writer, just before the next
    /// `Land` runs.
    pub fn race(&self, race: Race) {
        self.races.lock().expect("races").push_back(race);
    }

    /// The operations called so far, in order.
    pub fn calls(&self) -> Vec<&'static str> {
        self.calls.lock().expect("calls").clone()
    }

    /// The context every call runs with.
    pub fn cx(&self) -> CallContext {
        CallContext {
            principal: self.principal.lock().expect("principal").clone(),
            request_id: "engine-repo".to_owned(),
            deadline_ms: None,
        }
    }

    /// An admin context for the test's own calls.
    fn admin(&self) -> CallContext {
        let mut cx = self.cx();
        cx.principal.scopes = vec![format!("repo:{}:admin", self.name)];
        cx.principal.kind = PrincipalKind::Ci;
        cx
    }

    /// Records `op` and fires a `Before` fault, or rewrites the context.
    fn enter(&self, op: &'static str, cx: &CallContext) -> ApiResult<CallContext> {
        self.calls.lock().expect("calls").push(op);
        let mut faults = self.faults.lock().expect("faults");
        let queue = faults.entry(op).or_default();
        if let Some(Fault::Before(_)) = queue.front()
            && let Some(Fault::Before(e)) = queue.pop_front()
        {
            return Err(e);
        }
        drop(faults);
        let mut out = self.cx();
        out.request_id = cx.request_id.clone();
        Ok(out)
    }

    /// Fires an `After` fault, discarding the engine's answer.
    fn leave<T>(&self, op: &'static str, out: ApiResult<T>) -> ApiResult<T> {
        let mut faults = self.faults.lock().expect("faults");
        let queue = faults.entry(op).or_default();
        if let Some(Fault::After(_)) = queue.front()
            && let Some(Fault::After(e)) = queue.pop_front()
        {
            return Err(e);
        }
        out
    }

    /// Runs the periodic lease sweep: ends every lapsed workspace.
    /// Returns how many it ended.
    pub async fn expire_leases(&self) -> u32 {
        self.engine
            .expire_leases(&self.name, 0)
            .await
            .expect("expire leases")
    }

    /// What collection would take now (the `Gc` dry run).
    pub async fn gc_dry_run(&self) -> GcReport {
        self.engine
            .gc(
                &self.admin(),
                Gc {
                    repo: self.name.clone(),
                    dry_run: true,
                },
            )
            .await
            .expect("gc dry run")
    }

    /// `main`'s tip.
    pub async fn tip(&self) -> ChangesetId {
        self.engine
            .resolve_ref(
                &self.admin(),
                ResolveRef {
                    repo: self.name.clone(),
                    name: "main".to_owned(),
                },
            )
            .await
            .expect("main")
            .target
    }

    /// Whether `id` is on `main`'s first-parent chain.
    pub async fn on_main(&self, id: ChangesetId) -> bool {
        let tip = self.tip().await;
        let view = self.changeset(id).await;
        let Some(view) = view else { return false };
        view.state == DraftState::Landed
            && self
                .engine
                .is_ancestor(
                    &self.admin(),
                    Pair {
                        repo: self.name.clone(),
                        a: id,
                        b: tip,
                    },
                )
                .await
                .expect("is_ancestor")
    }

    /// A changeset and its record.
    pub async fn changeset(&self, id: ChangesetId) -> Option<ChangesetView> {
        self.engine
            .get_changesets(
                &self.admin(),
                GetChangesets {
                    repo: self.name.clone(),
                    ids: vec![id],
                },
            )
            .await
            .expect("get_changesets")
            .found
            .pop()
    }

    /// Posts a passing attestation of every D6 check on `id`, as CI.
    pub async fn attest_all(&self, id: ChangesetId) {
        // CI attests as the OIDC exchange mints its token (WI-2272): `read`
        // and `attest`, pinned to the changeset. The engine refuses a `ci`
        // principal that is not pinned to it, even with `admin`.
        let mut ci = self.admin();
        ci.principal.scopes = vec![
            format!("repo:{}:read", self.name),
            format!("repo:{}:attest", self.name),
        ];
        ci.principal.subject = format!("ci:github:o/{}#{id}", self.name);
        for check in D6_CHECKS {
            self.engine
                .attest(
                    &ci,
                    Attest {
                        repo: self.name.clone(),
                        changeset: id,
                        check: check.to_owned(),
                        passed: true,
                        url: format!("https://ci.example/{check}"),
                    },
                )
                .await
                .expect("attest");
        }
    }

    /// Uploads the contents of `race` and creates a changeset on `parents`
    /// with no workspace.
    async fn write(&self, parents: Vec<ChangesetId>, race: Race) -> ChangesetId {
        let cx = self.admin();
        let blobs: Vec<Frame> = race
            .iter()
            .filter_map(|(_, b)| b.as_ref())
            .map(|b| Frame::encode(ObjectKind::Blob, b))
            .collect();
        if !blobs.is_empty() {
            self.engine
                .put_objects(
                    &cx,
                    PutObjects {
                        repo: self.name.clone(),
                        blobs,
                    },
                )
                .await
                .expect("upload");
        }
        let mut changes: Vec<Change> = race
            .into_iter()
            .map(|(p, b)| match b {
                Some(b) => Change::Put {
                    path: path(&p),
                    kind: EntryKind::File,
                    id: ObjectId::hash(ObjectKind::Blob, &b),
                },
                None => Change::Delete { path: path(&p) },
            })
            .collect();
        changes.sort_by(|a, b| a.path().cmp(b.path()));
        self.engine
            .create_changeset(
                &cx,
                CreateChangeset {
                    repo: self.name.clone(),
                    parents,
                    changes,
                    author: writer(),
                    authored_at: Timestamp {
                        ms: 1_700_000_000_000,
                        offset_min: 0,
                    },
                    message: "another writer".to_owned(),
                    metadata: Metadata::new(),
                    inline_blobs: Vec::new(),
                    workspace: None,
                },
            )
            .await
            .expect("commit")
            .id
    }

    /// Lands `race` on `main` as another writer (attested, as D6 needs).
    pub async fn advance(&self, race: Race) -> ChangesetId {
        let tip = self.tip().await;
        let id = self.write(vec![tip], race).await;
        self.attest_all(id).await;
        match self
            .engine
            .land(
                &self.admin(),
                Land {
                    repo: self.name.clone(),
                    head: id,
                    line: "main".to_owned(),
                    squash: false,
                    merge: MergeMode::File,
                    deadline_ms: None,
                },
            )
            .await
            .expect("land")
        {
            Landed::Landed { landed, .. } => landed,
            other => panic!("unexpected {other:?}"),
        }
    }

    /// Every file of snapshot `id`, by path.
    pub async fn files(&self, id: ChangesetId) -> BTreeMap<String, Vec<u8>> {
        let root = self.changeset(id).await.expect("changeset").changeset.root;
        let mut out = BTreeMap::new();
        let mut dirs: VecDeque<(String, TreeId)> = VecDeque::from([(String::new(), root)]);
        while let Some((dir, tree)) = dirs.pop_front() {
            let tree = Tree::decode(&self.object(tree.0).await).expect("tree");
            for e in tree.entries {
                let p = if dir.is_empty() {
                    e.name.clone()
                } else {
                    format!("{dir}/{}", e.name)
                };
                if e.kind.is_tree() {
                    dirs.push_back((p, TreeId(e.id)));
                } else {
                    out.insert(p, self.object(e.id).await);
                }
            }
        }
        out
    }

    async fn object(&self, id: ObjectId) -> Vec<u8> {
        let got = self
            .engine
            .get_objects(
                &self.admin(),
                GetObjects {
                    repo: self.name.clone(),
                    ids: vec![id],
                },
            )
            .await
            .expect("get_objects");
        let (_, frame) = got.found.into_iter().next().expect("object");
        frame.decode_verified(id).expect("verified")
    }

    /// The entry at `p` in snapshot `at`.
    async fn entry(
        &self,
        cx: &CallContext,
        at: ChangesetId,
        p: &RepoPath,
    ) -> ApiResult<Option<(EntryKind, ObjectId)>> {
        match self
            .engine
            .stat(
                cx,
                PathAt {
                    repo: self.name.clone(),
                    at,
                    path: p.clone(),
                },
            )
            .await
        {
            Ok(e) if e.kind.is_tree() => Ok(None),
            Ok(e) => Ok(Some((e.kind, e.id))),
            Err(ApiError::NotFound { .. }) => Ok(None),
            Err(e) => Err(e),
        }
    }

    async fn content(&self, cx: &CallContext, id: ChangesetId) -> ApiResult<Changeset> {
        self.engine
            .get_changesets(
                cx,
                GetChangesets {
                    repo: self.name.clone(),
                    ids: vec![id],
                },
            )
            .await?
            .found
            .pop()
            .map(|v| v.changeset)
            .ok_or_else(|| ApiError::NotFound {
                what: format!("changeset {id}"),
            })
    }

    /// The model `Revert`: the inverse of `req.changeset` against its first
    /// parent, on `req.onto`. A path `onto` changed since is a conflict.
    async fn model_revert(&self, cx: &CallContext, req: Revert) -> ApiResult<ChangesetId> {
        let cs = self.content(cx, req.changeset).await?;
        let parent = *cs.parents.first().ok_or_else(|| ApiError::InvalidChange {
            path: String::new(),
            rule: "revert-root".to_owned(),
        })?;
        let mut changes = Vec::new();
        let mut conflicts = Vec::new();
        for c in &cs.changes {
            let p = c.path();
            let before = self.entry(cx, parent, p).await?;
            let after = match c {
                Change::Put { kind, id, .. } => Some((*kind, *id)),
                Change::Delete { .. } => None,
            };
            let now = self.entry(cx, req.onto, p).await?;
            if now != after {
                conflicts.push(ConflictPath {
                    path: p.to_string(),
                    base: after.map(|(_, id)| id),
                    ours: before.map(|(_, id)| id),
                    theirs: now.map(|(_, id)| id),
                });
                continue;
            }
            changes.push(match before {
                Some((kind, id)) => Change::Put {
                    path: p.clone(),
                    kind,
                    id,
                },
                None => Change::Delete { path: p.clone() },
            });
        }
        if !conflicts.is_empty() {
            return Err(ApiError::Conflict {
                paths: conflicts,
                onto: req.onto,
            });
        }
        let subject = cs.message.lines().next().unwrap_or("").to_owned();
        let template = Changeset {
            message: format!("Revert \"{subject}\"\n\nThis reverts {}.", req.changeset),
            ..cs
        };
        self.replay(cx, req.onto, changes, &template).await
    }

    /// Creates `changes` on `parent` with `template`'s author, time,
    /// message and metadata, and no workspace. No-op changes are dropped;
    /// a change that no longer applies is a conflict.
    async fn replay(
        &self,
        cx: &CallContext,
        parent: ChangesetId,
        changes: Vec<Change>,
        template: &Changeset,
    ) -> ApiResult<ChangesetId> {
        let mut kept = Vec::new();
        for c in changes {
            let now = self.entry(cx, parent, c.path()).await?;
            let noop = match &c {
                Change::Put { kind, id, .. } => now == Some((*kind, *id)),
                Change::Delete { .. } => now.is_none(),
            };
            if !noop {
                kept.push(c);
            }
        }
        if kept.is_empty() {
            return Ok(parent);
        }
        kept.sort_by(|a, b| a.path().cmp(b.path()));
        let created = self
            .engine
            .create_changeset(
                cx,
                CreateChangeset {
                    repo: self.name.clone(),
                    parents: vec![parent],
                    changes: kept,
                    author: template.author.clone(),
                    authored_at: template.authored_at,
                    message: template.message.clone(),
                    metadata: template.metadata.clone(),
                    inline_blobs: Vec::new(),
                    workspace: None,
                },
            )
            .await;
        match created {
            Ok(c) => Ok(c.id),
            Err(ApiError::InvalidChange { path, .. }) => Err(ApiError::Conflict {
                paths: vec![ConflictPath {
                    path,
                    base: None,
                    ours: None,
                    theirs: None,
                }],
                onto: parent,
            }),
            Err(e) => Err(e),
        }
    }

    /// The model `Stack`: rebuilds the stack in its new shape on the base,
    /// then moves the head to it.
    async fn model_stack(
        &self,
        cx: &CallContext,
        req: WorkspaceRef,
        op: StackOp,
    ) -> ApiResult<WorkspaceView> {
        let ws = self.engine.get_workspace(cx, req.clone()).await?;
        let invalid = |rule: &str| ApiError::InvalidChange {
            path: String::new(),
            rule: rule.to_owned(),
        };
        // The drafts, bottom to top.
        let mut drafts: Vec<(ChangesetId, Changeset)> = Vec::new();
        let mut cur = ws.head;
        while let Some(id) = cur.filter(|id| *id != ws.base) {
            let cs = self.content(cx, id).await?;
            cur = cs.parents.first().copied();
            drafts.push((id, cs));
        }
        drafts.reverse();
        if drafts.is_empty() {
            return Err(invalid("no-drafts"));
        }
        let index = |id: &ChangesetId| drafts.iter().position(|(d, _)| d == id);
        // Each new draft: its changes and the changeset it takes its
        // author, time, message and metadata from.
        let mut shape: Vec<(Vec<Change>, Changeset)> = Vec::new();
        match op {
            StackOp::Reorder(order) => {
                let mut seen: Vec<usize> = order.iter().filter_map(index).collect();
                seen.sort_unstable();
                seen.dedup();
                if order.len() != drafts.len() || seen.len() != drafts.len() {
                    return Err(invalid("reorder-not-a-permutation"));
                }
                for id in &order {
                    let (_, cs) = &drafts[index(id).expect("checked")];
                    shape.push((cs.changes.clone(), cs.clone()));
                }
            }
            StackOp::Squash(ids) => {
                let mut at: Vec<usize> = ids.iter().filter_map(index).collect();
                at.sort_unstable();
                at.dedup();
                if at.len() < 2 || at.len() != ids.len() || at.windows(2).any(|w| w[1] != w[0] + 1)
                {
                    return Err(invalid("squash-not-adjacent"));
                }
                let (first, last) = (at[0], at[at.len() - 1]);
                for (i, (_, cs)) in drafts.iter().enumerate() {
                    if i < first || i > last {
                        shape.push((cs.changes.clone(), cs.clone()));
                    } else if i == last {
                        let mut net: BTreeMap<RepoPath, Change> = BTreeMap::new();
                        for (_, d) in &drafts[first..=last] {
                            for c in &d.changes {
                                net.insert(c.path().clone(), c.clone());
                            }
                        }
                        let message = drafts[first..=last]
                            .iter()
                            .map(|(_, d)| d.message.as_str())
                            .collect::<Vec<_>>()
                            .join("\n\n");
                        let template = Changeset {
                            message,
                            ..cs.clone()
                        };
                        shape.push((net.into_values().collect(), template));
                    }
                }
            }
            StackOp::Split { draft, groups } => {
                let at = index(&draft).ok_or_else(|| invalid("split-not-in-stack"))?;
                for (i, (_, cs)) in drafts.iter().enumerate() {
                    if i != at {
                        shape.push((cs.changes.clone(), cs.clone()));
                        continue;
                    }
                    let mut rest = cs.changes.clone();
                    let mut parts = Vec::new();
                    for group in &groups {
                        let (inside, outside): (Vec<Change>, Vec<Change>) = rest
                            .into_iter()
                            .partition(|c| group.iter().any(|g| c.path().starts_with(g)));
                        rest = outside;
                        if !inside.is_empty() {
                            parts.push(inside);
                        }
                    }
                    if !rest.is_empty() {
                        parts.push(rest);
                    }
                    if parts.len() < 2 {
                        return Err(invalid("split-one-part"));
                    }
                    let n = parts.len();
                    for (k, part) in parts.into_iter().enumerate() {
                        let template = Changeset {
                            message: format!("{} (part {}/{n})", cs.message, k + 1),
                            ..cs.clone()
                        };
                        shape.push((part, template));
                    }
                }
            }
            StackOp::Drop(id) => {
                let at = index(&id).ok_or_else(|| invalid("drop-not-in-stack"))?;
                for (i, (_, cs)) in drafts.iter().enumerate() {
                    if i != at {
                        shape.push((cs.changes.clone(), cs.clone()));
                    }
                }
            }
            _ => return Err(invalid("stack-op")),
        }
        let mut parent = ws.base;
        for (changes, template) in shape {
            parent = self.replay(cx, parent, changes, &template).await?;
        }
        let new = (parent != ws.base).then_some(parent);
        self.engine
            .set_head(
                cx,
                SetHead {
                    workspace: req,
                    expected: ws.head,
                    new,
                },
            )
            .await
    }

    /// Lands the queued races.
    async fn run_races(&self) {
        loop {
            let next = self.races.lock().expect("races").pop_front();
            match next {
                Some(race) => {
                    self.advance(race).await;
                }
                None => return,
            }
        }
    }
}

/// `files` as a race of puts.
pub fn files_race(files: &[(&str, &[u8])]) -> Race {
    files
        .iter()
        .map(|(p, b)| ((*p).to_owned(), Some(b.to_vec())))
        .collect()
}

/// One delegated call: fault, engine, fault.
macro_rules! delegate {
    ($self:ident, $op:literal, $cx:ident, |$c:ident| $call:expr) => {{
        let $c = $self.enter($op, $cx)?;
        let out = $call;
        $self.leave($op, out)
    }};
}

impl ObjectApi for EngineRepo {
    async fn get_objects(&self, cx: &CallContext, req: GetObjects) -> ApiResult<Objects> {
        delegate!(self, "GetObjects", cx, |c| self
            .engine
            .get_objects(&c, req)
            .await)
    }
    async fn has_objects(&self, cx: &CallContext, req: HasObjects) -> ApiResult<Vec<ObjectId>> {
        delegate!(self, "HasObjects", cx, |c| self
            .engine
            .has_objects(&c, req)
            .await)
    }
    async fn put_objects(&self, cx: &CallContext, req: PutObjects) -> ApiResult<Vec<ObjectId>> {
        delegate!(self, "PutObjects", cx, |c| self
            .engine
            .put_objects(&c, req)
            .await)
    }
}

impl RepoApi for EngineRepo {
    async fn create_changeset(&self, cx: &CallContext, req: CreateChangeset) -> ApiResult<Created> {
        delegate!(self, "CreateChangeset", cx, |c| self
            .engine
            .create_changeset(&c, req)
            .await)
    }
    async fn get_changesets(&self, cx: &CallContext, req: GetChangesets) -> ApiResult<Changesets> {
        delegate!(self, "GetChangesets", cx, |c| self
            .engine
            .get_changesets(&c, req)
            .await)
    }
    async fn resolve_ref(&self, cx: &CallContext, req: ResolveRef) -> ApiResult<RefView> {
        delegate!(self, "ResolveRef", cx, |c| self
            .engine
            .resolve_ref(&c, req)
            .await)
    }
    async fn list_refs(&self, cx: &CallContext, req: ListRefs) -> ApiResult<Vec<RefView>> {
        delegate!(self, "ListRefs", cx, |c| self
            .engine
            .list_refs(&c, req)
            .await)
    }
    async fn update_ref(&self, cx: &CallContext, req: UpdateRef) -> ApiResult<RefView> {
        delegate!(self, "UpdateRef", cx, |c| self
            .engine
            .update_ref(&c, req)
            .await)
    }
    async fn create_line(&self, cx: &CallContext, req: CreateLine) -> ApiResult<RefView> {
        delegate!(self, "CreateLine", cx, |c| self
            .engine
            .create_line(&c, req)
            .await)
    }
    async fn stat(&self, cx: &CallContext, req: PathAt) -> ApiResult<EntryView> {
        delegate!(self, "Stat", cx, |c| self.engine.stat(&c, req).await)
    }
    async fn list_tree(&self, cx: &CallContext, req: ListTree) -> ApiResult<Entries> {
        delegate!(self, "ListTree", cx, |c| self
            .engine
            .list_tree(&c, req)
            .await)
    }
    async fn read_file(&self, cx: &CallContext, req: PathAt) -> ApiResult<FileContent> {
        delegate!(self, "ReadFile", cx, |c| self
            .engine
            .read_file(&c, req)
            .await)
    }
    async fn log(&self, cx: &CallContext, req: Log) -> ApiResult<History> {
        delegate!(self, "Log", cx, |c| self.engine.log(&c, req).await)
    }
    async fn is_ancestor(&self, cx: &CallContext, req: Pair) -> ApiResult<bool> {
        delegate!(self, "IsAncestor", cx, |c| self
            .engine
            .is_ancestor(&c, req)
            .await)
    }
    async fn merge_base(&self, cx: &CallContext, req: Pair) -> ApiResult<Option<ChangesetId>> {
        delegate!(self, "MergeBase", cx, |c| self
            .engine
            .merge_base(&c, req)
            .await)
    }
    async fn diff(&self, cx: &CallContext, req: Diff) -> ApiResult<DiffResult> {
        delegate!(self, "Diff", cx, |c| self.engine.diff(&c, req).await)
    }
    async fn blame(&self, cx: &CallContext, req: Blame) -> ApiResult<BlameResult> {
        delegate!(self, "Blame", cx, |c| self.engine.blame(&c, req).await)
    }
    async fn rebase(&self, cx: &CallContext, req: Rebase) -> ApiResult<ChangesetId> {
        delegate!(self, "Rebase", cx, |c| self.engine.rebase(&c, req).await)
    }
    async fn revert(&self, cx: &CallContext, req: Revert) -> ApiResult<ChangesetId> {
        delegate!(self, "Revert", cx, |c| self.model_revert(&c, req).await)
    }
    async fn land(&self, cx: &CallContext, req: Land) -> ApiResult<Landed> {
        let c = self.enter("Land", cx)?;
        self.run_races().await;
        let out = self.engine.land(&c, req).await;
        self.leave("Land", out)
    }
    async fn attest(&self, cx: &CallContext, req: Attest) -> ApiResult<()> {
        delegate!(self, "Attest", cx, |c| self.engine.attest(&c, req).await)
    }
    async fn watch(&self, cx: &CallContext, req: Watch) -> ApiResult<JournalPage> {
        delegate!(self, "Watch", cx, |c| self.engine.watch(&c, req).await)
    }
}

impl WorkspaceApi for EngineRepo {
    async fn open_workspace(
        &self,
        cx: &CallContext,
        req: OpenWorkspace,
    ) -> ApiResult<WorkspaceView> {
        delegate!(self, "OpenWorkspace", cx, |c| self
            .engine
            .open_workspace(&c, req)
            .await)
    }
    async fn get_workspace(&self, cx: &CallContext, req: WorkspaceRef) -> ApiResult<WorkspaceView> {
        delegate!(self, "GetWorkspace", cx, |c| self
            .engine
            .get_workspace(&c, req)
            .await)
    }
    async fn list_workspaces(
        &self,
        cx: &CallContext,
        repo: String,
        page: Page,
    ) -> ApiResult<Vec<WorkspaceView>> {
        delegate!(self, "ListWorkspaces", cx, |c| self
            .engine
            .list_workspaces(&c, repo, page)
            .await)
    }
    async fn renew_lease(
        &self,
        cx: &CallContext,
        req: WorkspaceRef,
        lease_ms: u64,
    ) -> ApiResult<WorkspaceView> {
        delegate!(self, "RenewLease", cx, |c| self
            .engine
            .renew_lease(&c, req, lease_ms)
            .await)
    }
    async fn set_head(&self, cx: &CallContext, req: SetHead) -> ApiResult<WorkspaceView> {
        delegate!(self, "SetHead", cx, |c| self.engine.set_head(&c, req).await)
    }
    async fn sync(
        &self,
        cx: &CallContext,
        req: WorkspaceRef,
        onto: Option<ChangesetId>,
    ) -> ApiResult<Synced> {
        delegate!(self, "Sync", cx, |c| self.engine.sync(&c, req, onto).await)
    }
    async fn stack(
        &self,
        cx: &CallContext,
        req: WorkspaceRef,
        op: StackOp,
    ) -> ApiResult<WorkspaceView> {
        delegate!(self, "Stack", cx, |c| self.model_stack(&c, req, op).await)
    }
    async fn close_workspace(
        &self,
        cx: &CallContext,
        req: WorkspaceRef,
        keep_drafts: bool,
    ) -> ApiResult<()> {
        delegate!(self, "CloseWorkspace", cx, |c| self
            .engine
            .close_workspace(&c, req, keep_drafts)
            .await)
    }

    async fn get_conflicts(
        &self,
        cx: &graft_proto::CallContext,
        req: graft_proto::types::GetConflicts,
    ) -> graft_proto::error::ApiResult<graft_proto::types::Conflicts> {
        delegate!(self, "GetConflicts", cx, |c| self
            .engine
            .get_conflicts(&c, req)
            .await)
    }

    async fn resolve(
        &self,
        _cx: &graft_proto::CallContext,
        _req: graft_proto::types::Resolve,
    ) -> graft_proto::error::ApiResult<graft_proto::types::Resolved> {
        Err(graft_proto::ApiError::unimplemented("Resolve"))
    }
}
