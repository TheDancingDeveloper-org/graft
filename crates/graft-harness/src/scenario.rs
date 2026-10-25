//! One adversarial run: the plan drawn from a seed, the clients, the
//! adversary's events, and the history they leave (§3.1, §3.2).
//!
//! Clients race to land *intents* on line `trunk`. An intent writes the
//! client's token into one or two regions of the shared file, or into the
//! client's own file. A region holds the sorted set of tokens written to
//! it, and a client always writes the union of what it read and its own
//! token. So the final value of a region does not depend on the order of
//! the lands: it is the set of tokens of every intent that landed (§3.3
//! property 4), and the model needs no merge algorithm.
//!
//! Regions are separated by [`REGION_GAP`] unchanged lines, so two intents
//! on different regions should merge by line (D16) and two on one region
//! conflict. On a conflict, the client drops its draft, syncs and writes a
//! new root draft for the same intent: the resolution.

use std::cell::RefCell;
use std::collections::BTreeSet;
use std::rc::Rc;
use std::sync::Arc;

use graft_engine::{Engine, EngineConfig, GcSettings};
use graft_gc::{GcConfig, SweepLeases};
use graft_model::{
    Author, AuthorKind, Change, ChangesetId, EntryKind, Metadata, ObjectId, ObjectKind, RepoPath,
    Timestamp,
};
use graft_proto::types::*;
use graft_proto::{
    AdminApi, ApiError, CallContext, Frame, ObjectApi, Principal, PrincipalKind, RepoApi,
    WorkspaceApi,
};
use graft_store::MetaStore;
use graft_store::memory::MemoryStore;
use graft_testkit::clock::FakeClock;

use crate::exec::{Scheduler, Yield, YieldPace, YieldSleeper, run_local};
use crate::faulty::{FaultRates, Faults, FaultyMeta, FaultyObjects, WrapperBug};
use crate::rng::Rng;

/// The repository.
pub const REPO: &str = "r";
/// The line every client lands on.
pub const LINE: &str = "trunk";
/// The shared file.
pub const SHARED: &str = "shared.txt";
/// Regions in the shared file.
pub const REGIONS: usize = 6;
/// Unchanged lines between two regions.
pub const REGION_GAP: usize = 3;
/// The token every region starts with.
pub const BASE_TOKEN: &str = "base";
/// Workspace lease asked for, ms (shorter than the clock jumps, so leases
/// run out mid-land).
pub const LEASE_MS: u64 = 5 * 60 * 1000;
/// The clock's start, ms.
pub const START_MS: i64 = 1_700_000_000_000;
/// Land calls per intent before a client gives up on it.
pub const MAX_TRIES: u32 = 40;

/// The engine under test.
pub type HarnessEngine = Engine<FaultyMeta<Arc<MemoryStore>>, FaultyObjects<Arc<MemoryStore>>>;

/// Installs things on each engine the run builds (the planted-bugs handle).
pub type EngineHook = Rc<dyn Fn(HarnessEngine) -> HarnessEngine>;

/// What one intent writes.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Intent {
    /// Regions of the shared file it adds its token to.
    pub regions: Vec<usize>,
    /// Whether it also writes the client's own file.
    pub private: bool,
    /// Whether the client sends the land again after it succeeds (a
    /// duplicated retry at the transport).
    pub duplicate: bool,
}

/// Something the adversary does between two scheduler steps.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum AdversaryEvent {
    /// Moves the clock forward, ms: past workspace leases, the land-attempt
    /// lease, retention and the GC grace.
    AdvanceClock(i64),
    /// Runs a GC sweep pass alongside the clients.
    Gc,
    /// Refuses object puts for want of space, for this many scheduler
    /// steps.
    FillDisk(u64),
}

/// A run, fully determined: the shrinker edits it.
#[derive(Clone, Debug, PartialEq)]
pub struct Plan {
    /// The seed it came from.
    pub seed: u64,
    /// Each client's intents.
    pub clients: Vec<Vec<Intent>>,
    /// Events, by the scheduler step before which they happen.
    pub events: Vec<(u64, AdversaryEvent)>,
    /// Fault rates.
    pub rates: FaultRates,
    /// Fault keys never to fire.
    pub disabled: BTreeSet<u64>,
    /// Wrapper bugs to plant.
    pub bugs: Vec<WrapperBug>,
    /// Steps before the run counts as stuck.
    pub max_steps: u64,
}

/// The shape of the plans a seed draws.
#[derive(Clone, Debug, PartialEq)]
pub struct Shape {
    /// Clients.
    pub clients: usize,
    /// Intents per client.
    pub intents: usize,
    /// Adversary events.
    pub events: usize,
    /// Fault rates.
    pub rates: FaultRates,
}

impl Default for Shape {
    fn default() -> Self {
        Self {
            clients: 4,
            intents: 4,
            events: 12,
            rates: FaultRates::hostile(),
        }
    }
}

impl Plan {
    /// The plan `seed` draws for `shape`.
    pub fn generate(seed: u64, shape: &Shape) -> Self {
        let mut rng = Rng::new(seed ^ 0x9a1f_7e55);
        let clients = (0..shape.clients)
            .map(|_| {
                (0..shape.intents)
                    .map(|_| {
                        let private = rng.chance(0.3);
                        let mut regions = BTreeSet::new();
                        if !private || rng.chance(0.5) {
                            regions.insert(rng.below(REGIONS as u64) as usize);
                            if rng.chance(0.3) {
                                regions.insert(rng.below(REGIONS as u64) as usize);
                            }
                        }
                        Intent {
                            regions: regions.into_iter().collect(),
                            private,
                            duplicate: rng.chance(0.2),
                        }
                    })
                    .collect()
            })
            .collect();
        let mut events: Vec<(u64, AdversaryEvent)> = (0..shape.events)
            .map(|_| {
                let step = rng.below(3_000);
                let event = match rng.below(6) {
                    0 | 1 => AdversaryEvent::Gc,
                    2 => AdversaryEvent::FillDisk(5 + rng.below(60)),
                    // Short jumps, and ones past the 10-minute land lease.
                    3 | 4 => AdversaryEvent::AdvanceClock(1_000 + rng.below(4 * 60_000) as i64),
                    _ => AdversaryEvent::AdvanceClock(9 * 60_000 + rng.below(16 * 60_000) as i64),
                };
                (step, event)
            })
            .collect();
        events.sort_by_key(|(step, _)| *step);
        Plan {
            seed,
            clients,
            events,
            rates: shape.rates.clone(),
            disabled: BTreeSet::new(),
            bugs: Vec::new(),
            max_steps: 200_000,
        }
    }
}

/// One recorded operation and its result.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Op {
    /// A client made a root draft for an intent: its identity (§3.3).
    Root {
        /// Client.
        client: usize,
        /// Intent.
        intent: usize,
        /// The draft.
        root: ChangesetId,
        /// What it was made on.
        parent: ChangesetId,
    },
    /// A `Land` call.
    Land {
        /// Client.
        client: usize,
        /// Intent.
        intent: usize,
        /// The head sent.
        head: ChangesetId,
        /// Whether the client sent this head before (a blind retry).
        retry: bool,
        /// The answer.
        result: Result<Landed, ApiError>,
    },
    /// A `Sync` call and the head it left.
    Sync {
        /// Client.
        client: usize,
        /// The answer: the workspace head after it.
        result: Result<Option<ChangesetId>, ApiError>,
    },
    /// A client gave up on an intent.
    GaveUp {
        /// Client.
        client: usize,
        /// Intent.
        intent: usize,
    },
    /// The adversary acted.
    Adversary(AdversaryEvent),
    /// A GC pass finished.
    Gc(Result<(), ApiError>),
    /// The process died and restarted.
    Restart,
    /// The run hit its step budget.
    Stuck,
}

/// Everything a run recorded.
pub type History = Vec<Op>;

/// A client's token for an intent.
pub fn token(client: usize, intent: usize) -> String {
    format!("c{client}.{intent}")
}

/// A client's own file.
pub fn private_path(client: usize) -> String {
    format!("own/c{client}.txt")
}

/// The shared file with each region holding `regions[r]`.
pub fn render_shared(regions: &[BTreeSet<String>]) -> Vec<u8> {
    let mut out = String::new();
    for (r, tokens) in regions.iter().enumerate() {
        let joined: Vec<&str> = tokens.iter().map(String::as_str).collect();
        out.push_str(&format!("region {r}: {}\n", joined.join(",")));
        for g in 0..REGION_GAP {
            out.push_str(&format!("gap {r}.{g}\n"));
        }
    }
    out.into_bytes()
}

/// The token sets of a shared file, or `None` when it is not in the layout.
pub fn parse_shared(data: &[u8]) -> Option<Vec<BTreeSet<String>>> {
    let text = std::str::from_utf8(data).ok()?;
    let mut out = Vec::new();
    for line in text.lines() {
        if line.starts_with("gap ") {
            continue;
        }
        let (head, tokens) = line.split_once(": ")?;
        if head != format!("region {}", out.len()) {
            return None;
        }
        out.push(
            tokens
                .split(',')
                .filter(|t| !t.is_empty())
                .map(str::to_owned)
                .collect(),
        );
    }
    (out.len() == REGIONS).then_some(out)
}

/// The token set of a private file.
pub fn parse_private(data: &[u8]) -> BTreeSet<String> {
    String::from_utf8_lossy(data)
        .lines()
        .map(str::to_owned)
        .collect()
}

fn render_private(tokens: &BTreeSet<String>) -> Vec<u8> {
    tokens
        .iter()
        .map(|t| format!("{t}\n"))
        .collect::<String>()
        .into_bytes()
}

/// A caller with the repository's admin scope.
pub fn caller(subject: &str) -> CallContext {
    CallContext {
        principal: Principal {
            subject: subject.to_owned(),
            kind: PrincipalKind::Agent,
            session: None,
            work_item: None,
            scopes: vec![format!("repo:{REPO}:admin")],
        },
        request_id: format!("{subject}-req"),
        deadline_ms: None,
    }
}

fn path(s: &str) -> RepoPath {
    RepoPath::new(s).expect("harness path")
}

fn retryable(e: &ApiError) -> bool {
    matches!(e, ApiError::Unavailable { .. } | ApiError::Contended { .. })
}

/// Calls `f` until it succeeds or fails with an error that is not
/// retryable, at most `tries` times.
async fn again<T>(
    tries: u32,
    mut f: impl AsyncFnMut() -> Result<T, ApiError>,
) -> Result<T, ApiError> {
    let mut last = None;
    for _ in 0..tries {
        match f().await {
            Err(e) if retryable(&e) => {
                last = Some(e);
                Yield(1).await;
            }
            other => return other,
        }
    }
    Err(last.expect("at least one try"))
}

/// The stores and clock that outlive a crash.
pub struct World {
    /// The store under both ports.
    pub store: Arc<MemoryStore>,
    /// The adversary.
    pub faults: Faults,
    /// The lease clock.
    pub clock: Arc<FakeClock>,
    hook: Option<EngineHook>,
    restarts: u32,
}

impl World {
    /// A fresh world for `plan`, with `hook` run on each engine built.
    pub fn new(plan: &Plan, hook: Option<EngineHook>) -> Self {
        let clock = Arc::new(FakeClock::at(START_MS));
        let faults = Faults::new(plan.seed, plan.rates.clone(), clock.clone());
        faults.disable(plan.disabled.iter().copied());
        for bug in &plan.bugs {
            faults.plant(*bug);
        }
        Self {
            store: Arc::new(MemoryStore::new()),
            faults,
            clock,
            hook,
            restarts: 0,
        }
    }

    /// Builds an engine over the world: a process start.
    pub fn engine(&mut self) -> Rc<HarnessEngine> {
        self.restarts += 1;
        let meta = FaultyMeta::new(self.store.clone(), self.faults.clone());
        let objects = FaultyObjects::new(self.store.clone(), self.faults.clone());
        let engine = Engine::new(
            Arc::new(meta),
            Arc::new(objects),
            self.clock.clone(),
            EngineConfig::default(),
        )
        .with_sleeper(Arc::new(YieldSleeper))
        .with_gc(GcSettings {
            config: GcConfig::default(),
            pace: Arc::new(YieldPace),
            leases: SweepLeases::with_owner(format!("harness-{}", self.restarts)),
        });
        Rc::new(match &self.hook {
            Some(hook) => hook(engine),
            None => engine,
        })
    }

    /// Creates the repository and line, with GC deletion on. No faults.
    pub fn setup(&mut self, engine: &HarnessEngine) -> ChangesetId {
        self.faults.set_enabled(false);
        let cx = caller("setup");
        run_local(async {
            engine
                .create_repo(&cx, REPO.to_owned())
                .await
                .expect("repo");
            let id = MetaStore::repo(&*self.store, REPO)
                .await
                .expect("read")
                .expect("repo id");
            let regions = vec![BTreeSet::from([BASE_TOKEN.to_owned()]); REGIONS];
            let data = render_shared(&regions);
            engine
                .put_objects(
                    &cx,
                    PutObjects {
                        repo: REPO.to_owned(),
                        blobs: vec![Frame::encode(ObjectKind::Blob, &data)],
                    },
                )
                .await
                .expect("upload");
            let root = engine
                .create_changeset(
                    &cx,
                    commit_req(
                        Vec::new(),
                        vec![Change::Put {
                            path: path(SHARED),
                            kind: EntryKind::File,
                            id: ObjectId::hash(ObjectKind::Blob, &data),
                        }],
                        None,
                        "root",
                    ),
                )
                .await
                .expect("root")
                .id;
            engine
                .update_ref(
                    &cx,
                    UpdateRef {
                        repo: REPO.to_owned(),
                        name: LINE.to_owned(),
                        expected: None,
                        new: root,
                        kind: RefKind::Line,
                    },
                )
                .await
                .expect("line");
            graft_gc::set_deletion(&*self.store, id, true, "harness")
                .await
                .expect("deletion on");
            self.faults.set_enabled(true);
            root
        })
    }
}

pub(crate) fn commit_req(
    parents: Vec<ChangesetId>,
    changes: Vec<Change>,
    workspace: Option<String>,
    message: &str,
) -> CreateChangeset {
    CreateChangeset {
        repo: REPO.to_owned(),
        parents,
        changes,
        author: Author {
            name: "Harness".to_owned(),
            email: "harness@example.com".to_owned(),
            kind: AuthorKind::Agent,
        },
        authored_at: Timestamp {
            ms: START_MS,
            offset_min: 0,
        },
        message: message.to_owned(),
        metadata: Metadata::new(),
        inline_blobs: Vec::new(),
        workspace,
    }
}

/// A land request for `head`.
pub fn land_req(head: ChangesetId) -> Land {
    Land {
        repo: REPO.to_owned(),
        head,
        line: LINE.to_owned(),
        squash: false,
        merge: MergeMode::Line,
        deadline_ms: None,
        merged_from: None,
    }
}

/// A client's state; it survives a crash, its task does not.
#[derive(Clone, Debug)]
struct Client {
    id: usize,
    intents: Vec<Intent>,
    next: usize,
    /// The head the client is landing, once its root exists.
    pending: Option<ChangesetId>,
    tries: u32,
    rng: Rng,
}

type Shared<T> = Rc<RefCell<T>>;

fn wref(client: usize) -> WorkspaceRef {
    WorkspaceRef {
        repo: REPO.to_owned(),
        name: format!("w{client}"),
    }
}

/// The client's workspace: the live one, or a new one at the line's tip.
async fn workspace(
    e: &HarnessEngine,
    cx: &CallContext,
    client: usize,
) -> Result<WorkspaceView, ApiError> {
    match e.get_workspace(cx, wref(client)).await {
        Ok(ws) if ws.lease_until_ms >= e.clock().now().ms => Ok(ws),
        Ok(_) | Err(ApiError::NotFound { .. } | ApiError::LeaseExpired { .. }) => {
            e.open_workspace(
                cx,
                OpenWorkspace {
                    repo: REPO.to_owned(),
                    name: Some(format!("w{client}")),
                    line: LINE.to_owned(),
                    at: None,
                    lease_ms: LEASE_MS,
                },
            )
            .await
        }
        Err(e) => Err(e),
    }
}

/// Syncs the client's workspace, recording the result.
async fn sync(
    e: &HarnessEngine,
    cx: &CallContext,
    history: &Shared<History>,
    client: usize,
) -> Result<Option<ChangesetId>, ApiError> {
    let result = e
        .sync(cx, wref(client), None)
        .await
        .map(|s| s.workspace.head);
    history.borrow_mut().push(Op::Sync {
        client,
        result: result.clone(),
    });
    result
}

/// Drops whatever the workspace stacks and moves it to the tip.
async fn reset(
    e: &HarnessEngine,
    cx: &CallContext,
    history: &Shared<History>,
    client: usize,
) -> Result<WorkspaceView, ApiError> {
    let ws = workspace(e, cx, client).await?;
    if ws.head.is_some() {
        e.set_head(
            cx,
            SetHead {
                workspace: wref(client),
                expected: ws.head,
                new: None,
            },
        )
        .await?;
    }
    sync(e, cx, history, client).await?;
    workspace(e, cx, client).await
}

/// Makes the root draft for the client's current intent.
async fn make_root(
    e: &HarnessEngine,
    cx: &CallContext,
    history: &Shared<History>,
    client: usize,
    intent_no: usize,
    intent: &Intent,
) -> Result<ChangesetId, ApiError> {
    let ws = reset(e, cx, history, client).await?;
    let _ = e.renew_lease(cx, wref(client), LEASE_MS).await;
    let top = ws.head.unwrap_or(ws.base);
    let mine = token(client, intent_no);
    let mut files: Vec<(String, Vec<u8>)> = Vec::new();
    if !intent.regions.is_empty() {
        let at = PathAt {
            repo: REPO.to_owned(),
            at: top,
            path: path(SHARED),
        };
        let data = e.read_file(cx, at).await?.data;
        let mut regions = parse_shared(&data).ok_or_else(|| ApiError::NotFound {
            what: "harness: shared file out of layout".to_owned(),
        })?;
        for r in &intent.regions {
            regions[*r].insert(mine.clone());
        }
        files.push((SHARED.to_owned(), render_shared(&regions)));
    }
    if intent.private {
        let own = private_path(client);
        let at = PathAt {
            repo: REPO.to_owned(),
            at: top,
            path: path(&own),
        };
        let mut tokens = match e.read_file(cx, at).await {
            Ok(f) => parse_private(&f.data),
            Err(ApiError::NotFound { .. }) => BTreeSet::new(),
            Err(err) => return Err(err),
        };
        tokens.insert(mine.clone());
        files.push((own, render_private(&tokens)));
    }
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
    let created = e
        .create_changeset(
            cx,
            commit_req(vec![top], changes, Some(format!("w{client}")), &mine),
        )
        .await?;
    history.borrow_mut().push(Op::Root {
        client,
        intent: intent_no,
        root: created.id,
        parent: top,
    });
    Ok(created.id)
}

/// A client's loop: each intent in turn, until all are landed or given up.
async fn run_client(e: Rc<HarnessEngine>, me: Shared<Client>, history: Shared<History>) {
    let client = me.borrow().id;
    let cx = caller(&format!("c{client}"));
    loop {
        let (intent_no, intent, pending) = {
            let c = me.borrow();
            match c.intents.get(c.next) {
                Some(i) => (c.next, i.clone(), c.pending),
                None => return,
            }
        };
        Yield(1).await;
        let give_up = {
            let mut c = me.borrow_mut();
            c.tries += 1;
            c.tries > MAX_TRIES
        };
        if give_up {
            history.borrow_mut().push(Op::GaveUp {
                client,
                intent: intent_no,
            });
            let mut c = me.borrow_mut();
            (c.next, c.pending, c.tries) = (c.next + 1, None, 0);
            continue;
        }
        let (head, retry) = match pending {
            Some(head) => (head, true),
            None => match make_root(&e, &cx, &history, client, intent_no, &intent).await {
                Ok(root) => {
                    me.borrow_mut().pending = Some(root);
                    (root, false)
                }
                Err(_) => continue,
            },
        };
        let result = e.land(&cx, land_req(head)).await;
        history.borrow_mut().push(Op::Land {
            client,
            intent: intent_no,
            head,
            retry,
            result: result.clone(),
        });
        match result {
            Ok(_) => {
                if intent.duplicate {
                    let again = e.land(&cx, land_req(head)).await;
                    history.borrow_mut().push(Op::Land {
                        client,
                        intent: intent_no,
                        head,
                        retry: true,
                        result: again,
                    });
                }
                {
                    let mut c = me.borrow_mut();
                    (c.next, c.pending, c.tries) = (c.next + 1, None, 0);
                }
                let _ = again(4, async || sync(&e, &cx, &history, client).await).await;
            }
            Err(ApiError::Conflict { .. } | ApiError::StaleBase(_)) => {
                // Resolve: a new root for the same intent, on the new tip.
                // A stale base is the same dead end as a content conflict:
                // the client's base is behind, so retrying the same head
                // cannot land.
                me.borrow_mut().pending = None;
            }
            Err(_) => {
                // Unsure whether it landed. Retry blind, or sync first.
                let sync_first = me.borrow_mut().rng.chance(0.4);
                if sync_first && let Ok(Some(new_head)) = sync(&e, &cx, &history, client).await {
                    me.borrow_mut().pending = Some(new_head);
                }
            }
        }
    }
}

/// What a run leaves for the checker.
pub struct Outcome {
    /// The world, faults off.
    pub world: World,
    /// A fresh engine over it.
    pub engine: Rc<HarnessEngine>,
    /// The line's first changeset.
    pub root: ChangesetId,
    /// The history.
    pub history: History,
    /// How many scheduler steps ran.
    pub steps: u64,
}

/// Runs `plan`, with `hook` on every engine.
pub fn run(plan: &Plan, hook: Option<EngineHook>) -> Outcome {
    let mut world = World::new(plan, hook);
    let mut engine = world.engine();
    let root = world.setup(&engine);
    let history: Shared<History> = Rc::default();
    let mut seed = Rng::new(plan.seed ^ 0x5c4e_d01e);
    let clients: Vec<Shared<Client>> = plan
        .clients
        .iter()
        .enumerate()
        .map(|(id, intents)| {
            Rc::new(RefCell::new(Client {
                id,
                intents: intents.clone(),
                next: 0,
                pending: None,
                tries: 0,
                rng: seed.fork(),
            }))
        })
        .collect();
    let mut sched = Scheduler::new(seed.fork());
    let spawn_all = |sched: &mut Scheduler, engine: &Rc<HarnessEngine>| {
        for c in &clients {
            sched.spawn(Box::pin(run_client(
                engine.clone(),
                c.clone(),
                history.clone(),
            )));
        }
    };
    spawn_all(&mut sched, &engine);
    let mut events = plan.events.iter().peekable();
    let mut steps = 0u64;
    let mut full_until = 0u64;
    loop {
        if full_until != 0 && steps >= full_until {
            world.faults.set_full(false);
            full_until = 0;
        }
        while let Some((_, event)) = events.next_if(|(at, _)| *at <= steps) {
            history.borrow_mut().push(Op::Adversary(event.clone()));
            match event {
                AdversaryEvent::AdvanceClock(ms) => world.clock.advance(*ms),
                AdversaryEvent::FillDisk(n) => {
                    world.faults.set_full(true);
                    full_until = steps + n;
                }
                AdversaryEvent::Gc => {
                    let (e, h) = (engine.clone(), history.clone());
                    sched.spawn(Box::pin(async move {
                        let r = e.gc_sweep(&caller("gc"), REPO, false).await.map(|_| ());
                        h.borrow_mut().push(Op::Gc(r));
                    }));
                }
            }
        }
        if !sched.step() {
            break;
        }
        steps += 1;
        if world.faults.crashed() {
            history.borrow_mut().push(Op::Restart);
            sched.kill_all();
            world.faults.restart();
            engine = world.engine();
            spawn_all(&mut sched, &engine);
        }
        if steps >= plan.max_steps {
            history.borrow_mut().push(Op::Stuck);
            break;
        }
    }
    world.faults.set_enabled(false);
    world.faults.set_full(false);
    let engine = world.engine();
    let history = Rc::try_unwrap(history)
        .map(RefCell::into_inner)
        .unwrap_or_else(|h| h.borrow().clone());
    Outcome {
        world,
        engine,
        root,
        history,
        steps,
    }
}

impl World {
    /// The clock now, ms.
    pub fn clock_now(&self) -> i64 {
        graft_model::Clock::now(&*self.clock).ms
    }
}
