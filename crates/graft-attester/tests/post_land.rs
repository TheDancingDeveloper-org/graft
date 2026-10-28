//! Post-land mode (WI-2598): the real engine over the in-memory store, its
//! line configured with post-land checks, served by graft-testkit's
//! `TestServer`, with a fake check runner whose `ci/test` fails on any
//! snapshot holding the file `broken`. Every test asserts what ends up in
//! the journal, the line and the server's land-checks rows.

use std::collections::BTreeMap;
use std::io;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use graft_attester::post_land::{
    BlockReason, PostLand, PostLandReport, REVERT_CHECK, REVERTS, Run, Runner, flake_attempts,
    flake_url,
};
use graft_attester::{Attester, Check, Config, Error, Outcome, call_context};
use graft_client::{Endpoint, Remote};
use graft_engine::policy::LineChecks;
use graft_engine::{Engine, EngineConfig, LineConfig};
use graft_model::{
    Author, AuthorKind, Change, ChangesetId, EntryKind, Metadata, ObjectId, ObjectKind, RepoPath,
    Timestamp,
};
use graft_proto::api::{AdminApi, ObjectApi, RepoApi};
use graft_proto::error::ApiResult;
use graft_proto::types::*;
use graft_proto::{ApiError, CallContext, Frame, Principal, PrincipalKind};
use graft_store::memory::MemoryStore;
use graft_testkit::clock::FakeClock;
use graft_testkit::server::TestServer;

const REPO: &str = "graft";
const LINE: &str = "integration";
const BUILD: &str = "ci/build";
const TEST: &str = "ci/test";

type Eng = Engine<MemoryStore, MemoryStore>;

// ---- the server -----------------------------------------------------------

/// Faults injected in front of the engine.
#[derive(Default)]
struct Faults {
    /// `Some(n)`: `n` more attestations succeed, then one fails
    /// `Unavailable` (and the fault is spent).
    attest_after: Mutex<Option<u32>>,
    /// Attestations failed by the fault.
    attest_fired: Mutex<u32>,
    /// Lands to carry out and then answer `Unavailable`, as a lost response.
    land_lost: Mutex<u32>,
    /// Lands whose response was lost.
    land_fired: Mutex<u32>,
}

/// The engine, answering every call as the checker's principal.
struct AsChecker {
    engine: Arc<Eng>,
    faults: Arc<Faults>,
}

fn checker_principal() -> Principal {
    Principal {
        subject: "graft-attester-post-land".to_owned(),
        kind: PrincipalKind::Service,
        session: None,
        work_item: None,
        scopes: vec![
            format!("repo:{REPO}:write"),
            format!("repo:{REPO}:attest"),
            format!("repo:{REPO}:land:{LINE}"),
        ],
    }
}

impl AsChecker {
    fn cx(&self, cx: &CallContext) -> CallContext {
        CallContext {
            principal: checker_principal(),
            request_id: cx.request_id.clone(),
            deadline_ms: cx.deadline_ms,
        }
    }
}

macro_rules! delegate {
    ($($method:ident($req:ty) -> $out:ty;)*) => {
        $(
            async fn $method(&self, cx: &CallContext, req: $req) -> ApiResult<$out> {
                self.engine.$method(&self.cx(cx), req).await
            }
        )*
    };
}

impl ObjectApi for AsChecker {
    delegate! {
    get_objects(GetObjects) -> Objects;
    has_objects(HasObjects) -> Vec<ObjectId>;
    put_objects(PutObjects) -> Vec<ObjectId>;
    }
}

impl RepoApi for AsChecker {
    delegate! {
    create_changeset(CreateChangeset) -> Created;
    get_changesets(GetChangesets) -> Changesets;
    resolve_ref(ResolveRef) -> RefView;
    list_refs(ListRefs) -> Vec<RefView>;
    update_ref(UpdateRef) -> RefView;
    stat(PathAt) -> EntryView;
    list_tree(ListTree) -> Entries;
    read_file(PathAt) -> FileContent;
    log(Log) -> History;
    is_ancestor(Pair) -> bool;
    merge_base(Pair) -> Option<ChangesetId>;
    diff(Diff) -> DiffResult;
    blame(Blame) -> BlameResult;
    rebase(Rebase) -> ChangesetId;
    revert(Revert) -> ChangesetId;
    watch(Watch) -> JournalPage;
    }

    async fn land(&self, cx: &CallContext, req: Land) -> ApiResult<Landed> {
        let landed = self.engine.land(&self.cx(cx), req).await?;
        let mut lost = self.faults.land_lost.lock().expect("faults");
        if *lost > 0 {
            *lost -= 1;
            *self.faults.land_fired.lock().expect("faults") += 1;
            return Err(ApiError::Unavailable { retry_after_ms: 1 });
        }
        Ok(landed)
    }

    async fn attest(&self, cx: &CallContext, req: Attest) -> ApiResult<()> {
        {
            let mut after = self.faults.attest_after.lock().expect("faults");
            if let Some(n) = after.as_mut() {
                if *n == 0 {
                    *after = None;
                    *self.faults.attest_fired.lock().expect("faults") += 1;
                    return Err(ApiError::Unavailable { retry_after_ms: 1 });
                }
                *n -= 1;
            }
        }
        self.engine.attest(&self.cx(cx), req).await
    }
}

fn admin() -> CallContext {
    CallContext {
        principal: Principal {
            subject: "tester".to_owned(),
            kind: PrincipalKind::Human,
            session: None,
            work_item: None,
            scopes: vec![format!("repo:{REPO}:admin")],
        },
        request_id: "post-land".to_owned(),
        deadline_ms: None,
    }
}

fn path(s: &str) -> RepoPath {
    RepoPath::new(s).expect("path")
}

/// The engine, its line `integration` checked post-land with `ci/build`
/// and `ci/test` and `retries`, holding one root changeset.
async fn repo(retries: u32) -> Arc<Eng> {
    let mut line = LineConfig::direct(LINE);
    line.checks = LineChecks::PostLand {
        required: vec![BUILD.to_owned(), TEST.to_owned()],
        retries,
    };
    let config = EngineConfig {
        lines: vec![line],
        ..EngineConfig::default()
    };
    let store = Arc::new(MemoryStore::new());
    let clock = Arc::new(FakeClock::at(1_700_000_000_000));
    let engine = Arc::new(Engine::new(store.clone(), store, clock, config));
    engine
        .create_repo(&admin(), REPO.to_owned())
        .await
        .expect("create repo");
    let root = commit(&engine, None, &[("README", "graft\n")]).await;
    engine
        .update_ref(
            &admin(),
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
    engine
}

/// A changeset on `parent` putting `files`, held by nothing.
async fn commit(engine: &Eng, parent: Option<ChangesetId>, files: &[(&str, &str)]) -> ChangesetId {
    let mut changes: Vec<Change> = files
        .iter()
        .map(|(p, c)| Change::Put {
            path: path(p),
            kind: EntryKind::File,
            id: ObjectId::hash(ObjectKind::Blob, c.as_bytes()),
        })
        .collect();
    changes.sort_by(|a, b| a.path().cmp(b.path()));
    engine
        .create_changeset(
            &admin(),
            CreateChangeset {
                repo: REPO.to_owned(),
                parents: parent.into_iter().collect(),
                changes,
                author: Author {
                    name: "Tester".to_owned(),
                    email: "tester@example.com".to_owned(),
                    kind: AuthorKind::Agent,
                },
                authored_at: Timestamp {
                    ms: 1_700_000_000_000,
                    offset_min: 0,
                },
                message: format!("put {}", files[0].0),
                metadata: Metadata::new(),
                inline_blobs: files
                    .iter()
                    .map(|(_, c)| Frame::encode(ObjectKind::Blob, c.as_bytes()))
                    .collect(),
                workspace: None,
            },
        )
        .await
        .expect("commit")
        .id
}

async fn tip(engine: &Eng) -> ChangesetId {
    engine
        .resolve_ref(
            &admin(),
            ResolveRef {
                repo: REPO.to_owned(),
                name: LINE.to_owned(),
            },
        )
        .await
        .expect("tip")
        .target
}

/// Lands one changeset putting `files` on the line, as an author would.
async fn land(engine: &Eng, files: &[(&str, &str)]) -> ChangesetId {
    let head = commit(engine, Some(tip(engine).await), files).await;
    match engine
        .land(
            &admin(),
            Land {
                repo: REPO.to_owned(),
                head,
                line: LINE.to_owned(),
                squash: false,
                merge: MergeMode::Line,
                deadline_ms: None,
                merged_from: None,
            },
        )
        .await
        .expect("land")
    {
        Landed::Landed { landed, .. } => landed,
        other => panic!("not landed: {other:?}"),
    }
}

/// `n` lands, the `bad`-th (from 1) planting `broken`. Returns them in
/// order.
async fn lands(engine: &Eng, n: usize, bad: usize) -> Vec<ChangesetId> {
    let mut out = Vec::new();
    for i in 1..=n {
        let name = format!("f{i}.txt");
        let mut files = vec![(name.as_str(), "ok\n")];
        if i == bad {
            files.push(("broken", "yes\n"));
        }
        out.push(land(engine, &files).await);
    }
    out
}

/// Journal entries of `kind`, parsed.
async fn entries(engine: &Eng, kind: &str) -> Vec<(u64, serde_json::Value)> {
    let mut out = Vec::new();
    let mut from = 1;
    loop {
        let page = engine
            .watch(
                &admin(),
                Watch {
                    repo: REPO.to_owned(),
                    from_lsn: from,
                    kinds: vec![kind.to_owned()],
                    limit: 0,
                    wait_ms: 0,
                },
            )
            .await
            .expect("watch");
        for e in &page.entries {
            out.push((e.lsn, serde_json::from_str(&e.body).expect("json")));
        }
        if page.entries.is_empty() || page.next_lsn <= from {
            return out;
        }
        from = page.next_lsn;
    }
}

/// Check attestations per land: `(lsn, check, passed)`, in journal order.
async fn attested(engine: &Eng) -> BTreeMap<String, Vec<(u64, String, bool)>> {
    let mut out: BTreeMap<String, Vec<(u64, String, bool)>> = BTreeMap::new();
    for (lsn, v) in entries(engine, "Attested").await {
        let passed = match v.get("status").and_then(|s| s.as_str()) {
            Some(s) => s == "pass",
            None => v["passed"].as_bool().expect("passed"),
        };
        out.entry(v["changeset"].as_str().expect("changeset").to_owned())
            .or_default()
            .push((lsn, v["check"].as_str().expect("check").to_owned(), passed));
    }
    out
}

/// The server's land-checks state per landed changeset.
async fn land_states(engine: &Eng) -> BTreeMap<ChangesetId, LandCheckState> {
    engine
        .get_land_checks(
            &admin(),
            GetLandChecks {
                repo: REPO.to_owned(),
                line: LINE.to_owned(),
                from_pos: 0,
                page: Page {
                    limit: 1000,
                    token: None,
                },
            },
        )
        .await
        .expect("land checks")
        .rows
        .into_iter()
        .map(|r| (r.changeset, r.state))
        .collect()
}

/// Whether `file` is in the line's tip.
async fn at_tip(engine: &Eng, file: &str) -> bool {
    engine
        .stat(
            &admin(),
            PathAt {
                repo: REPO.to_owned(),
                at: tip(engine).await,
                path: path(file),
            },
        )
        .await
        .is_ok()
}

// ---- the fake runner ------------------------------------------------------

/// `ci/test` fails on a snapshot holding `broken`; `ci/build` passes.
/// `flaky[check]` runs fail first whatever the snapshot. `crash_at` fails
/// the run with that index (from 0) with an error, as a crash would.
#[derive(Default)]
struct Fake {
    runs: Vec<(ChangesetId, String, u32)>,
    flaky: BTreeMap<String, u32>,
    crash_at: Option<usize>,
    crashed: u32,
}

impl Fake {
    fn runs_of(&self, check: &str) -> usize {
        self.runs.iter().filter(|(_, c, _)| c == check).count()
    }
}

impl<A: RepoApi> Runner<A> for Fake {
    async fn run(
        &mut self,
        api: &A,
        at: ChangesetId,
        check: &Check,
        attempt: u32,
    ) -> Result<Run, Error> {
        if self.crash_at == Some(self.runs.len()) {
            self.crash_at = None;
            self.crashed += 1;
            return Err(Error::Io(io::Error::other("the checker died")));
        }
        self.runs.push((at, check.name.clone(), attempt));
        let failed = match self.flaky.get_mut(&check.name) {
            Some(n) if *n > 0 => {
                *n -= 1;
                true
            }
            _ if check.name == TEST => {
                match api
                    .stat(
                        &call_context(),
                        PathAt {
                            repo: REPO.to_owned(),
                            at,
                            path: path("broken"),
                        },
                    )
                    .await
                {
                    Ok(_) => true,
                    Err(ApiError::NotFound { .. }) => false,
                    Err(e) => return Err(Error::Api(e)),
                }
            }
            _ => false,
        };
        Ok(Run {
            outcome: if failed {
                Outcome::Failed(Some(1))
            } else {
                Outcome::Passed
            },
            url: format!("https://ci.example/{at}/{}/{attempt}", check.name),
        })
    }

    async fn release(&mut self) {}
}

// ---- the checker ----------------------------------------------------------

struct Rig {
    engine: Arc<Eng>,
    faults: Arc<Faults>,
    server: TestServer,
    dir: tempfile::TempDir,
    retries: u32,
}

impl Rig {
    async fn new(retries: u32) -> Rig {
        let engine = repo(retries).await;
        let faults = Arc::new(Faults::default());
        let server = TestServer::tcp(Arc::new(AsChecker {
            engine: engine.clone(),
            faults: faults.clone(),
        }))
        .await;
        Rig {
            engine,
            faults,
            server,
            dir: tempfile::tempdir().expect("tempdir"),
            retries,
        }
    }

    async fn remote(&self) -> Remote {
        Remote::connect(Endpoint::Server(self.server.url().to_owned()))
            .await
            .expect("connect")
            .with_token("not-a-real-token")
    }

    fn config(&self) -> Config {
        let mut cfg = Config::new(REPO, LINE, self.dir.path());
        cfg.checks = [BUILD, TEST]
            .iter()
            .map(|c| Check::parse(&format!("{c}=true")).expect("check"))
            .collect();
        cfg.state_file = Some(self.dir.path().join("state").join("post-land-lsn"));
        cfg.attest_tries = 1;
        cfg.retry_delay = Duration::from_millis(1);
        cfg.wait_ms = 0;
        cfg
    }

    /// A checker reading from the saved position, as after a (re)start.
    async fn checker(&self, runner: Fake) -> PostLand<Remote, Fake> {
        PostLand::new(self.remote().await, runner, self.config(), self.retries).expect("checker")
    }
}

/// Steps until idle (or blocked), collecting the reports. Bounded, so a
/// checker that never settles fails the test instead of hanging it.
async fn drain(c: &mut PostLand<Remote, Fake>) -> Vec<PostLandReport> {
    let mut out = Vec::new();
    for _ in 0..50 {
        let r = c.step(0).await.expect("step");
        let stop = matches!(r, PostLandReport::Idle | PostLandReport::Blocked(_));
        out.push(r);
        if stop {
            return out;
        }
    }
    panic!("not settled after 50 steps: {out:?}");
}

fn ceil_log2(n: usize) -> usize {
    (usize::BITS - (n - 1).leading_zeros()) as usize
}

/// Accept 1 and 2: N lands with one bad land at k. The checker skips ahead
/// to the newest, bisects to k in at most ceil(log2 N)+1 runs of the
/// failing check, attests every land, lands exactly one revert of k, and
/// settles the lands after k on top of the revert, where they stay.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn bisection_finds_the_first_bad_land_and_one_revert_settles_the_rest() {
    const N: usize = 13;
    const K: usize = 9;
    let rig = Rig::new(0).await;
    let landed = lands(&rig.engine, N, K).await;
    let bad = landed[K - 1];
    let mut c = rig.checker(Fake::default()).await;

    // First step: skip ahead, bisect, attest.
    let first = c.step(0).await.expect("step");
    match &first {
        PostLandReport::Bad { land, check, good } => {
            assert_eq!(*land, bad);
            assert_eq!(check, TEST);
            assert_eq!(good, &landed[..K - 1]);
        }
        other => panic!("expected the bad land, got {other:?}"),
    }
    let test_runs = c.runner().runs_of(TEST);
    assert!(
        test_runs <= ceil_log2(N) + 1,
        "{test_runs} runs of {TEST} for {N} lands"
    );
    // The newest land was checked first.
    assert_eq!(c.runner().runs[0].0, landed[N - 1]);

    let rest = drain(&mut c).await;
    let revert = match &rest[0] {
        PostLandReport::Reverted { land, revert } => {
            assert_eq!(*land, bad);
            *revert
        }
        other => panic!("expected the revert, got {other:?}"),
    };
    match &rest[1] {
        PostLandReport::Good { at, lands } => {
            assert_eq!(*at, revert);
            let mut want = landed[K..].to_vec();
            want.push(revert);
            assert_eq!(lands, &want, "the lands after k, then the revert");
        }
        other => panic!("expected the lands after k settled, got {other:?}"),
    }
    assert_eq!(rest[2..], [PostLandReport::Idle]);

    // Exactly one revert, of k, landed through the ordinary path.
    let reverted = entries(&rig.engine, "LandReverted").await;
    assert_eq!(reverted.len(), 1, "{reverted:?}");
    assert_eq!(reverted[0].1["land"], bad.to_string());
    assert_eq!(tip(&rig.engine).await, revert);
    let revert_cs = &rig
        .engine
        .get_changesets(
            &admin(),
            GetChangesets {
                repo: REPO.to_owned(),
                ids: vec![revert],
            },
        )
        .await
        .expect("revert")
        .found[0]
        .changeset;
    assert_eq!(revert_cs.metadata[REVERTS], bad.to_string());
    assert_eq!(
        reverted[0].1["check"], TEST,
        "the failed check is journalled"
    );
    // WI-2608: the revert carries the link of the last failed run of k.
    let failed_url = entries(&rig.engine, "Attested")
        .await
        .into_iter()
        .rev()
        .find(|(_, b)| b["changeset"] == bad.to_string() && b["passed"] == false)
        .map(|(_, b)| b["url"].as_str().expect("url").to_owned())
        .expect("a failed attestation of k");
    assert!(failed_url.starts_with(&format!("https://ci.example/{bad}/{TEST}/")));
    assert_eq!(revert_cs.metadata[REVERT_CHECK], failed_url);

    // Every land is attested: k failed, every other land (and the revert)
    // passed every check, and never both.
    let got = attested(&rig.engine).await;
    let lsn_reverted = reverted[0].0;
    for (i, id) in landed.iter().chain([&revert]).enumerate() {
        let reports = got.get(&id.to_string()).expect("attested");
        if *id == bad {
            assert!(
                reports.iter().any(|(_, c, p)| c == TEST && !p),
                "{reports:?}"
            );
            assert!(!reports.iter().any(|(_, c, p)| c == TEST && *p));
            continue;
        }
        for check in [BUILD, TEST] {
            assert!(
                reports.iter().all(|(_, c, p)| c != check || *p),
                "land {i}: {reports:?}"
            );
            assert!(reports.iter().any(|(_, c, _)| c == check), "land {i}");
        }
        // Accept 2: the lands after k were settled on top of the revert.
        if i >= K {
            assert!(
                reports.iter().all(|(lsn, _, _)| *lsn > lsn_reverted),
                "land {i} attested before the revert: {reports:?}"
            );
        }
    }

    // The lands after k stay on the line; k's change is gone.
    for i in K + 1..=N {
        assert!(at_tip(&rig.engine, &format!("f{i}.txt")).await, "f{i}");
    }
    assert!(!at_tip(&rig.engine, &format!("f{K}.txt")).await);
    assert!(!at_tip(&rig.engine, "broken").await);

    // The server agrees: k reverted, everything else good.
    let states = land_states(&rig.engine).await;
    for id in landed.iter().chain([&revert]) {
        let want = if *id == bad {
            LandCheckState::Reverted
        } else {
            LandCheckState::Good
        };
        assert_eq!(states.get(id), Some(&want), "{id}");
    }
}

/// Accept 3: a check that fails and then passes is retried, the land is
/// good, only the pass is attested, and the flake is counted.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_flaky_check_is_retried_and_counted() {
    let rig = Rig::new(1).await;
    let landed = lands(&rig.engine, 2, 0).await;
    let mut fake = Fake::default();
    fake.flaky.insert(TEST.to_owned(), 1);
    let mut c = rig.checker(fake).await;
    let reports = drain(&mut c).await;
    assert_eq!(
        reports[0],
        PostLandReport::Good {
            at: landed[1],
            lands: landed.clone()
        }
    );
    assert_eq!(c.flakes().get(TEST), Some(&1));
    assert_eq!(c.flakes().get(BUILD), None);
    // Ran twice: the failure and the retry that passed.
    assert_eq!(c.runner().runs_of(TEST), 2);
    let got = attested(&rig.engine).await;
    for id in &landed {
        let reports = &got[&id.to_string()];
        assert!(reports.iter().all(|(_, _, p)| *p), "{reports:?}");
    }
    let states = land_states(&rig.engine).await;
    assert!(
        landed
            .iter()
            .all(|id| states.get(id) == Some(&LandCheckState::Good)),
        "{states:?}"
    );
    assert!(entries(&rig.engine, "LandReverted").await.is_empty());
}

/// Pass urls per land and check, from the server's journal.
async fn pass_urls(engine: &Eng) -> BTreeMap<(String, String), Vec<String>> {
    let mut out: BTreeMap<(String, String), Vec<String>> = BTreeMap::new();
    for (_, v) in entries(engine, "Attested").await {
        let passed = match v.get("status").and_then(|s| s.as_str()) {
            Some(s) => s == "pass",
            None => v["passed"].as_bool().expect("passed"),
        };
        if passed {
            out.entry((
                v["changeset"].as_str().expect("changeset").to_owned(),
                v["check"].as_str().expect("check").to_owned(),
            ))
            .or_default()
            .push(v["url"].as_str().expect("url").to_owned());
        }
    }
    out
}

/// WI-2616: a flake leaves a record on the server. Each land's pass of
/// `ci/test` says it took 2 runs; `ci/build`, which passed first time,
/// carries no marker. The record is read after the checker is gone and a
/// restarted checker leaves it as it is (accept 2); the land reads good
/// with no failure attested (accept 3).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_flake_is_recorded_on_the_server_and_survives_a_restart() {
    let rig = Rig::new(2).await;
    let landed = lands(&rig.engine, 2, 0).await;
    let mut fake = Fake::default();
    fake.flaky.insert(TEST.to_owned(), 1);
    let mut c = rig.checker(fake).await;
    drain(&mut c).await;
    drop(c);

    // Restarted, with nothing new to check.
    let mut c = rig.checker(Fake::default()).await;
    drain(&mut c).await;
    assert_eq!(c.runner().runs.len(), 0, "nothing re-run");
    assert!(c.flakes().is_empty(), "the counter is gone");
    drop(c);

    let urls = pass_urls(&rig.engine).await;
    for id in &landed {
        let test = &urls[&(id.to_string(), TEST.to_owned())];
        assert_eq!(test.len(), 1, "{test:?}");
        assert_eq!(flake_attempts(&test[0]), Some(2), "{test:?}");
        let build = &urls[&(id.to_string(), BUILD.to_owned())];
        assert_eq!(build.len(), 1, "{build:?}");
        assert_eq!(flake_attempts(&build[0]), None, "{build:?}");
    }
    let got = attested(&rig.engine).await;
    for id in &landed {
        assert!(got[&id.to_string()].iter().all(|(_, _, p)| *p));
    }
    let states = land_states(&rig.engine).await;
    assert!(
        landed
            .iter()
            .all(|id| states.get(id) == Some(&LandCheckState::Good)),
        "{states:?}"
    );
}

/// The marker joins an existing fragment, reads back, and still fits the
/// server's url limit when the run url is already at it.
#[test]
fn the_flake_marker_reads_back_and_fits() {
    let u = flake_url("https://ci.example/run/7", 3);
    assert_eq!(u, "https://ci.example/run/7#graft-attempts=3");
    assert_eq!(flake_attempts(&u), Some(3));
    let u = flake_url("https://ci.example/run/7#log", 2);
    assert_eq!(u, "https://ci.example/run/7#log;graft-attempts=2");
    assert_eq!(flake_attempts(&u), Some(2));
    assert_eq!(flake_attempts("https://ci.example/run/7#log"), None);
    let long = format!("https://ci.example/{}", "é".repeat(MAX_ATTEST_URL));
    let u = flake_url(&long, 4);
    assert!(u.len() <= MAX_ATTEST_URL, "{}", u.len());
    assert_eq!(flake_attempts(&u), Some(4));
}

/// Accept 3, the other side: a check that fails every retry makes the land
/// bad, one failure attested per run.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_check_failing_every_retry_is_bad() {
    let rig = Rig::new(1).await;
    let landed = lands(&rig.engine, 1, 1).await;
    let mut c = rig.checker(Fake::default()).await;
    let reports = drain(&mut c).await;
    assert!(
        matches!(&reports[0], PostLandReport::Bad { land, .. } if *land == landed[0]),
        "{reports:?}"
    );
    assert!(c.flakes().is_empty());
    let got = attested(&rig.engine).await;
    let fails = got[&landed[0].to_string()]
        .iter()
        .filter(|(_, c, p)| c == TEST && !p)
        .count();
    assert_eq!(fails, 2, "retries + 1 failures");
    assert_eq!(entries(&rig.engine, "LandReverted").await.len(), 1);
}

/// Posts a `check` report on `land` directly, as another attester would.
async fn attest_directly(engine: &Eng, land: ChangesetId, check: &str, passed: bool) {
    let cx = CallContext {
        principal: checker_principal(),
        request_id: "another-attester".to_owned(),
        deadline_ms: None,
    };
    engine
        .attest_v2(
            &cx,
            AttestV2 {
                repo: REPO.to_owned(),
                changeset: land,
                kind: AttestKind::Check,
                check: check.to_owned(),
                status: if passed {
                    AttestStatus::Pass
                } else {
                    AttestStatus::Fail
                },
                by: None,
                url: format!("https://other.example/{land}/{check}/{passed}"),
            },
        )
        .await
        .expect("attest");
}

/// WI-2611 review: a check that failed and then passed within `retries`
/// is a flake, not an interrupted bad verdict. The land holds a fail and
/// then a pass of `ci/build` (`ci/test` not yet reported), the checker
/// dies before posting anything, and the restart settles the land good:
/// no failure added, no revert.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_restart_leaves_a_flake_that_already_passed_good() {
    let rig = Rig::new(1).await;
    let land = lands(&rig.engine, 1, 0).await[0];
    attest_directly(&rig.engine, land, BUILD, false).await;
    attest_directly(&rig.engine, land, BUILD, true).await;
    assert_eq!(
        land_states(&rig.engine).await.get(&land),
        Some(&LandCheckState::Pending),
        "{TEST} not reported yet"
    );

    // Dies on its first run, before posting anything.
    let mut c = rig
        .checker(Fake {
            crash_at: Some(0),
            ..Fake::default()
        })
        .await;
    let first = c.step(0).await;
    assert!(
        first.is_err(),
        "the checks run and the run crashes: {first:?}"
    );
    assert_eq!(c.runner().crashed, 1, "the crash fired");
    drop(c);

    // Restarted.
    let mut c = rig.checker(Fake::default()).await;
    let reports = drain(&mut c).await;
    assert!(
        reports
            .iter()
            .all(|r| matches!(r, PostLandReport::Good { .. } | PostLandReport::Idle)),
        "{reports:?}"
    );
    let build: Vec<bool> = attested(&rig.engine).await[&land.to_string()]
        .iter()
        .filter(|(_, c, _)| c == BUILD)
        .map(|(_, _, p)| *p)
        .collect();
    assert_eq!(build, [false, true], "no failure added");
    assert!(entries(&rig.engine, "LandReverted").await.is_empty());
    assert_eq!(
        land_states(&rig.engine).await.get(&land),
        Some(&LandCheckState::Good)
    );
}

/// WI-2611: the checker finds a land bad (a flaky check failed every
/// retry) and dies after posting only one of its failures. Restarted, the
/// check would pass, but the posted failure is final: the restart posts the
/// missing failure without running the check, and the land is reverted
/// once. No land gets both a pass and a fail of one check.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_restart_finishes_a_bad_verdict_instead_of_passing_a_flaky_check() {
    let rig = Rig::new(1).await;
    let landed = lands(&rig.engine, 1, 0).await;
    let land = landed[0];

    // `ci/test` fails both runs; the pass of `ci/build` and one failure
    // post, the second failure does not.
    *rig.faults.attest_after.lock().expect("faults") = Some(2);
    let mut fake = Fake::default();
    fake.flaky.insert(TEST.to_owned(), 2);
    let mut c = rig.checker(fake).await;
    assert!(c.step(0).await.is_err(), "attest failed");
    assert_eq!(*rig.faults.attest_fired.lock().expect("faults"), 1);
    drop(c);
    let got = attested(&rig.engine).await;
    let posted: Vec<(&str, bool)> = got[&land.to_string()]
        .iter()
        .map(|(_, c, p)| (c.as_str(), *p))
        .collect();
    assert_eq!(posted, [(BUILD, true), (TEST, false)]);
    assert_eq!(
        land_states(&rig.engine).await.get(&land),
        Some(&LandCheckState::Pending),
        "one failure is within retries"
    );

    // Restarted with the check no longer failing.
    let mut c = rig.checker(Fake::default()).await;
    let reports = drain(&mut c).await;
    assert!(
        matches!(&reports[0], PostLandReport::Bad { land: l, check, .. } if *l == land && check == TEST),
        "{reports:?}"
    );
    assert!(
        matches!(&reports[1], PostLandReport::Reverted { land: l, .. } if *l == land),
        "{reports:?}"
    );
    assert!(
        !c.runner().runs.iter().any(|(at, _, _)| *at == land),
        "the land's checks were not run again: {:?}",
        c.runner().runs
    );

    // No land attested both ways for one check.
    for (id, reports) in attested(&rig.engine).await {
        for check in [BUILD, TEST] {
            let verdicts: Vec<bool> = reports
                .iter()
                .filter(|(_, c, _)| c == check)
                .map(|(_, _, p)| *p)
                .collect();
            assert!(
                verdicts.iter().all(|p| *p) || verdicts.iter().all(|p| !*p),
                "{id} {check}: {verdicts:?}"
            );
        }
    }
    let fails = attested(&rig.engine).await[&land.to_string()]
        .iter()
        .filter(|(_, c, p)| c == TEST && !p)
        .count();
    assert_eq!(fails, 2, "retries + 1 failures");
    assert_eq!(entries(&rig.engine, "LandReverted").await.len(), 1);
    assert_eq!(
        land_states(&rig.engine).await.get(&land),
        Some(&LandCheckState::Reverted)
    );
}

/// Accept 4: a revert that conflicts with a later land lands nothing,
/// leaves a typed report, and the line is not modified.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_conflicting_revert_lands_nothing_and_blocks_the_line() {
    let rig = Rig::new(0).await;
    land(&rig.engine, &[("a.txt", "a\n")]).await;
    let bad = land(&rig.engine, &[("broken", "one\n")]).await;
    // A later land edits what the bad land added: undoing it conflicts.
    land(&rig.engine, &[("broken", "two\n")]).await;
    let before = tip(&rig.engine).await;
    let moves_before = entries(&rig.engine, "RefMoved").await.len();

    let mut c = rig.checker(Fake::default()).await;
    let reports = drain(&mut c).await;
    let blocked = match reports.last() {
        Some(PostLandReport::Blocked(b)) => b.clone(),
        other => panic!("expected blocked, got {other:?} in {reports:?}"),
    };
    assert_eq!(blocked.land, bad);
    assert_eq!(blocked.line, LINE);
    match &blocked.reason {
        BlockReason::RevertConflict { paths } => assert_eq!(paths, &["broken".to_owned()]),
        other => panic!("expected a conflict, got {other:?}"),
    }
    assert_eq!(tip(&rig.engine).await, before, "the line did not move");
    assert_eq!(entries(&rig.engine, "RefMoved").await.len(), moves_before);
    assert!(entries(&rig.engine, "LandReverted").await.is_empty());
    // Blocked stays blocked: no force, nothing more is tried.
    let runs = c.runner().runs.len();
    assert_eq!(
        c.step(0).await.expect("step"),
        PostLandReport::Blocked(blocked)
    );
    assert_eq!(c.runner().runs.len(), runs);
    assert_eq!(tip(&rig.engine).await, before);
}

/// Accept 5: the checker dies mid-bisect, then while posting attestations,
/// then after its revert landed but before it heard so. Each time it is
/// restarted from its saved position. No land is attested both ways and
/// exactly one revert lands.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_restart_never_contradicts_an_attestation_or_reverts_twice() {
    const N: usize = 9;
    const K: usize = 4;
    let rig = Rig::new(0).await;
    let landed = lands(&rig.engine, N, K).await;
    let bad = landed[K - 1];

    // Dies on its third run: after skip-ahead and one bisection step.
    let mut c = rig
        .checker(Fake {
            crash_at: Some(2),
            ..Fake::default()
        })
        .await;
    assert!(c.step(0).await.is_err(), "the run crashed");
    assert_eq!(c.runner().crashed, 1, "the crash fired");
    drop(c);
    assert!(
        attested(&rig.engine).await.is_empty(),
        "nothing posted mid-bisect"
    );

    // Restarted; dies after posting two attestations.
    *rig.faults.attest_after.lock().expect("faults") = Some(2);
    let mut c = rig.checker(Fake::default()).await;
    assert!(c.step(0).await.is_err(), "attest failed");
    assert_eq!(*rig.faults.attest_fired.lock().expect("faults"), 1);
    drop(c);
    assert_eq!(
        attested(&rig.engine)
            .await
            .values()
            .map(Vec::len)
            .sum::<usize>(),
        2
    );

    // Restarted; its revert lands, but the answer is lost and it dies.
    *rig.faults.land_lost.lock().expect("faults") = 1;
    let mut c = rig.checker(Fake::default()).await;
    let mut died = false;
    for _ in 0..5 {
        match c.step(0).await {
            Ok(PostLandReport::Bad { .. }) => continue,
            Ok(other) => panic!("expected to die landing the revert, got {other:?}"),
            Err(_) => {
                died = true;
                break;
            }
        }
    }
    assert!(died);
    assert_eq!(*rig.faults.land_fired.lock().expect("faults"), 1);
    drop(c);
    assert_eq!(entries(&rig.engine, "LandReverted").await.len(), 1);

    // Restarted once more: it finishes.
    let mut c = rig.checker(Fake::default()).await;
    let reports = drain(&mut c).await;
    assert!(
        !reports
            .iter()
            .any(|r| matches!(r, PostLandReport::Reverted { .. })),
        "a second revert: {reports:?}"
    );

    // One revert of k, and only one land more than the authors made.
    let reverted = entries(&rig.engine, "LandReverted").await;
    assert_eq!(reverted.len(), 1);
    assert_eq!(reverted[0].1["land"], bad.to_string());
    let moves = entries(&rig.engine, "RefMoved").await;
    assert_eq!(moves.len(), 1 + N + 1, "creation, N lands, one revert");

    // No land attested both ways for one check.
    for (land, reports) in attested(&rig.engine).await {
        for check in [BUILD, TEST] {
            let verdicts: Vec<bool> = reports
                .iter()
                .filter(|(_, c, _)| c == check)
                .map(|(_, _, p)| *p)
                .collect();
            assert!(
                verdicts.iter().all(|p| *p) || verdicts.iter().all(|p| !*p),
                "{land} {check}: {verdicts:?}"
            );
        }
    }
    // Everything settled on the server.
    let states = land_states(&rig.engine).await;
    for (i, id) in landed.iter().enumerate() {
        let want = if *id == bad {
            LandCheckState::Reverted
        } else {
            LandCheckState::Good
        };
        assert_eq!(states.get(id), Some(&want), "land {}", i + 1);
    }
    assert!(
        states
            .values()
            .all(|s| matches!(s, LandCheckState::Good | LandCheckState::Reverted)),
        "{states:?}"
    );
}

/// Accept 6: the draft-mode attester (the default) does nothing post-land:
/// a line full of lands, one of them bad, gets no attestation and no
/// revert from it.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_draft_mode_checks_no_lands_and_reverts_nothing() {
    let rig = Rig::new(0).await;
    lands(&rig.engine, 4, 2).await;
    let before = tip(&rig.engine).await;
    let mut cfg = rig.config();
    cfg.state_file = Some(rig.dir.path().join("state").join("lsn"));
    let mut a = Attester::new(rig.remote().await, cfg).expect("attester");
    assert_eq!(a.poll(0).await.expect("poll"), 0, "no draft queued");
    assert!(a.run_next().await.expect("run").is_none());
    assert!(attested(&rig.engine).await.is_empty());
    assert!(entries(&rig.engine, "LandReverted").await.is_empty());
    assert_eq!(tip(&rig.engine).await, before);
}

/// A bad land of a two-changeset stack is reverted whole: each changeset
/// undone, newest first, in one revert land.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_bad_stack_is_reverted_whole() {
    let rig = Rig::new(0).await;
    let good = land(&rig.engine, &[("a.txt", "a\n")]).await;
    let lower = commit(&rig.engine, Some(good), &[("broken", "yes\n")]).await;
    let upper = commit(&rig.engine, Some(lower), &[("b.txt", "b\n")]).await;
    let bad = match rig
        .engine
        .land(
            &admin(),
            Land {
                repo: REPO.to_owned(),
                head: upper,
                line: LINE.to_owned(),
                squash: false,
                merge: MergeMode::Line,
                deadline_ms: None,
                merged_from: None,
            },
        )
        .await
        .expect("land")
    {
        Landed::Landed { landed, .. } => landed,
        other => panic!("not landed: {other:?}"),
    };
    let mut c = rig.checker(Fake::default()).await;
    let reports = drain(&mut c).await;
    assert!(
        matches!(&reports[0], PostLandReport::Bad { land, .. } if *land == bad),
        "{reports:?}"
    );
    assert!(
        matches!(&reports[1], PostLandReport::Reverted { land, .. } if *land == bad),
        "{reports:?}"
    );
    assert!(at_tip(&rig.engine, "a.txt").await);
    assert!(!at_tip(&rig.engine, "broken").await);
    assert!(!at_tip(&rig.engine, "b.txt").await);
    let states = land_states(&rig.engine).await;
    assert_eq!(states.get(&good), Some(&LandCheckState::Good));
    assert_eq!(states.get(&bad), Some(&LandCheckState::Reverted));
    assert_eq!(entries(&rig.engine, "LandReverted").await.len(), 1);
}
