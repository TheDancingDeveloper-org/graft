//! The GitHub Actions runner (WI-2625) against a fake GitHub API served on
//! loopback: no real network call is made. The fake records every request,
//! runs each dispatched workflow through queued, in progress and completed
//! as it is polled (by count, never by time), and, as the workflow's
//! `attest` job does, attests a pass for each job that succeeded. The
//! post-land tests drive the real engine, as `tests/post_land.rs` does, and
//! assert what ends up in the journal, the line and the land-checks rows.

use std::collections::{BTreeMap, BTreeSet};
use std::os::unix::fs::PermissionsExt as _;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::Router;
use axum::extract::State;
use axum::http::{HeaderMap, Method, StatusCode, Uri};
use graft_attester::github_runner::{
    GitHubConfig, GitHubRunner, GitHubToken, read_secret, request_id,
};
use graft_attester::post_land::{PostLand, PostLandReport, Runner};
use graft_attester::{Check, Config, Error, Outcome};
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
use graft_proto::{CallContext, Frame, Principal, PrincipalKind};
use graft_store::memory::MemoryStore;
use graft_testkit::clock::FakeClock;
use graft_testkit::server::TestServer;
use serde_json::{Value, json};

const REPO: &str = "graft";
const LINE: &str = "integration";
const FMT: &str = "ci/fmt";
const CLIPPY: &str = "ci/clippy";
const TEST: &str = "ci/test";
const CHECKS: [&str; 3] = [FMT, CLIPPY, TEST];
const PREFIX: &str = "graft/check/integration/";
const GH_TOKEN: &str = "ghp_fake0000secret1111token2222";

type Eng = Engine<MemoryStore, MemoryStore>;

// ---- the fake GitHub -------------------------------------------------------

/// How the fake answers.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
enum Mode {
    /// Runs go queued, in progress, completed.
    #[default]
    Normal,
    /// Every request answers 503, echoing the `Authorization` header.
    Outage,
    /// Dispatches are accepted, and no run ever appears.
    NeverStarts,
    /// Runs stay queued.
    StaysQueued,
    /// Runs stay in progress.
    RunsForever,
    /// Dispatches are refused 422 with this message.
    Refuse(String),
}

#[derive(Clone, Debug)]
struct FakeRun {
    id: u64,
    branch: String,
    changeset: String,
    request_id: String,
    /// Reads still answered `queued`, then `in_progress`.
    queued: u32,
    running: u32,
    /// Job name to conclusion.
    jobs: BTreeMap<String, String>,
    /// The run ended `cancelled`.
    cancelled: bool,
    attested: bool,
    /// Not dispatched by the runner under test (another checker's).
    foreign: bool,
}

#[derive(Default)]
struct GhState {
    mode: Mode,
    /// Jobs that fail on every snapshot.
    red: BTreeSet<String>,
    runs: Vec<FakeRun>,
    /// `(ref, inputs)` of each accepted dispatch.
    dispatches: Vec<(String, Value)>,
    cancels: Vec<u64>,
    /// Requests whose `Authorization` was not the token.
    bad_auth: u32,
    /// Requests carrying the token outside the header.
    leaked: u32,
    /// Passes the workflow attested, `(changeset, check)`.
    attested: Vec<(String, String)>,
    /// Jobs that end `cancelled` in runs dispatched from now on.
    cancel_jobs: BTreeSet<String>,
    /// Runs dispatched from now on end `cancelled`, every job with them.
    cancel_runs: bool,
    /// Most runs queued or in progress at once, counted at each dispatch
    /// with the new run.
    peak_in_flight: usize,
}

impl GhState {
    /// Runs queued or in progress now, read without advancing them.
    fn in_flight(&self) -> usize {
        self.runs
            .iter()
            .filter(|r| peek(r, &self.mode) != "completed")
            .count()
    }

    /// Adds a run of another checker on its own ref, finishing after
    /// `polls` observations.
    fn foreign_run(&mut self, polls: u32) {
        let id = 1000 + self.runs.len() as u64;
        self.runs.push(FakeRun {
            id,
            branch: format!("graft/check/other/{id}"),
            changeset: String::new(),
            request_id: format!("other-{id}"),
            queued: 0,
            running: polls,
            jobs: BTreeMap::new(),
            cancelled: false,
            attested: false,
            foreign: true,
        });
    }
}

struct Gh {
    state: Mutex<GhState>,
    /// The engine the workflow attests to and reads snapshots from.
    engine: Option<Arc<Eng>>,
}

struct FakeGitHub {
    gh: Arc<Gh>,
    api: String,
}

impl FakeGitHub {
    async fn start(engine: Option<Arc<Eng>>) -> FakeGitHub {
        let gh = Arc::new(Gh {
            state: Mutex::new(GhState::default()),
            engine,
        });
        let app = Router::new().fallback(handle).with_state(gh.clone());
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind");
        let addr = listener.local_addr().expect("addr");
        tokio::spawn(async move {
            axum::serve(listener, app).await.expect("serve");
        });
        FakeGitHub {
            gh,
            api: format!("http://{addr}"),
        }
    }

    fn state(&self) -> std::sync::MutexGuard<'_, GhState> {
        self.gh.state.lock().expect("fake")
    }

    fn set_mode(&self, mode: Mode) {
        self.state().mode = mode;
    }

    fn config(&self) -> GitHubConfig {
        let mut cfg = GitHubConfig::new("owner/graft", PREFIX).expect("config");
        cfg.api = self.api.clone();
        cfg.poll = Duration::ZERO;
        // Long enough never to be reached by a run that does start.
        cfg.start_timeout = Duration::from_secs(3600);
        cfg.run_timeout = Duration::from_secs(3600);
        cfg
    }

    fn runner_with(&self, cfg: GitHubConfig) -> GitHubRunner {
        GitHubRunner::new(cfg, GitHubToken::new(GH_TOKEN).expect("token")).expect("runner")
    }

    fn runner(&self) -> GitHubRunner {
        self.runner_with(self.config())
    }

    /// Requests were authenticated by the header alone.
    fn assert_token_only_in_header(&self) {
        let st = self.state();
        assert_eq!(st.bad_auth, 0, "a request without the token header");
        assert_eq!(st.leaked, 0, "the token outside the header");
    }
}

fn reply(status: StatusCode, body: Value) -> (StatusCode, String) {
    (status, body.to_string())
}

fn percent_decode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = Vec::new();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%'
            && i + 3 <= bytes.len()
            && let Ok(b) = u8::from_str_radix(&s[i + 1..i + 3], 16)
        {
            out.push(b);
            i += 3;
            continue;
        }
        out.push(if bytes[i] == b'+' { b' ' } else { bytes[i] });
        i += 1;
    }
    String::from_utf8(out).expect("utf8")
}

fn query(uri: &Uri) -> BTreeMap<String, String> {
    uri.query()
        .unwrap_or_default()
        .split('&')
        .filter_map(|kv| kv.split_once('='))
        .map(|(k, v)| (k.to_owned(), percent_decode(v)))
        .collect()
}

/// A run's status, without advancing it.
fn peek(run: &FakeRun, mode: &Mode) -> &'static str {
    match mode {
        Mode::StaysQueued => "queued",
        Mode::RunsForever => "in_progress",
        _ if run.queued > 0 => "queued",
        _ if run.running > 0 => "in_progress",
        _ => "completed",
    }
}

/// Reads a run's status, advancing it one poll.
fn advance(run: &mut FakeRun, mode: &Mode) -> &'static str {
    match mode {
        Mode::StaysQueued => "queued",
        Mode::RunsForever => "in_progress",
        _ if run.queued > 0 => {
            run.queued -= 1;
            "queued"
        }
        _ if run.running > 0 => {
            run.running -= 1;
            "in_progress"
        }
        _ => "completed",
    }
}

fn run_json(run: &FakeRun, status: &str) -> Value {
    json!({
        "id": run.id,
        "name": "graft post-land",
        "display_title": format!("graft post-land {}", run.request_id),
        "head_branch": run.branch,
        "status": status,
        "conclusion": match status {
            "completed" if run.cancelled => Value::from("cancelled"),
            "completed" if run.jobs.values().any(|c| c != "success") => Value::from("failure"),
            "completed" => Value::from("success"),
            _ => Value::Null,
        },
        "html_url": format!("https://github.example/runs/{}", run.id),
    })
}

async fn handle(
    State(gh): State<Arc<Gh>>,
    method: Method,
    uri: Uri,
    headers: HeaderMap,
    body: String,
) -> (StatusCode, String) {
    let auth = headers
        .get("authorization")
        .and_then(|v| v.to_str().ok())
        .unwrap_or_default()
        .to_owned();
    {
        let mut st = gh.state.lock().expect("fake");
        if auth != format!("Bearer {GH_TOKEN}") {
            st.bad_auth += 1;
        }
        if uri.to_string().contains(GH_TOKEN) || body.contains(GH_TOKEN) {
            st.leaked += 1;
        }
        if st.mode == Mode::Outage {
            return reply(
                StatusCode::SERVICE_UNAVAILABLE,
                json!({ "message": format!("unicorn: upstream down; you sent {auth}") }),
            );
        }
    }
    let path = uri.path().to_owned();
    let Some(rest) = path.strip_prefix("/repos/owner/graft/actions/") else {
        return reply(StatusCode::NOT_FOUND, json!({ "message": "Not Found" }));
    };
    let parts: Vec<&str> = rest.split('/').collect();
    match (method.as_str(), parts.as_slice()) {
        ("POST", ["workflows", "graft-post-land.yml", "dispatches"]) => dispatch(&gh, &body).await,
        ("GET", ["workflows", "graft-post-land.yml", "runs"]) => {
            let q = query(&uri);
            if let Some(wanted) = q.get("status") {
                // A count of the workflow's runs on every ref; each is
                // observed, as time passing would.
                let (n, completed) = {
                    let mut st = gh.state.lock().expect("fake");
                    let mode = st.mode.clone();
                    let mut n = 0;
                    let mut completed = Vec::new();
                    for run in st.runs.iter_mut() {
                        let status = advance(run, &mode);
                        if status == wanted {
                            n += 1;
                        }
                        if status == "completed" && !run.attested {
                            run.attested = true;
                            completed.push(run.clone());
                        }
                    }
                    (n, completed)
                };
                attest_job(&gh, completed).await;
                return reply(
                    StatusCode::OK,
                    json!({ "total_count": n, "workflow_runs": [] }),
                );
            }
            let completed = {
                let mut st = gh.state.lock().expect("fake");
                let mode = st.mode.clone();
                let mut list = Vec::new();
                let mut completed = Vec::new();
                for run in st.runs.iter_mut() {
                    if Some(&run.branch) != q.get("branch") {
                        continue;
                    }
                    let status = advance(run, &mode);
                    if status == "completed" && !run.attested {
                        run.attested = true;
                        completed.push(run.clone());
                    }
                    list.push(run_json(run, status));
                }
                (list, completed)
            };
            attest_job(&gh, completed.1).await;
            reply(
                StatusCode::OK,
                json!({ "total_count": completed.0.len(), "workflow_runs": completed.0 }),
            )
        }
        ("GET", ["runs", id]) => {
            let id: u64 = id.parse().expect("id");
            let (json, completed) = {
                let mut st = gh.state.lock().expect("fake");
                let mode = st.mode.clone();
                let Some(run) = st.runs.iter_mut().find(|r| r.id == id) else {
                    return reply(StatusCode::NOT_FOUND, json!({ "message": "Not Found" }));
                };
                let status = advance(run, &mode);
                let mut completed = Vec::new();
                if status == "completed" && !run.attested {
                    run.attested = true;
                    completed.push(run.clone());
                }
                (run_json(run, status), completed)
            };
            attest_job(&gh, completed).await;
            reply(StatusCode::OK, json)
        }
        ("GET", ["runs", id, "jobs"]) => {
            let id: u64 = id.parse().expect("id");
            let st = gh.state.lock().expect("fake");
            let Some(run) = st.runs.iter().find(|r| r.id == id) else {
                return reply(StatusCode::NOT_FOUND, json!({ "message": "Not Found" }));
            };
            let mut jobs: Vec<Value> = run
                .jobs
                .iter()
                .map(|(name, conclusion)| {
                    json!({
                        "name": format!("check / {name}"),
                        "status": "completed",
                        "conclusion": conclusion,
                        "html_url": job_url(id, name),
                    })
                })
                .collect();
            jobs.push(json!({
                "name": "check / attest",
                "status": "completed",
                "conclusion": "success",
                "html_url": job_url(id, "attest"),
            }));
            reply(
                StatusCode::OK,
                json!({ "total_count": jobs.len(), "jobs": jobs }),
            )
        }
        ("POST", ["runs", id, "cancel"]) => {
            let id: u64 = id.parse().expect("id");
            gh.state.lock().expect("fake").cancels.push(id);
            reply(StatusCode::ACCEPTED, json!({}))
        }
        _ => reply(StatusCode::NOT_FOUND, json!({ "message": "Not Found" })),
    }
}

fn job_url(run: u64, job: &str) -> String {
    format!("https://github.example/runs/{run}/job/{job}")
}

async fn dispatch(gh: &Gh, body: &str) -> (StatusCode, String) {
    let v: Value = serde_json::from_str(body).expect("dispatch body");
    let branch = v["ref"].as_str().expect("ref").to_owned();
    let inputs = v["inputs"].clone();
    let changeset = inputs["changeset"].as_str().expect("changeset").to_owned();
    let mode = gh.state.lock().expect("fake").mode.clone();
    if let Mode::Refuse(message) = &mode {
        return reply(
            StatusCode::UNPROCESSABLE_ENTITY,
            json!({ "message": message }),
        );
    }
    // `test` fails on a snapshot holding `broken`.
    let broken = match &gh.engine {
        Some(engine) => {
            let at = ChangesetId::parse(&changeset).expect("changeset id");
            engine
                .stat(
                    &admin(),
                    PathAt {
                        repo: REPO.to_owned(),
                        at,
                        path: path("broken"),
                    },
                )
                .await
                .is_ok()
        }
        None => false,
    };
    let mut st = gh.state.lock().expect("fake");
    st.dispatches.push((branch.clone(), inputs.clone()));
    if mode == Mode::NeverStarts {
        return (StatusCode::NO_CONTENT, String::new());
    }
    let jobs = ["fmt", "clippy", "test"]
        .iter()
        .map(|job| {
            let red = st.red.contains(*job) || (*job == "test" && broken);
            let conclusion = if st.cancel_runs || st.cancel_jobs.contains(*job) {
                "cancelled"
            } else if red {
                "failure"
            } else {
                "success"
            };
            ((*job).to_owned(), conclusion.to_owned())
        })
        .collect();
    let id = 1000 + st.runs.len() as u64;
    let cancelled = st.cancel_runs;
    st.runs.push(FakeRun {
        id,
        branch,
        changeset,
        request_id: inputs["request_id"]
            .as_str()
            .expect("request_id")
            .to_owned(),
        queued: 1,
        running: 1,
        jobs,
        cancelled,
        attested: false,
        foreign: false,
    });
    st.peak_in_flight = st.peak_in_flight.max(st.in_flight());
    (StatusCode::NO_CONTENT, String::new())
}

/// The workflow's `attest` job: a pass for each job that succeeded.
async fn attest_job(gh: &Gh, completed: Vec<FakeRun>) {
    let Some(engine) = &gh.engine else {
        return;
    };
    for run in completed.into_iter().filter(|r| !r.foreign) {
        let at = ChangesetId::parse(&run.changeset).expect("changeset id");
        for (job, conclusion) in &run.jobs {
            if conclusion != "success" {
                continue;
            }
            let check = format!("ci/{job}");
            engine
                .attest_v2(
                    &checker_cx("graft-post-land-check"),
                    AttestV2 {
                        repo: REPO.to_owned(),
                        changeset: at,
                        kind: AttestKind::Check,
                        check: check.clone(),
                        status: AttestStatus::Pass,
                        by: None,
                        url: job_url(run.id, job),
                    },
                )
                .await
                .expect("workflow attest");
            gh.state
                .lock()
                .expect("fake")
                .attested
                .push((run.changeset.clone(), check));
        }
    }
}

// ---- the runner alone -----------------------------------------------------

fn cs(n: u8) -> ChangesetId {
    ChangesetId(ObjectId([n; 32]))
}

fn check(name: &str) -> Check {
    Check {
        name: name.to_owned(),
        argv: vec![name.rsplit('/').next().expect("job").to_owned()],
    }
}

async fn run_one(
    runner: &mut GitHubRunner,
    at: ChangesetId,
    name: &str,
    attempt: u32,
) -> Result<graft_attester::post_land::Run, Error> {
    Runner::<()>::run(runner, &(), at, &check(name), attempt).await
}

/// Done 1: one dispatch per (snapshot, attempt), on `<prefix><changeset>`
/// with the three inputs; its jobs map to the checks, the job's page is
/// the link; a retry is a new dispatch; a restart finds the run by its
/// request id and dispatches nothing.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn one_dispatch_per_snapshot_and_attempt_maps_jobs_to_checks() {
    let gh = FakeGitHub::start(None).await;
    gh.state().red.insert("clippy".to_owned());
    let at = cs(7);
    let mut runner = gh.runner();

    let mut got = Vec::new();
    for name in CHECKS {
        got.push(run_one(&mut runner, at, name, 1).await.expect("run"));
    }
    {
        let st = gh.state();
        assert_eq!(st.dispatches.len(), 1, "one dispatch for three checks");
        let (branch, inputs) = &st.dispatches[0];
        assert_eq!(branch, &format!("{PREFIX}{at}"));
        assert_eq!(
            inputs,
            &json!({
                "changeset": at.to_string(),
                "attempt": "1",
                "request_id": request_id(at, 1),
            })
        );
    }
    let run = 1000;
    assert_eq!(got[0].outcome, Outcome::Passed);
    assert_eq!(got[0].url, job_url(run, "fmt"));
    assert!(!got[1].outcome.passed(), "clippy is red: {:?}", got[1]);
    assert_eq!(got[1].url, job_url(run, "clippy"));
    assert_eq!(got[2].outcome, Outcome::Passed);
    assert_eq!(got[2].url, job_url(run, "test"));

    // A retry of clippy is a second run.
    let retry = run_one(&mut runner, at, CLIPPY, 2).await.expect("retry");
    assert_eq!(retry.url, job_url(run + 1, "clippy"));
    assert_eq!(gh.state().dispatches.len(), 2);
    assert_eq!(gh.state().dispatches[1].1["attempt"], "2");
    assert!(Runner::<()>::attests_passes(&runner));

    // Restarted: both runs are found again; nothing is dispatched.
    let mut runner = gh.runner();
    let again = run_one(&mut runner, at, FMT, 1).await.expect("run");
    assert_eq!(again.url, job_url(run, "fmt"));
    let again = run_one(&mut runner, at, CLIPPY, 2).await.expect("run");
    assert_eq!(again.url, job_url(run + 1, "clippy"));
    assert_eq!(
        gh.state().dispatches.len(),
        2,
        "no dispatch after a restart"
    );
    assert_eq!(runner.dispatched(), 0);
    gh.assert_token_only_in_header();
}

/// Done 1: a job with an explicit name, and one missing from the run.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_mapped_job_is_read_and_a_missing_job_fails() {
    let gh = FakeGitHub::start(None).await;
    let mut cfg = gh.config();
    cfg.jobs.insert("ci/unit".to_owned(), "test".to_owned());
    let mut runner = gh.runner_with(cfg);
    let at = cs(8);
    let unit = run_one(&mut runner, at, "ci/unit", 1).await.expect("run");
    assert_eq!(unit.outcome, Outcome::Passed);
    assert_eq!(unit.url, job_url(1000, "test"));
    let missing = run_one(&mut runner, at, "ci/miri", 1).await.expect("run");
    assert!(
        matches!(missing.outcome, Outcome::NotStarted(_)),
        "{missing:?}"
    );
    assert_eq!(missing.url, "https://github.example/runs/1000");
    assert_eq!(gh.state().dispatches.len(), 1);
}

/// Done 2: an outage, a dispatch that never starts, a run that stays
/// queued, and a ref the mirror has not pushed yet run nothing: `Err`.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_outage_or_a_run_that_never_starts_is_an_error() {
    let gh = FakeGitHub::start(None).await;
    let mut cfg = gh.config();
    cfg.start_timeout = Duration::ZERO;
    for (i, mode) in [
        Mode::Outage,
        Mode::NeverStarts,
        Mode::StaysQueued,
        Mode::Refuse("No ref found for: graft/check/integration/x".to_owned()),
    ]
    .into_iter()
    .enumerate()
    {
        gh.set_mode(mode.clone());
        let mut runner = gh.runner_with(cfg.clone());
        let got = run_one(&mut runner, cs(20 + i as u8), FMT, 1).await;
        assert!(matches!(got, Err(Error::Forge(_))), "{mode:?} gave {got:?}");
    }
    gh.assert_token_only_in_header();
}

/// Done 2: a red job, a run past its timeout (cancelled), and a 422 for a
/// workflow that is not on the ref are failed runs.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_red_job_a_timeout_and_a_workflow_not_on_the_ref_fail() {
    let gh = FakeGitHub::start(None).await;
    gh.state().red.insert("test".to_owned());
    let mut runner = gh.runner();
    let red = run_one(&mut runner, cs(30), TEST, 1).await.expect("run");
    assert_eq!(red.outcome, Outcome::Failed(Some(1)));

    gh.set_mode(Mode::RunsForever);
    let mut cfg = gh.config();
    cfg.run_timeout = Duration::ZERO;
    let mut runner = gh.runner_with(cfg);
    let late = run_one(&mut runner, cs(31), FMT, 1).await.expect("run");
    assert_eq!(late.outcome, Outcome::TimedOut);
    assert_eq!(gh.state().cancels, vec![1001], "the late run is cancelled");

    gh.set_mode(Mode::Refuse(
        "Workflow does not have 'workflow_dispatch' trigger".to_owned(),
    ));
    let mut runner = gh.runner();
    let refused = run_one(&mut runner, cs(32), FMT, 1).await.expect("run");
    assert!(
        matches!(&refused.outcome, Outcome::NotStarted(why) if why.contains("workflow_dispatch")),
        "{refused:?}"
    );
    assert!(
        refused
            .url
            .ends_with("/actions/workflows/graft-post-land.yml")
    );
}

/// Done 4: the token is sent only as a header, and GitHub echoing it back
/// leaves no trace in the error text or in any `Debug` string.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_token_is_never_in_an_error_or_debug_string() {
    let gh = FakeGitHub::start(None).await;
    gh.set_mode(Mode::Outage);
    let mut runner = gh.runner();
    let err = run_one(&mut runner, cs(40), FMT, 1)
        .await
        .expect_err("outage");
    let shown = [
        err.to_string(),
        format!("{err:?}"),
        format!("{runner:?}"),
        format!("{:?}", GitHubToken::new(GH_TOKEN).expect("token")),
    ];
    assert!(
        shown[0].contains("503") && shown[0].contains("Bearer <redacted>"),
        "the echo is redacted, not lost: {}",
        shown[0]
    );
    for s in &shown {
        assert!(!s.contains(GH_TOKEN), "token in {s}");
        assert!(!s.contains("secret1111"), "part of the token in {s}");
    }
    gh.assert_token_only_in_header();
    assert_eq!(gh.state().bad_auth, 0);
}

/// Done 4: the token file is read read-only and left as it was; one that
/// others can read is refused without showing its contents.
#[test]
fn the_token_file_is_read_only_and_must_be_private() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("github-token");
    std::fs::write(&path, format!("{GH_TOKEN}\n")).expect("write");
    // Read-only even for its owner, as a shared secret mount is.
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o400)).expect("chmod");
    let before = std::fs::metadata(&path).expect("meta");
    let token = GitHubToken::read(&path).expect("read");
    assert!(!format!("{token:?}").contains(GH_TOKEN));
    let after = std::fs::metadata(&path).expect("meta");
    assert_eq!(
        before.modified().expect("mtime"),
        after.modified().expect("mtime")
    );
    assert_eq!(after.permissions().mode() & 0o777, 0o400);
    assert_eq!(
        std::fs::read_to_string(&path).expect("read"),
        format!("{GH_TOKEN}\n")
    );

    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).expect("chmod");
    let err = read_secret(&path).expect_err("group-readable");
    assert!(err.contains("0600"), "{err}");
    assert!(!err.contains(GH_TOKEN), "{err}");
}

// ---- the post-land checker on GitHub --------------------------------------

/// The engine, answering every call as the checker's principal.
struct AsChecker {
    engine: Arc<Eng>,
}

fn checker_cx(request_id: &str) -> CallContext {
    CallContext {
        principal: Principal {
            subject: "graft-attester-post-land".to_owned(),
            kind: PrincipalKind::Service,
            session: None,
            work_item: None,
            scopes: vec![
                format!("repo:{REPO}:write"),
                format!("repo:{REPO}:attest"),
                format!("repo:{REPO}:land:{LINE}"),
            ],
        },
        request_id: request_id.to_owned(),
        deadline_ms: None,
    }
}

macro_rules! delegate {
    ($($method:ident($req:ty) -> $out:ty;)*) => {
        $(
            async fn $method(&self, cx: &CallContext, req: $req) -> ApiResult<$out> {
                self.engine.$method(&checker_cx(&cx.request_id), req).await
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
    land(Land) -> Landed;
    attest(Attest) -> ();
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
        request_id: "github-runner".to_owned(),
        deadline_ms: None,
    }
}

fn path(s: &str) -> RepoPath {
    RepoPath::new(s).expect("path")
}

/// The engine, its line checked post-land with the three checks and
/// `retries`, holding one root changeset.
async fn repo(retries: u32) -> Arc<Eng> {
    let mut line = LineConfig::direct(LINE);
    line.checks = LineChecks::PostLand {
        required: CHECKS.iter().map(|c| (*c).to_owned()).collect(),
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

/// `n` lands, the `bad`-th (from 1; 0 for none) planting `broken`.
async fn lands(engine: &Eng, n: usize, bad: usize) -> Vec<ChangesetId> {
    let mut out = Vec::new();
    for i in 1..=n {
        let name = format!("f{i}.txt");
        let mut files = vec![(name.as_str(), "ok\n")];
        if i == bad {
            files.push(("broken", "yes\n"));
        }
        let head = commit(engine, Some(tip(engine).await), &files).await;
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
            Landed::Landed { landed, .. } => out.push(landed),
            other => panic!("not landed: {other:?}"),
        }
    }
    out
}

async fn entries(engine: &Eng, kind: &str) -> Vec<Value> {
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
            out.push(serde_json::from_str(&e.body).expect("json"));
        }
        if page.entries.is_empty() || page.next_lsn <= from {
            return out;
        }
        from = page.next_lsn;
    }
}

/// Check reports per `(land, check)`: the pass/fail of each, in order.
async fn attested(engine: &Eng) -> BTreeMap<(String, String), Vec<bool>> {
    let mut out: BTreeMap<(String, String), Vec<bool>> = BTreeMap::new();
    for v in entries(engine, "Attested").await {
        let passed = match v.get("status").and_then(|s| s.as_str()) {
            Some(s) => s == "pass",
            None => v["passed"].as_bool().expect("passed"),
        };
        out.entry((
            v["changeset"].as_str().expect("changeset").to_owned(),
            v["check"].as_str().expect("check").to_owned(),
        ))
        .or_default()
        .push(passed);
    }
    out
}

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

struct Rig {
    engine: Arc<Eng>,
    gh: FakeGitHub,
    server: TestServer,
    dir: tempfile::TempDir,
    retries: u32,
}

impl Rig {
    async fn new(retries: u32) -> Rig {
        let engine = repo(retries).await;
        let gh = FakeGitHub::start(Some(engine.clone())).await;
        let server = TestServer::tcp(Arc::new(AsChecker {
            engine: engine.clone(),
        }))
        .await;
        Rig {
            engine,
            gh,
            server,
            dir: tempfile::tempdir().expect("tempdir"),
            retries,
        }
    }

    async fn checker(&self, cfg: GitHubConfig) -> PostLand<Remote, GitHubRunner> {
        let remote = Remote::connect(Endpoint::Server(self.server.url().to_owned()))
            .await
            .expect("connect")
            .with_token("not-a-real-token");
        let mut config = Config::new(REPO, LINE, self.dir.path());
        config.checks = CHECKS.iter().map(|c| check(c)).collect();
        config.state_file = Some(self.dir.path().join("state").join("post-land-lsn"));
        config.attest_tries = 1;
        config.retry_delay = Duration::from_millis(1);
        config.wait_ms = 0;
        PostLand::new(remote, self.gh.runner_with(cfg), config, self.retries).expect("checker")
    }
}

async fn drain(c: &mut PostLand<Remote, GitHubRunner>) -> Vec<PostLandReport> {
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

/// Done 3: the workflow attests its own passes. The checker posts no
/// second pass for any (land, check), the lands it skipped ahead over get
/// theirs from the checker, and every land reads good.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_workflows_passes_count_and_are_never_posted_twice() {
    let rig = Rig::new(0).await;
    let landed = lands(&rig.engine, 3, 0).await;
    let mut c = rig.checker(rig.gh.config()).await;
    let reports = drain(&mut c).await;
    assert_eq!(
        reports[0],
        PostLandReport::Good {
            at: landed[2],
            lands: landed.clone()
        }
    );
    assert_eq!(rig.gh.state().dispatches.len(), 1, "one run settles all");
    let top = landed[2].to_string();
    let by_workflow: BTreeSet<(String, String)> = rig.gh.state().attested.iter().cloned().collect();
    assert_eq!(
        by_workflow,
        CHECKS
            .iter()
            .map(|c| (top.clone(), (*c).to_owned()))
            .collect::<BTreeSet<_>>(),
        "the workflow attested the snapshot it ran"
    );
    let got = attested(&rig.engine).await;
    for land in &landed {
        for name in CHECKS {
            assert_eq!(
                got.get(&(land.to_string(), name.to_owned())),
                Some(&vec![true]),
                "one pass of {name} on {land}"
            );
        }
    }
    let states = land_states(&rig.engine).await;
    assert!(
        landed
            .iter()
            .all(|id| states.get(id) == Some(&LandCheckState::Good)),
        "{states:?}"
    );
    rig.gh.assert_token_only_in_header();
}

/// Done 1-3 end to end: the checker bisects over GitHub runs to the bad
/// land, posts its failure, and reverts it once; every pass is posted once.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_bad_land_found_on_github_is_reverted_once() {
    let rig = Rig::new(0).await;
    let landed = lands(&rig.engine, 4, 3).await;
    let mut c = rig.checker(rig.gh.config()).await;
    let reports = drain(&mut c).await;
    assert!(
        reports.contains(&PostLandReport::Bad {
            land: landed[2],
            check: TEST.to_owned(),
            good: landed[..2].to_vec(),
        }),
        "{reports:?}"
    );
    let reverted = entries(&rig.engine, "LandReverted").await;
    assert_eq!(reverted.len(), 1, "{reverted:?}");
    assert_eq!(reverted[0]["land"], landed[2].to_string());
    let got = attested(&rig.engine).await;
    for (key, reports) in &got {
        let fails = reports.iter().filter(|p| !**p).count();
        let passes = reports.len() - fails;
        assert!(passes <= 1, "{key:?} passed twice: {reports:?}");
        assert!(
            passes == 0 || fails == 0,
            "{key:?} has a pass and a fail: {reports:?}"
        );
    }
    assert_eq!(
        got.get(&(landed[2].to_string(), TEST.to_owned())),
        Some(&vec![false])
    );
    let states = land_states(&rig.engine).await;
    assert_eq!(states.get(&landed[2]), Some(&LandCheckState::Reverted));
    for land in [landed[0], landed[1], landed[3]] {
        assert_eq!(states.get(&land), Some(&LandCheckState::Good), "{land}");
    }
}

/// Done 2: while GitHub is down, or its runs never start, the checker
/// posts nothing and reverts nothing, even with a bad land on the line.
/// Once GitHub is back, the bad land is found and reverted, once.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_github_outage_never_gives_a_bad_verdict_or_a_revert() {
    let rig = Rig::new(1).await;
    let landed = lands(&rig.engine, 3, 2).await;
    let mut cfg = rig.gh.config();
    cfg.start_timeout = Duration::ZERO;
    let mut c = rig.checker(cfg).await;
    for mode in [Mode::Outage, Mode::NeverStarts, Mode::StaysQueued] {
        rig.gh.set_mode(mode.clone());
        for _ in 0..3 {
            let step = c.step(0).await;
            assert!(matches!(step, Err(Error::Forge(_))), "{mode:?}: {step:?}");
        }
        assert!(
            attested(&rig.engine)
                .await
                .values()
                .flatten()
                .all(|passed| *passed),
            "{mode:?}: a failure was attested"
        );
        assert!(
            entries(&rig.engine, "LandReverted").await.is_empty(),
            "{mode:?}: reverted"
        );
        let states = land_states(&rig.engine).await;
        assert!(
            landed
                .iter()
                .all(|id| states.get(id) != Some(&LandCheckState::Bad)),
            "{mode:?}: {states:?}"
        );
    }
    assert!(
        rig.gh.state().dispatches.len() >= 2,
        "the faults were reached: {:?}",
        rig.gh.state().dispatches.len()
    );

    // Back up: the runs queued during the fault finish too.
    rig.gh.set_mode(Mode::Normal);
    let mut c = rig.checker(rig.gh.config()).await;
    drain(&mut c).await;
    let reverted = entries(&rig.engine, "LandReverted").await;
    assert_eq!(reverted.len(), 1, "{reverted:?}");
    assert_eq!(reverted[0]["land"], landed[1].to_string());
    let states = land_states(&rig.engine).await;
    assert_eq!(states.get(&landed[0]), Some(&LandCheckState::Good));
    assert_eq!(states.get(&landed[1]), Some(&LandCheckState::Reverted));
    rig.gh.assert_token_only_in_header();
}

// ---- the in-flight cap and cancelled runs (WI-2626) -----------------------

/// The cap: with two runs of another checker in flight and a cap of 2,
/// the runner waits for one to finish before it dispatches; with the cap
/// held past the start timeout it dispatches nothing and gives `Err`.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_dispatch_waits_under_the_in_flight_cap() {
    let gh = FakeGitHub::start(None).await;
    gh.state().foreign_run(3);
    gh.state().foreign_run(5);
    assert_eq!(gh.state().in_flight(), 2);
    let mut cfg = gh.config();
    cfg.max_in_flight = 2;
    let mut runner = gh.runner_with(cfg.clone());
    let got = run_one(&mut runner, cs(50), FMT, 1).await.expect("run");
    assert_eq!(got.outcome, Outcome::Passed);
    assert_eq!(gh.state().dispatches.len(), 1);
    assert!(
        gh.state().peak_in_flight <= 2,
        "{} in flight",
        gh.state().peak_in_flight
    );

    // Held for good: no dispatch, no verdict.
    gh.state().foreign_run(u32::MAX);
    gh.state().foreign_run(u32::MAX);
    cfg.start_timeout = Duration::ZERO;
    let mut runner = gh.runner_with(cfg);
    let got = run_one(&mut runner, cs(51), FMT, 1).await;
    assert!(matches!(got, Err(Error::Forge(_))), "{got:?}");
    assert_eq!(
        gh.state().dispatches.len(),
        1,
        "nothing dispatched over the cap"
    );
}

/// The cap end to end: bisecting eight lands with a cap of 1, while
/// another checker's run is in flight at the start, never has more than
/// one run queued or in progress, and still finds and reverts the bad land.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_in_flight_cap_is_never_exceeded() {
    let rig = Rig::new(0).await;
    let landed = lands(&rig.engine, 8, 5).await;
    // Outlasts one count of the runs in flight (five reads), so the first
    // dispatch has to wait for it.
    rig.gh.state().foreign_run(12);
    let mut cfg = rig.gh.config();
    cfg.max_in_flight = 1;
    let mut c = rig.checker(cfg).await;
    drain(&mut c).await;
    let (dispatched, peak) = {
        let st = rig.gh.state();
        (st.dispatches.len(), st.peak_in_flight)
    };
    assert!(dispatched >= 3, "bisected: {dispatched}");
    assert_eq!(peak, 1, "the cap was exceeded");
    let reverted = entries(&rig.engine, "LandReverted").await;
    assert_eq!(reverted.len(), 1, "{reverted:?}");
    assert_eq!(reverted[0]["land"], landed[4].to_string());
}

/// A cancelled job, or a cancelled run, is no verdict: `Err`. The next
/// call, and a restarted runner, dispatch a fresh run and read that.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_cancelled_run_is_an_error_and_runs_again() {
    let gh = FakeGitHub::start(None).await;
    gh.state().cancel_jobs.insert("test".to_owned());
    let mut runner = gh.runner();
    let got = run_one(&mut runner, cs(60), TEST, 1).await;
    assert!(matches!(got, Err(Error::Forge(_))), "{got:?}");
    gh.state().cancel_jobs.clear();
    let got = run_one(&mut runner, cs(60), TEST, 1).await.expect("run");
    assert_eq!(got.outcome, Outcome::Passed);
    assert_eq!(gh.state().dispatches.len(), 2, "a fresh run");

    gh.state().cancel_runs = true;
    let got = run_one(&mut runner, cs(61), FMT, 1).await;
    assert!(matches!(got, Err(Error::Forge(_))), "{got:?}");
    gh.state().cancel_runs = false;
    // Restarted: the cancelled run is not read as a result either.
    let mut runner = gh.runner();
    let got = run_one(&mut runner, cs(61), FMT, 1).await.expect("run");
    assert_eq!(got.outcome, Outcome::Passed);
    assert_eq!(gh.state().dispatches.len(), 4);
}

/// A cancelled run never fails a land: with no retries, a cancelled `test`
/// posts no failure and reverts nothing; once runs complete, every land is
/// good.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_cancelled_run_never_gives_a_bad_verdict_or_a_revert() {
    let rig = Rig::new(0).await;
    let landed = lands(&rig.engine, 2, 0).await;
    rig.gh.state().cancel_jobs.insert("test".to_owned());
    let mut c = rig.checker(rig.gh.config()).await;
    for _ in 0..3 {
        let step = c.step(0).await;
        assert!(matches!(step, Err(Error::Forge(_))), "{step:?}");
    }
    assert!(
        rig.gh.state().dispatches.len() >= 3,
        "the fault was reached"
    );
    assert!(
        attested(&rig.engine)
            .await
            .values()
            .flatten()
            .all(|passed| *passed),
        "a failure was attested"
    );
    assert!(entries(&rig.engine, "LandReverted").await.is_empty());

    rig.gh.state().cancel_jobs.clear();
    drain(&mut c).await;
    assert!(entries(&rig.engine, "LandReverted").await.is_empty());
    let states = land_states(&rig.engine).await;
    assert!(
        landed
            .iter()
            .all(|id| states.get(id) == Some(&LandCheckState::Good)),
        "{states:?}"
    );
}
