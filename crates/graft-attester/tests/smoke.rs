//! Smoke test (WI-2276): the real engine over the in-memory store, served by
//! graft-testkit's `TestServer`, with a tiny repository whose checks are
//! shell scripts in the tree. A passing draft gets two passing attestations;
//! a failing draft gets a failing one (and its later check is attested
//! failed without running); a check past its timeout is killed and failed.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use graft_attester::{Attester, Check, Config, Outcome, Report};
use graft_client::{Endpoint, Remote};
use graft_engine::{Engine, EngineConfig};
use graft_model::{
    Author, AuthorKind, Change, ChangesetId, EntryKind, Metadata, ObjectId, ObjectKind, RepoPath,
    Timestamp,
};
use graft_proto::api::{AdminApi, ObjectApi, RepoApi, WorkspaceApi};
use graft_proto::error::ApiResult;
use graft_proto::types::*;
use graft_proto::{ApiError, CallContext, Frame, Principal, PrincipalKind};
use graft_store::memory::MemoryStore;
use graft_testkit::clock::FakeClock;
use graft_testkit::server::TestServer;

const REPO: &str = graft_attester::DEFAULT_REPO;
const LINE: &str = graft_attester::DEFAULT_LINE;

type Eng = Engine<MemoryStore, MemoryStore>;

/// The engine, answering every call as the attester's principal whatever
/// the transport says: a service token with the scopes `up.sh` mints.
struct AsAttester {
    engine: Arc<Eng>,
    faults: Arc<Faults>,
}

/// Calls to fail with `Unavailable` before they reach the engine, as a
/// restarting server would.
#[derive(Default)]
struct Faults {
    get_objects: Mutex<u32>,
    attest: Mutex<u32>,
    /// `GetWorkspace` calls to fail, counted down. `u32::MAX` never runs out.
    get_workspace: Mutex<u32>,
    /// Fail `GetWorkspace` only for this workspace name.
    get_workspace_name: Mutex<Option<String>>,
    /// `Watch` calls to fail. The reader makes one per page, so one failure
    /// is one failed per-entry fetch.
    watch: Mutex<u32>,
}

impl Faults {
    fn fire(counter: &Mutex<u32>) -> ApiResult<()> {
        let mut left = counter.lock().expect("faults");
        if *left == 0 {
            return Ok(());
        }
        *left -= 1;
        Err(ApiError::Unavailable { retry_after_ms: 1 })
    }
}

fn attester_principal() -> Principal {
    Principal {
        subject: "graft-attester-trial".to_owned(),
        kind: PrincipalKind::Service,
        session: None,
        work_item: None,
        scopes: vec![format!("repo:{REPO}:read"), format!("repo:{REPO}:attest")],
    }
}

impl AsAttester {
    fn cx(&self, cx: &CallContext) -> CallContext {
        CallContext {
            principal: attester_principal(),
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

impl ObjectApi for AsAttester {
    async fn get_objects(&self, cx: &CallContext, req: GetObjects) -> ApiResult<Objects> {
        Faults::fire(&self.faults.get_objects)?;
        self.engine.get_objects(&self.cx(cx), req).await
    }
    async fn has_objects(&self, cx: &CallContext, req: HasObjects) -> ApiResult<Vec<ObjectId>> {
        self.engine.has_objects(&self.cx(cx), req).await
    }
    async fn put_objects(&self, cx: &CallContext, req: PutObjects) -> ApiResult<Vec<ObjectId>> {
        self.engine.put_objects(&self.cx(cx), req).await
    }
}

impl WorkspaceApi for AsAttester {
    async fn open_workspace(
        &self,
        cx: &CallContext,
        req: OpenWorkspace,
    ) -> ApiResult<WorkspaceView> {
        self.engine.open_workspace(&self.cx(cx), req).await
    }
    async fn get_workspace(&self, cx: &CallContext, req: WorkspaceRef) -> ApiResult<WorkspaceView> {
        let named = self
            .faults
            .get_workspace_name
            .lock()
            .expect("faults")
            .clone();
        if named.as_deref().is_none_or(|n| n == req.name) {
            Faults::fire(&self.faults.get_workspace)?;
        }
        self.engine.get_workspace(&self.cx(cx), req).await
    }
    async fn list_workspaces(
        &self,
        cx: &CallContext,
        repo: String,
        page: Page,
    ) -> ApiResult<Vec<WorkspaceView>> {
        self.engine.list_workspaces(&self.cx(cx), repo, page).await
    }
    async fn renew_lease(
        &self,
        cx: &CallContext,
        req: WorkspaceRef,
        lease_ms: u64,
    ) -> ApiResult<WorkspaceView> {
        self.engine.renew_lease(&self.cx(cx), req, lease_ms).await
    }
    async fn set_head(&self, cx: &CallContext, req: SetHead) -> ApiResult<WorkspaceView> {
        self.engine.set_head(&self.cx(cx), req).await
    }
    async fn sync(
        &self,
        cx: &CallContext,
        req: WorkspaceRef,
        onto: Option<ChangesetId>,
    ) -> ApiResult<Synced> {
        self.engine.sync(&self.cx(cx), req, onto).await
    }
    async fn stack(
        &self,
        cx: &CallContext,
        req: WorkspaceRef,
        op: StackOp,
    ) -> ApiResult<WorkspaceView> {
        self.engine.stack(&self.cx(cx), req, op).await
    }
    async fn close_workspace(
        &self,
        cx: &CallContext,
        req: WorkspaceRef,
        keep_drafts: bool,
    ) -> ApiResult<()> {
        self.engine
            .close_workspace(&self.cx(cx), req, keep_drafts)
            .await
    }

    async fn get_conflicts(
        &self,
        _cx: &graft_proto::CallContext,
        _req: graft_proto::types::GetConflicts,
    ) -> graft_proto::error::ApiResult<graft_proto::types::Conflicts> {
        Err(graft_proto::ApiError::unimplemented("GetConflicts"))
    }

    async fn resolve(
        &self,
        _cx: &graft_proto::CallContext,
        _req: graft_proto::types::Resolve,
    ) -> graft_proto::error::ApiResult<graft_proto::types::Resolved> {
        Err(graft_proto::ApiError::unimplemented("Resolve"))
    }
}

impl RepoApi for AsAttester {
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
    land(Land) -> Landed;
    }

    async fn attest(&self, cx: &CallContext, req: Attest) -> ApiResult<()> {
        Faults::fire(&self.faults.attest)?;
        self.engine.attest(&self.cx(cx), req).await
    }

    async fn watch(&self, cx: &CallContext, req: Watch) -> ApiResult<JournalPage> {
        Faults::fire(&self.faults.watch)?;
        self.engine.watch(&self.cx(cx), req).await
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
        request_id: "smoke".to_owned(),
        deadline_ms: None,
    }
}

fn path(s: &str) -> RepoPath {
    RepoPath::new(s).expect("path")
}

/// A changeset on `parent` putting `files`, in `workspace` if given.
async fn commit(
    engine: &Eng,
    parent: Option<ChangesetId>,
    files: &[(&str, &str)],
    workspace: Option<&str>,
) -> ChangesetId {
    let mut changes: Vec<Change> = files
        .iter()
        .map(|(p, c)| Change::Put {
            path: path(p),
            kind: EntryKind::File,
            id: ObjectId::hash(ObjectKind::Blob, c.as_bytes()),
        })
        .collect();
    changes.sort_by(|a, b| a.path().cmp(b.path()));
    let inline_blobs = files
        .iter()
        .map(|(_, c)| Frame::encode(ObjectKind::Blob, c.as_bytes()))
        .collect();
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
                    kind: AuthorKind::Human,
                },
                authored_at: Timestamp {
                    ms: 1_700_000_000_000,
                    offset_min: 0,
                },
                message: "smoke".to_owned(),
                metadata: Metadata::new(),
                inline_blobs,
                workspace: workspace.map(str::to_owned),
            },
        )
        .await
        .expect("commit")
        .id
}

/// A repository whose `main` holds the check scripts, and an engine.
async fn repo() -> (Arc<Eng>, ChangesetId) {
    repo_with(EngineConfig::default()).await
}

/// A repository whose journal pages are `default_page` entries. A request
/// that asks for page size 0 gets this size, so a test can place drafts on
/// later pages.
async fn repo_with(mut config: EngineConfig) -> (Arc<Eng>, ChangesetId) {
    config.default_page = config.default_page.max(1);
    let store = Arc::new(MemoryStore::new());
    let clock = Arc::new(FakeClock::at(1_700_000_000_000));
    let engine = Arc::new(Engine::new(store.clone(), store, clock, config));
    engine
        .create_repo(&admin(), REPO.to_owned())
        .await
        .expect("create repo");
    // build.sh prints its environment, so the test can see what a check
    // inherits; test.sh fails when the tree says so.
    let root = commit(
        &engine,
        None,
        &[
            (
                "build.sh",
                "env | sort\necho building >&2\nif [ -n \"$RUNS\" ]; then echo run >> \"$RUNS\"; fi\n",
            ),
            ("test.sh", "test \"$(cat verdict)\" = pass\n"),
            ("verdict", "pass\n"),
        ],
        None,
    )
    .await;
    // `main` is the followed line; the trial's integration line forks it.
    for line in ["main", LINE] {
        engine
            .update_ref(
                &admin(),
                UpdateRef {
                    repo: REPO.to_owned(),
                    name: line.to_owned(),
                    expected: None,
                    new: root,
                    kind: RefKind::Line,
                },
            )
            .await
            .expect("line");
    }
    (engine, root)
}

/// A draft in a new workspace `name` on the trial line.
async fn draft(engine: &Eng, root: ChangesetId, name: &str, files: &[(&str, &str)]) -> ChangesetId {
    draft_on(engine, root, name, LINE, files).await
}

/// A draft in a new workspace `name` on `line`.
async fn draft_on(
    engine: &Eng,
    root: ChangesetId,
    name: &str,
    line: &str,
    files: &[(&str, &str)],
) -> ChangesetId {
    engine
        .open_workspace(
            &admin(),
            OpenWorkspace {
                repo: REPO.to_owned(),
                name: Some(name.to_owned()),
                line: line.to_owned(),
                at: None,
                lease_ms: 3_600_000,
            },
        )
        .await
        .expect("open workspace");
    commit(engine, Some(root), files, Some(name)).await
}

/// The attestations journaled so far: `(changeset, check, passed, url)`.
async fn attestations(engine: &Eng) -> Vec<(String, String, bool, String)> {
    let page = engine
        .watch(
            &admin(),
            Watch {
                repo: REPO.to_owned(),
                from_lsn: 1,
                kinds: vec!["Attested".to_owned()],
                limit: 0,
                wait_ms: 0,
            },
        )
        .await
        .expect("watch");
    page.entries
        .iter()
        .map(|e| {
            let v: serde_json::Value = serde_json::from_str(&e.body).expect("json");
            (
                v["changeset"].as_str().expect("changeset").to_owned(),
                v["check"].as_str().expect("check").to_owned(),
                v["passed"].as_bool().expect("passed"),
                v["url"].as_str().expect("url").to_owned(),
            )
        })
        .collect()
}

async fn attester(
    engine: &Arc<Eng>,
    root: &std::path::Path,
    checks: &[&str],
    timeout: Duration,
) -> (TestServer, Attester<Remote>, Arc<Faults>) {
    let faults = Arc::new(Faults::default());
    let server = TestServer::tcp_workspaces(Arc::new(AsAttester {
        engine: engine.clone(),
        faults: faults.clone(),
    }))
    .await;
    let remote = Remote::connect(Endpoint::Server(server.url().to_owned()))
        .await
        .expect("connect")
        .with_token("not-a-real-token");
    let mut cfg = Config::new(REPO, LINE, root);
    cfg.checks = checks
        .iter()
        .map(|c| Check::parse(c).expect("check"))
        .collect();
    cfg.env = vec![
        ("ATTESTER_SMOKE".to_owned(), "1".to_owned()),
        // build.sh appends a line per run here.
        ("RUNS".to_owned(), root.join("runs").display().to_string()),
    ];
    cfg.timeout = timeout;
    cfg.wait_ms = 0;
    cfg.idle = Duration::from_millis(10);
    cfg.retry_delay = Duration::from_millis(1);
    (
        server,
        Attester::new(remote, cfg).expect("attester"),
        faults,
    )
}

fn checked(report: Option<Report>) -> (ChangesetId, Vec<graft_attester::CheckResult>) {
    match report.expect("a draft ran") {
        Report::Checked { draft, results } => (draft, results),
        other => panic!("expected a checked draft, got {other:?}"),
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_passing_draft_gets_two_passing_attests_and_a_failing_one_a_failing_attest() {
    let (engine, root) = repo().await;
    let good = draft(&engine, root, "good", &[("src.txt", "hello\n")]).await;
    let bad = draft(&engine, root, "bad", &[("verdict", "fail\n")]).await;

    let dir = tempfile::tempdir().expect("tempdir");
    let (_server, mut a, _) = attester(
        &engine,
        dir.path(),
        &["ci/build=sh build.sh", "ci/test=sh test.sh"],
        Duration::from_secs(60),
    )
    .await;
    assert_eq!(a.poll(0).await.expect("poll"), 2);

    let (id, results) = checked(a.run_next().await.expect("run good"));
    assert_eq!(id, good);
    assert_eq!(
        results
            .iter()
            .map(|r| r.outcome.clone())
            .collect::<Vec<_>>(),
        [Outcome::Passed, Outcome::Passed]
    );
    assert!(results.iter().all(|r| r.attested.is_ok()), "{results:?}");

    let (id, results) = checked(a.run_next().await.expect("run bad"));
    assert_eq!(id, bad);
    assert_eq!(results[0].outcome, Outcome::Passed);
    assert_eq!(results[1].outcome, Outcome::Failed(Some(1)));
    assert!(a.run_next().await.expect("nothing left").is_none());

    let got = attestations(&engine).await;
    let want = [
        (good, "ci/build", true),
        (good, "ci/test", true),
        (bad, "ci/build", true),
        (bad, "ci/test", false),
    ];
    assert_eq!(
        got.iter()
            .map(|(c, k, p, _)| (c.clone(), k.clone(), *p))
            .collect::<Vec<_>>(),
        want.iter()
            .map(|(c, k, p)| (c.to_string(), (*k).to_owned(), *p))
            .collect::<Vec<_>>()
    );

    // Each attestation links its log, which holds the check's output.
    let (_, _, _, url) = &got[0];
    let log_path = url.strip_prefix("file://").expect("a file URL");
    assert!(log_path.ends_with(&format!("{good}/ci-build.log")), "{url}");
    let log = std::fs::read_to_string(log_path).expect("build log");
    assert!(log.contains("building"), "{log}");
    assert!(log.contains("# graft-attester: passed"), "{log}");
    // The check's environment is the configured one only: nothing of the
    // attester's (cargo's, the test runner's) leaks into it.
    assert!(log.contains("ATTESTER_SMOKE=1"), "{log}");
    assert!(!log.contains("CARGO"), "{log}");
    assert!(!log.contains("not-a-real-token"), "{log}");

    // The scratch trees are gone; the read position moved past both drafts.
    assert_eq!(
        std::fs::read_dir(dir.path().join("work"))
            .expect("work")
            .count(),
        0
    );
    let saved: u64 = std::fs::read_to_string(dir.path().join("state/lsn"))
        .expect("state")
        .trim()
        .parse()
        .expect("lsn");
    assert_eq!(saved, a.next_lsn());
    assert_eq!(a.poll(0).await.expect("poll again"), 0, "nothing re-queued");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_check_past_its_timeout_is_killed_and_the_rest_fail_unrun() {
    let (engine, root) = repo().await;
    let slow = draft(&engine, root, "slow", &[("src.txt", "slow\n")]).await;
    let dir = tempfile::tempdir().expect("tempdir");
    let (_server, mut a, _) = attester(
        &engine,
        dir.path(),
        &["ci/build=sh -c sleep${IFS}300", "ci/test=sh test.sh"],
        Duration::from_secs(1),
    )
    .await;
    assert_eq!(a.poll(0).await.expect("poll"), 1);
    let started = std::time::Instant::now();
    let (id, results) = checked(a.run_next().await.expect("run"));
    assert!(
        started.elapsed() < Duration::from_secs(60),
        "killed at the timeout"
    );
    assert_eq!(id, slow);
    assert_eq!(results[0].outcome, Outcome::TimedOut);
    assert_eq!(results[1].outcome, Outcome::Skipped);
    let got = attestations(&engine).await;
    assert_eq!(
        got.iter()
            .map(|(c, k, p, _)| (c.clone(), k.clone(), *p))
            .collect::<Vec<_>>(),
        [
            (slow.to_string(), "ci/build".to_owned(), false),
            (slow.to_string(), "ci/test".to_owned(), false),
        ]
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_draft_that_is_no_longer_a_draft_is_skipped() {
    let (engine, root) = repo().await;
    let first = draft(&engine, root, "w", &[("src.txt", "one\n")]).await;
    let dir = tempfile::tempdir().expect("tempdir");
    let (_server, mut a, _) = attester(
        &engine,
        dir.path(),
        &["ci/build=true"],
        Duration::from_secs(30),
    )
    .await;
    assert_eq!(a.poll(0).await.expect("poll"), 1);
    // Closing the workspace abandons its queued draft before it runs.
    engine
        .close_workspace(
            &admin(),
            WorkspaceRef {
                repo: REPO.to_owned(),
                name: "w".to_owned(),
            },
            false,
        )
        .await
        .expect("close");
    match a.run_next().await.expect("run") {
        Some(Report::Skipped { draft, state }) => {
            assert_eq!(draft, first);
            assert_eq!(state, "abandoned");
        }
        other => panic!("expected a skip, got {other:?}"),
    }
    assert!(attestations(&engine).await.is_empty());

    // A draft whose workspace closed before it was read is queued (the
    // journal names its line) and skipped when its turn comes.
    let second = draft(&engine, root, "v", &[("src.txt", "two\n")]).await;
    close(&engine, "v").await;
    assert_eq!(a.poll(0).await.expect("poll"), 1);
    match a.run_next().await.expect("run") {
        Some(Report::Skipped { draft, state }) => {
            assert_eq!(draft, second);
            assert_eq!(state, "abandoned");
        }
        other => panic!("expected a skip, got {other:?}"),
    }
}

/// `(check, passed)` of each attestation, in order.
fn verdicts(got: &[(String, String, bool, String)]) -> Vec<(String, bool)> {
    got.iter().map(|(_, k, p, _)| (k.clone(), *p)).collect()
}

fn both(passed: bool) -> Vec<(String, bool)> {
    vec![
        ("ci/build".to_owned(), passed),
        ("ci/test".to_owned(), passed),
    ]
}

const CHECKS: [&str; 2] = ["ci/build=sh build.sh", "ci/test=sh test.sh"];

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_checkout_that_fails_transiently_is_retried() {
    let (engine, root) = repo().await;
    let good = draft(&engine, root, "w", &[("src.txt", "hello\n")]).await;
    let dir = tempfile::tempdir().expect("tempdir");
    let (_server, mut a, faults) =
        attester(&engine, dir.path(), &CHECKS, Duration::from_secs(60)).await;
    assert_eq!(a.poll(0).await.expect("poll"), 1);
    // The server goes away during the checkout: the draft waits, unattested.
    *faults.get_objects.lock().expect("faults") = 1;
    assert!(a.run_next().await.is_err());
    assert_eq!(a.queued(), 1, "the draft is back in the queue");
    assert!(attestations(&engine).await.is_empty());
    // The next turn checks it.
    let (id, results) = checked(a.run_next().await.expect("retry"));
    assert_eq!(id, good);
    assert!(results.iter().all(|r| r.outcome.passed()), "{results:?}");
    assert_eq!(verdicts(&attestations(&engine).await), both(true));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_checkout_that_keeps_failing_attests_every_check_failed() {
    let (engine, root) = repo().await;
    let bad = draft(&engine, root, "w", &[("src.txt", "hello\n")]).await;
    let dir = tempfile::tempdir().expect("tempdir");
    let (_server, mut a, faults) =
        attester(&engine, dir.path(), &CHECKS, Duration::from_secs(60)).await;
    assert_eq!(a.poll(0).await.expect("poll"), 1);
    *faults.get_objects.lock().expect("faults") = u32::MAX;
    let attempts = a.config().max_attempts;
    for _ in 1..attempts {
        assert!(a.run_next().await.is_err());
    }
    match a.run_next().await.expect("gives up") {
        Some(Report::NotCheckedOut {
            draft,
            error,
            results,
        }) => {
            assert_eq!(draft, bad);
            assert!(
                error.contains(&format!("gave up after {attempts} tries")),
                "{error}"
            );
            assert!(results.iter().all(|r| r.attested.is_ok()), "{results:?}");
        }
        other => panic!("expected a failed checkout, got {other:?}"),
    }
    let got = attestations(&engine).await;
    assert_eq!(verdicts(&got), both(false));
    let log_path = got[0].3.strip_prefix("file://").expect("a file URL");
    let log = std::fs::read_to_string(log_path).expect("log");
    assert!(log.contains("checkout failed"), "{log}");
    assert_eq!(a.queued(), 0);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_failed_attest_is_posted_again_without_rerunning_the_checks() {
    let (engine, root) = repo().await;
    let good = draft(&engine, root, "w", &[("src.txt", "hello\n")]).await;
    let dir = tempfile::tempdir().expect("tempdir");
    let (_server, mut a, faults) =
        attester(&engine, dir.path(), &CHECKS, Duration::from_secs(60)).await;
    let tries = a.config().attest_tries;
    assert_eq!(a.poll(0).await.expect("poll"), 1);
    // ci/build's Attest fails on every in-place try; ci/test's goes through.
    *faults.attest.lock().expect("faults") = tries;
    assert!(
        a.run_next().await.is_err(),
        "an unposted result is an error"
    );
    assert_eq!(a.queued(), 1, "the draft is back in the queue");
    assert_eq!(
        verdicts(&attestations(&engine).await),
        [("ci/test".to_owned(), true)]
    );
    // The next turn posts ci/build only, and runs nothing.
    let (id, results) = checked(a.run_next().await.expect("post again"));
    assert_eq!(id, good);
    assert!(results.iter().all(|r| r.attested.is_ok()), "{results:?}");
    let mut got = verdicts(&attestations(&engine).await);
    got.sort();
    assert_eq!(got, both(true));
    let ran = std::fs::read_to_string(dir.path().join("runs")).expect("runs");
    assert_eq!(ran.lines().count(), 1, "the build ran once");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_tree_that_cannot_be_checked_out_attests_failed_at_once() {
    let (engine, root) = repo().await;
    // A name the client reserves for its own state: refused every time.
    let bad = draft(&engine, root, "w", &[(".GRAFT/config", "x\n")]).await;
    let dir = tempfile::tempdir().expect("tempdir");
    let (_server, mut a, _) = attester(&engine, dir.path(), &CHECKS, Duration::from_secs(60)).await;
    assert_eq!(a.poll(0).await.expect("poll"), 1);
    match a.run_next().await.expect("first turn") {
        Some(Report::NotCheckedOut { draft, error, .. }) => {
            assert_eq!(draft, bad);
            assert!(!error.contains("gave up"), "not retried: {error}");
        }
        other => panic!("expected a failed checkout, got {other:?}"),
    }
    assert_eq!(verdicts(&attestations(&engine).await), both(false));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn only_drafts_on_the_configured_line_are_checked() {
    let (engine, root) = repo().await;
    let _elsewhere = draft_on(&engine, root, "on-main", "main", &[("src.txt", "main\n")]).await;
    let trial = draft(&engine, root, "on-trial", &[("src.txt", "trial\n")]).await;
    let dir = tempfile::tempdir().expect("tempdir");
    let (_server, mut a, _) = attester(&engine, dir.path(), &CHECKS, Duration::from_secs(60)).await;
    assert_eq!(
        a.poll(0).await.expect("poll"),
        1,
        "the main draft is not queued"
    );
    let (id, _) = checked(a.run_next().await.expect("run"));
    assert_eq!(id, trial);
    assert!(a.run_next().await.expect("nothing left").is_none());
    assert!(
        attestations(&engine)
            .await
            .iter()
            .all(|(c, ..)| *c == trial.to_string())
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn keep_going_runs_every_check_after_a_failure() {
    let (engine, root) = repo().await;
    let bad = draft(&engine, root, "w", &[("verdict", "fail\n")]).await;
    let dir = tempfile::tempdir().expect("tempdir");
    // The failing check first; with stop_on_failure off, ci/build still runs.
    let (server, a, _) = attester(
        &engine,
        dir.path(),
        &["ci/test=sh test.sh", "ci/build=sh build.sh"],
        Duration::from_secs(60),
    )
    .await;
    let mut cfg = a.config().clone();
    cfg.stop_on_failure = false;
    drop(a);
    let remote = Remote::connect(Endpoint::Server(server.url().to_owned()))
        .await
        .expect("connect");
    let mut a = Attester::new(remote, cfg).expect("attester");
    assert_eq!(a.poll(0).await.expect("poll"), 1);
    let (id, results) = checked(a.run_next().await.expect("run"));
    assert_eq!(id, bad);
    assert_eq!(results[0].outcome, Outcome::Failed(Some(1)));
    assert_eq!(results[1].outcome, Outcome::Passed);
}

/// The journal position of `id`'s `ChangesetCreated` entry.
async fn created_lsn(engine: &Eng, id: ChangesetId) -> u64 {
    let page = engine
        .watch(
            &admin(),
            Watch {
                repo: REPO.to_owned(),
                from_lsn: 1,
                kinds: vec!["ChangesetCreated".to_owned()],
                limit: 0,
                wait_ms: 0,
            },
        )
        .await
        .expect("watch");
    page.entries
        .iter()
        .find(|e| e.body.contains(&id.to_string()))
        .expect("created entry")
        .lsn
}

/// Closes workspace `name`, abandoning its drafts.
async fn close(engine: &Eng, name: &str) {
    engine
        .close_workspace(
            &admin(),
            WorkspaceRef {
                repo: REPO.to_owned(),
                name: name.to_owned(),
            },
            false,
        )
        .await
        .expect("close");
}

/// Drafts the attester has attested, as changeset ids.
async fn attested_ids(engine: &Eng) -> Vec<String> {
    attestations(engine)
        .await
        .into_iter()
        .map(|(id, ..)| id)
        .collect()
}

/// The saved read position.
fn saved_lsn(dir: &std::path::Path) -> u64 {
    std::fs::read_to_string(dir.join("state/lsn"))
        .expect("state")
        .trim()
        .parse()
        .expect("lsn")
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_failed_workspace_lookup_is_retried_and_neighbours_are_processed() {
    let (engine, root) = repo().await;
    let before = draft(&engine, root, "before", &[("src.txt", "b\n")]).await;
    let middle = draft(&engine, root, "middle", &[("src.txt", "m\n")]).await;
    let after = draft(&engine, root, "after", &[("src.txt", "a\n")]).await;
    let middle_lsn = created_lsn(&engine, middle).await;

    let dir = tempfile::tempdir().expect("tempdir");
    // Start at the middle entry, with `before` already handled.
    std::fs::create_dir_all(dir.path().join("state")).expect("state dir");
    std::fs::write(dir.path().join("state/lsn"), format!("{middle_lsn}\n")).expect("state");
    let (_server, mut a, faults) =
        attester(&engine, dir.path(), &CHECKS, Duration::from_secs(60)).await;
    *faults.watch.lock().expect("faults") = 1;
    let err = a.poll(0).await.expect_err("the fetch of entry N failed");
    assert!(err.to_string().contains("unavailable"), "{err}");
    assert!(
        a.run_next()
            .await
            .expect("nothing past the failure")
            .is_none()
    );
    assert_eq!(saved_lsn(dir.path()), middle_lsn);

    // Retried, the fetch succeeds: N and the entry after it are processed.
    // `before` was already handled (the cursor started at N).
    assert_eq!(a.poll(0).await.expect("retry"), 2);
    let (id, results) = checked(a.run_next().await.expect("middle"));
    assert_eq!(id, middle);
    assert!(results.iter().all(|r| r.outcome.passed()), "{results:?}");
    let (id, results) = checked(a.run_next().await.expect("after"));
    assert_eq!(id, after);
    assert!(results.iter().all(|r| r.outcome.passed()), "{results:?}");
    assert!(a.run_next().await.expect("nothing left").is_none());
    let got = attested_ids(&engine).await;
    assert!(
        !got.iter().any(|id| id == &before.to_string()),
        "before was already handled: {got:?}"
    );
    for id in [&middle, &after] {
        assert_eq!(
            got.iter().filter(|got| *got == &id.to_string()).count(),
            2,
            "{id} processed once: {got:?}"
        );
    }
    assert!(saved_lsn(dir.path()) > created_lsn(&engine, after).await);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_permanently_failing_lookup_does_not_stop_later_entries() {
    let (engine, root) = repo().await;
    let before = draft(&engine, root, "before", &[("src.txt", "b\n")]).await;
    let stuck = draft(&engine, root, "stuck", &[("src.txt", "s\n")]).await;
    let after = draft(&engine, root, "after", &[("src.txt", "a\n")]).await;
    let stuck_lsn = created_lsn(&engine, stuck).await;

    let dir = tempfile::tempdir().expect("tempdir");
    // Start at the stuck entry, with `before` already handled. The page
    // then holds the stuck entry and the one after it.
    std::fs::create_dir_all(dir.path().join("state")).expect("state dir");
    std::fs::write(dir.path().join("state/lsn"), format!("{stuck_lsn}\n")).expect("state");
    let (_server, mut a, faults) =
        attester(&engine, dir.path(), &CHECKS, Duration::from_secs(60)).await;
    // Drop the line the journal already recorded, so the stuck entry's
    // fetch is a `GetWorkspace` that never succeeds. Past the bound it is
    // recorded as failed and the cursor moves past it. The entry after it
    // is still processed.
    a.forget_lines();
    *faults.get_workspace.lock().expect("faults") = u32::MAX;
    // The first failing lookup is reported and the page continues, so the
    // entry after the stuck one is queued on that same read.
    let err = a.poll(0).await.expect_err("lookup fails");
    assert!(err.to_string().contains("unavailable"), "{err}");
    let (id, results) = checked(a.run_next().await.expect("after"));
    assert_eq!(id, after);
    assert!(results.iter().all(|r| r.outcome.passed()), "{results:?}");
    assert!(a.run_next().await.expect("stuck is not queued").is_none());
    for n in 2..8 {
        if let Err(err) = a.poll(0).await {
            assert!(err.to_string().contains("unavailable"), "try {n}: {err}");
        } else {
            break;
        }
        // The saved position stays on the unhandled entry, so this page is
        // read again. Drain whatever that re-read queued; `stuck` is not in it.
        while let Some(report) = a.run_next().await.expect("queue") {
            let (id, _) = checked(Some(report));
            assert_ne!(id, stuck);
        }
    }
    // Past the bound the stuck entry is recorded as failed and the cursor
    // moves past it. It is not queued, and it is not asked about again.
    loop {
        let _ = a.poll(0).await.expect("past the bound");
        let mut more = false;
        while let Some(report) = a.run_next().await.expect("queue") {
            more = true;
            let (id, _) = checked(Some(report));
            assert_ne!(id, stuck);
        }
        if saved_lsn(dir.path()) > stuck_lsn && !more {
            break;
        }
    }
    assert!(saved_lsn(dir.path()) > stuck_lsn);
    let got = attested_ids(&engine).await;
    assert!(
        !got.iter().any(|id| id == &before.to_string()),
        "before was already handled: {got:?}"
    );
    assert!(
        got.iter().any(|id| *id == after.to_string()),
        "after was processed: {got:?}"
    );
    assert!(
        !got.iter().any(|id| id == &stuck.to_string()),
        "the failing entry was not processed: {got:?}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_failed_lookup_then_a_full_queue_keeps_the_failed_entry() {
    let (engine, root) = repo().await;
    let before = draft(&engine, root, "before", &[("src.txt", "b\n")]).await;
    let failed = draft(&engine, root, "failed", &[("src.txt", "f\n")]).await;
    let filler = draft(&engine, root, "filler", &[("src.txt", "i\n")]).await;
    let full = draft(&engine, root, "full", &[("src.txt", "q\n")]).await;
    let failed_lsn = created_lsn(&engine, failed).await;

    let dir = tempfile::tempdir().expect("tempdir");
    std::fs::create_dir_all(dir.path().join("state")).expect("state dir");
    std::fs::write(dir.path().join("state/lsn"), format!("{failed_lsn}\n")).expect("state");
    let (_server, mut a, faults) =
        attester(&engine, dir.path(), &CHECKS, Duration::from_secs(60)).await;
    a.set_queue_cap(1);
    a.forget_lines();
    // The first GetWorkspace is the failed entry. Later ones on the page
    // succeed, and the queue fills on the entry after the filler.
    *faults.get_workspace_name.lock().expect("faults") = Some("failed".to_owned());
    *faults.get_workspace.lock().expect("faults") = u32::MAX;
    let err = a.poll(0).await.expect_err("lookup fails");
    assert!(err.to_string().contains("unavailable"), "{err}");
    // The cursor stays on the failed entry, not on the full-queue entry.
    assert_eq!(saved_lsn(dir.path()), failed_lsn);
    let (id, _) = checked(
        a.run_next()
            .await
            .expect("one draft queued before the queue filled"),
    );
    assert_eq!(id, filler);
    assert!(a.run_next().await.expect("the rest waits").is_none());
    // The named workspace was the one that failed. The retry succeeds.
    *faults.get_workspace.lock().expect("faults") = 0;

    // Retry: the failed entry is processed, then the one the full queue held.
    let mut got = Vec::new();
    for _ in 0..4 {
        let _ = a.poll(0).await.expect("retry");
        while let Some(report) = a.run_next().await.expect("queue") {
            got.push(checked(Some(report)).0);
        }
        if got.contains(&failed) && got.contains(&full) {
            break;
        }
    }
    assert!(got.contains(&failed), "entry 5 after retry: {got:?}");
    assert!(got.contains(&full), "the held entry after retry: {got:?}");

    // Restart from the saved position and read again: both were handled,
    // so neither is skipped and neither is required to run twice. The
    // failed entry's LSN was processed, which a cursor saved at the full
    // entry would have dropped.
    let remote = Remote::connect(Endpoint::Server(_server.url().to_owned()))
        .await
        .expect("connect");
    let mut cfg = Config::new(REPO, LINE, dir.path());
    cfg.checks = CHECKS
        .iter()
        .map(|c| Check::parse(c).expect("check"))
        .collect();
    cfg.wait_ms = 0;
    cfg.queue_cap = 1;
    let mut restarted = Attester::new(remote, cfg).expect("restart");
    assert!(restarted.next_lsn() > failed_lsn);
    let _ = restarted.poll(0).await.expect("reread");
    let attested = attested_ids(&engine).await;
    assert!(
        attested.iter().any(|id| id == &failed.to_string()),
        "failed entry processed: {attested:?}"
    );
    assert!(
        !attested.iter().any(|id| id == &before.to_string()),
        "before was already handled: {attested:?}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_given_up_entry_does_not_hide_a_later_page() {
    // Eight entries a page, so the stuck draft is on the first page and
    // drafts recorded after it land on later pages.
    let (engine, root) = repo_with(EngineConfig {
        default_page: 8,
        ..EngineConfig::default()
    })
    .await;
    let stuck = draft(&engine, root, "stuck", &[("src.txt", "s\n")]).await;
    let mut later = Vec::new();
    for n in 0..40 {
        later.push(
            draft(
                &engine,
                root,
                &format!("w{n}"),
                &[("src.txt", &format!("{n}\n"))],
            )
            .await,
        );
    }
    let stuck_lsn = created_lsn(&engine, stuck).await;
    let dir = tempfile::tempdir().expect("tempdir");
    std::fs::create_dir_all(dir.path().join("state")).expect("state dir");
    std::fs::write(dir.path().join("state/lsn"), format!("{stuck_lsn}\n")).expect("state");
    let (_server, mut a, faults) =
        attester(&engine, dir.path(), &CHECKS, Duration::from_secs(60)).await;
    a.forget_lines();
    *faults.get_workspace_name.lock().expect("faults") = Some("stuck".to_owned());
    *faults.get_workspace.lock().expect("faults") = u32::MAX;
    for n in 1..=8 {
        if let Err(err) = a.poll(0).await {
            assert!(err.to_string().contains("unavailable"), "try {n}: {err}");
        }
        while let Some(report) = a.run_next().await.expect("queue") {
            let (id, _) = checked(Some(report));
            assert_ne!(id, stuck);
        }
    }
    // Given up: the cursor has moved past the stuck entry, and drafts
    // recorded after it (past where a pinned cursor could read) run.
    let mut seen = Vec::new();
    for _ in 0..30 {
        let _ = a.poll(0).await.expect("later pages");
        while let Some(report) = a.run_next().await.expect("queue") {
            seen.push(checked(Some(report)).0);
        }
        if seen.len() >= 16 {
            break;
        }
    }
    assert!(saved_lsn(dir.path()) > stuck_lsn);
    assert!(a.dropped() >= 1, "the given-up lookup was counted");
    // Page 3 starts after two pages of eight. Those drafts ran, so the
    // given-up entry did not pin the read to its own page.
    let later_page = &later[16..];
    assert!(
        later_page.iter().any(|id| seen.contains(id)),
        "a draft on a later page ran: {} of {}",
        seen.len(),
        later.len()
    );
    let attested = attested_ids(&engine).await;
    assert!(
        !attested.iter().any(|id| id == &stuck.to_string()),
        "the given-up entry was not processed"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_crash_mid_page_does_not_skip_an_unprocessed_entry() {
    let (engine, root) = repo().await;
    let before = draft(&engine, root, "before", &[("src.txt", "b\n")]).await;
    let middle = draft(&engine, root, "middle", &[("src.txt", "m\n")]).await;
    let _after = draft(&engine, root, "after", &[("src.txt", "a\n")]).await;
    let middle_lsn = created_lsn(&engine, middle).await;

    let dir = tempfile::tempdir().expect("tempdir");
    // Start at the middle entry, with `before` already handled.
    std::fs::create_dir_all(dir.path().join("state")).expect("state dir");
    std::fs::write(dir.path().join("state/lsn"), format!("{middle_lsn}\n")).expect("state");
    let (server, mut a, faults) =
        attester(&engine, dir.path(), &CHECKS, Duration::from_secs(60)).await;
    *faults.watch.lock().expect("faults") = 1;
    assert!(a.poll(0).await.is_err(), "the fault fired");
    // Acknowledged nothing new. The middle entry was fetched and failed.
    let acknowledged = saved_lsn(dir.path());
    assert_eq!(acknowledged, middle_lsn);
    let _ = before;

    // Crash, then restart from the saved position. The fault is spent, so
    // the unprocessed entry is read again and processed.
    drop(a);
    let remote = Remote::connect(Endpoint::Server(server.url().to_owned()))
        .await
        .expect("connect");
    let mut cfg = Config::new(REPO, LINE, dir.path());
    cfg.checks = CHECKS
        .iter()
        .map(|c| Check::parse(c).expect("check"))
        .collect();
    cfg.wait_ms = 0;
    let mut restarted = Attester::new(remote, cfg).expect("restart");
    assert_eq!(restarted.next_lsn(), middle_lsn);
    assert_eq!(restarted.poll(0).await.expect("reread"), 2);
    let (id, results) = checked(restarted.run_next().await.expect("middle"));
    assert_eq!(id, middle);
    assert!(results.iter().all(|r| r.outcome.passed()), "{results:?}");
    assert!(saved_lsn(dir.path()) > middle_lsn);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_workspace_closed_and_reopened_on_the_line_is_checked() {
    let (engine, root) = repo().await;
    let dir = tempfile::tempdir().expect("tempdir");
    // `w` comes and goes before the attester starts, and the attester's
    // saved position is past `w`'s WorkspaceOpened: it asks the server
    // about `w`, which no longer knows it.
    let first = draft(&engine, root, "w", &[("src.txt", "first\n")]).await;
    close(&engine, "w").await;
    let created = created_lsn(&engine, first).await;
    std::fs::create_dir_all(dir.path().join("state")).expect("state dir");
    std::fs::write(dir.path().join("state/lsn"), format!("{created}\n")).expect("state");
    let (_server, mut a, _) = attester(&engine, dir.path(), &CHECKS, Duration::from_secs(60)).await;
    assert_eq!(a.poll(0).await.expect("poll"), 0, "not found: not queued");
    // Reopened under the same name on the trial line, its new draft is
    // checked: "not found" was not remembered.
    let second = draft(&engine, root, "w", &[("src.txt", "second\n")]).await;
    assert_eq!(a.poll(0).await.expect("poll"), 1);
    let (id, results) = checked(a.run_next().await.expect("run"));
    assert_eq!(id, second);
    assert!(results.iter().all(|r| r.outcome.passed()), "{results:?}");
    assert!(a.run_next().await.expect("nothing left").is_none());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_workspace_reopened_on_another_line_is_not_checked() {
    let (engine, root) = repo().await;
    let dir = tempfile::tempdir().expect("tempdir");
    let (_server, mut a, _) = attester(&engine, dir.path(), &CHECKS, Duration::from_secs(60)).await;
    let first = draft(&engine, root, "w", &[("src.txt", "first\n")]).await;
    assert_eq!(a.poll(0).await.expect("poll"), 1);
    let (id, _) = checked(a.run_next().await.expect("run"));
    assert_eq!(id, first);
    // The same name, now on main: its line is read from the journal, not
    // from what was remembered for the closed workspace.
    close(&engine, "w").await;
    draft_on(&engine, root, "w", "main", &[("src.txt", "main\n")]).await;
    assert_eq!(a.poll(0).await.expect("poll"), 0);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_amended_draft_is_checked() {
    let (engine, root) = repo().await;
    let dir = tempfile::tempdir().expect("tempdir");
    let (_server, mut a, _) = attester(&engine, dir.path(), &CHECKS, Duration::from_secs(60)).await;
    let first = draft(&engine, root, "w", &[("src.txt", "first\n")]).await;
    assert_eq!(a.poll(0).await.expect("poll"), 1);
    let (id, _) = checked(a.run_next().await.expect("run"));
    assert_eq!(id, first);
    // `graft commit --amend`: the replacement is created outside the
    // workspace, so its `ChangesetCreated` says abandoned, and `SetHead`
    // adopts it, superseding `first`.
    let amended = commit(&engine, Some(root), &[("src.txt", "amended\n")], None).await;
    engine
        .set_head(
            &admin(),
            SetHead {
                workspace: WorkspaceRef {
                    repo: REPO.to_owned(),
                    name: "w".to_owned(),
                },
                expected: Some(first),
                new: Some(amended),
            },
        )
        .await
        .expect("set head");
    assert_eq!(
        a.poll(0).await.expect("poll"),
        1,
        "the head move queues the amended draft"
    );
    let (id, results) = checked(a.run_next().await.expect("run"));
    assert_eq!(id, amended);
    assert!(results.iter().all(|r| r.outcome.passed()), "{results:?}");
    assert!(a.run_next().await.expect("nothing left").is_none());
    assert!(
        attestations(&engine)
            .await
            .iter()
            .any(|(c, ..)| *c == amended.to_string())
    );
}
