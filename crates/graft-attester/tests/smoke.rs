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

const REPO: &str = "trial";

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
        scopes: vec![
            format!("repo:{REPO}:read"),
            format!("repo:{REPO}:attest"),
            // Until WI-2272 is on main, Attest takes a write scope; with it,
            // `attest` alone does and this line goes.
            format!("repo:{REPO}:write"),
        ],
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
    watch(Watch) -> JournalPage;
    }

    async fn attest(&self, cx: &CallContext, req: Attest) -> ApiResult<()> {
        Faults::fire(&self.faults.attest)?;
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
    let store = Arc::new(MemoryStore::new());
    let clock = Arc::new(FakeClock::at(1_700_000_000_000));
    let engine = Arc::new(Engine::new(
        store.clone(),
        store,
        clock,
        EngineConfig::default(),
    ));
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
    engine
        .update_ref(
            &admin(),
            UpdateRef {
                repo: REPO.to_owned(),
                name: "main".to_owned(),
                expected: None,
                new: root,
                kind: RefKind::Line,
            },
        )
        .await
        .expect("main");
    (engine, root)
}

/// A draft in a new workspace `name` on `main`.
async fn draft(engine: &Eng, root: ChangesetId, name: &str, files: &[(&str, &str)]) -> ChangesetId {
    engine
        .open_workspace(
            &admin(),
            OpenWorkspace {
                repo: REPO.to_owned(),
                name: Some(name.to_owned()),
                line: "main".to_owned(),
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
    let server = TestServer::tcp(Arc::new(AsAttester {
        engine: engine.clone(),
        faults: faults.clone(),
    }))
    .await;
    let remote = Remote::connect(Endpoint::Server(server.url().to_owned()))
        .await
        .expect("connect")
        .with_token("not-a-real-token");
    let mut cfg = Config::new(REPO, root);
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
    // Closing the workspace abandons its draft before the attester gets to it.
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
    assert_eq!(a.poll(0).await.expect("poll"), 1);
    match a.run_next().await.expect("run") {
        Some(Report::Skipped { draft, state }) => {
            assert_eq!(draft, first);
            assert_eq!(state, "abandoned");
        }
        other => panic!("expected a skip, got {other:?}"),
    }
    assert!(attestations(&engine).await.is_empty());
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
