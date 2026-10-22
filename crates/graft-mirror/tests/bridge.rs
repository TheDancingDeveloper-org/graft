//! The PR bridge against a local bare upstream, an in-process engine, and a
//! fake GitHub API (wiremock). Nothing touches the network or a real GitHub.
//!
//! Covers the full lifecycle (land → PR → merge → follow → landed), a closed
//! PR (`Rejected` with its URL), a restart mid-lifecycle, duplicate
//! deliveries, a branch that diverged, a force-pushed upstream, two
//! authorities claiming one line, a hostile title, and rate-limit and 5xx
//! answers.

use std::cell::Cell;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use graft_engine::{Engine, EngineConfig};
use graft_git::GitSha;
use graft_git::exporter::{SESSION_KEY, WORK_ITEM_KEY};
use graft_git::map::MirrorMap;
use graft_mirror::bridge::{Bridge, BridgeConfig, BridgeState};
use graft_mirror::credentials::{SecretToken, StaticToken};
use graft_mirror::follow::{FollowConfig, FollowLine, Follower, LineOutcome, POLL_INTERVAL};
use graft_mirror::github::{ForgeError, HttpForge, RetryPolicy};
use graft_mirror::{Mirror, MirrorDb, MirrorError, current_ref};
use graft_model::{
    Author, AuthorKind, Change, ChangesetId, EntryKind, Metadata, ObjectId, ObjectKind, RepoPath,
    Timestamp,
};
use graft_proto::api::{AdminApi, ObjectApi, RepoApi, WorkspaceApi};
use graft_proto::types::{CreateChangeset, DraftState, GetChangesets, OpenWorkspace, PutObjects};
use graft_proto::{CallContext, Frame, Principal, PrincipalKind};
use graft_store::memory::MemoryStore;
use graft_testkit::clock::FakeClock;
use serde_json::{Value, json};
use wiremock::matchers::{method, path, path_regex};
use wiremock::{Mock, MockServer, Request, ResponseTemplate};

const REPO: &str = "r";
const OWNER: &str = "indexarr";
const GH_REPO: &str = "rusttorrent";
const TOKEN: &str = "fake-token-for-tests-only";
const WORK_ITEM: &str = "WI-2200";
const SESSION: &str = "ses_01M5HQPEYMW5JZMZ774J7DC2CS";
/// The workspace that holds the drafts (a draft without a holder is abandoned).
const WS: &str = "ws1";

type Api = Engine<MemoryStore, MemoryStore>;

fn engine() -> Api {
    let store = Arc::new(MemoryStore::new());
    Engine::new(
        store.clone(),
        store,
        Arc::new(FakeClock::at(1_000_000)),
        EngineConfig::default(),
    )
}

fn cx() -> CallContext {
    CallContext {
        principal: Principal {
            subject: "graft-mirror".to_owned(),
            kind: PrincipalKind::Service,
            session: None,
            work_item: None,
            scopes: vec![format!("repo:{REPO}:admin")],
        },
        request_id: "test".to_owned(),
        deadline_ms: None,
    }
}

fn clock() -> i64 {
    1_700_000_000_000
}

fn retry() -> RetryPolicy {
    RetryPolicy {
        attempts: 3,
        base: Duration::from_millis(1),
        max_wait: Duration::from_millis(5),
    }
}

fn git(dir: &Path, args: &[&str]) -> String {
    let out = Command::new("git")
        .current_dir(dir)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_AUTHOR_NAME", "A U Thor")
        .env("GIT_AUTHOR_EMAIL", "author@example.com")
        .env("GIT_COMMITTER_NAME", "C O Mitter")
        .env("GIT_COMMITTER_EMAIL", "committer@example.com")
        .env("GIT_AUTHOR_DATE", "1700000000 +0000")
        .env("GIT_COMMITTER_DATE", "1700000000 +0000")
        .args([
            "-c",
            "commit.gpgsign=false",
            "-c",
            "init.defaultBranch=main",
        ])
        .args(args)
        .stdin(Stdio::null())
        .output()
        .expect("git runs");
    assert!(
        out.status.success(),
        "git {args:?} failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8(out.stdout).unwrap().trim().to_owned()
}

/// The commit `branch` names in `upstream`, or zeros when there is none.
fn branch_sha(upstream: &Path, branch: &str) -> String {
    let out = Command::new("git")
        .current_dir(upstream)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .args([
            "rev-parse",
            "--verify",
            "-q",
            &format!("refs/heads/{branch}"),
        ])
        .output()
        .expect("git runs");
    if out.status.success() {
        String::from_utf8(out.stdout).unwrap().trim().to_owned()
    } else {
        "0".repeat(40)
    }
}

/// The short branch segment the bridge derives from a head.
fn short(head: ChangesetId) -> String {
    head.to_string()[1..13].to_owned()
}

/// A bare "GitHub", a work tree that pushes to it, the mirror's directory
/// and DB path, and an engine.
struct World {
    _tmp: tempfile::TempDir,
    upstream: PathBuf,
    work: PathBuf,
    mirror_dir: PathBuf,
    db_path: PathBuf,
    api: Api,
    n: Cell<u32>,
}

impl World {
    async fn new() -> World {
        let tmp = tempfile::tempdir().unwrap();
        let upstream = tmp.path().join("github.git");
        let work = tmp.path().join("work");
        std::fs::create_dir_all(&upstream).unwrap();
        std::fs::create_dir_all(&work).unwrap();
        git(&upstream, &["init", "-q", "--bare"]);
        git(&work, &["init", "-q", "-b", "main"]);
        git(
            &work,
            &["remote", "add", "origin", upstream.to_str().unwrap()],
        );
        let api = engine();
        api.create_repo(&cx(), REPO.to_owned()).await.unwrap();
        World {
            mirror_dir: tmp.path().join("mirror.git"),
            db_path: tmp.path().join("mirror.sqlite"),
            _tmp: tmp,
            upstream,
            work,
            api,
            n: Cell::new(0),
        }
    }

    /// Commits a file of its own with `message`, and returns the sha.
    fn commit(&self, message: &str) -> GitSha {
        self.n.set(self.n.get() + 1);
        let n = self.n.get();
        std::fs::write(self.work.join(format!("f{n}.txt")), format!("file {n}\n")).unwrap();
        git(&self.work, &["add", "-A"]);
        git(&self.work, &["commit", "-q", "-m", message]);
        GitSha::from_hex(&git(&self.work, &["rev-parse", "HEAD"])).unwrap()
    }

    fn push_main(&self) {
        git(&self.work, &["push", "-q", "origin", "main"]);
    }

    /// Makes `main` upstream an unrelated root, by force.
    fn force_push_main(&self) {
        git(&self.work, &["checkout", "-q", "--orphan", "forced"]);
        git(
            &self.work,
            &["commit", "-q", "--allow-empty", "-m", "forced root"],
        );
        git(
            &self.work,
            &["push", "-q", "--force", "origin", "forced:main"],
        );
    }

    fn open_db(&self) -> MirrorDb {
        MirrorDb::open(&self.db_path).unwrap()
    }

    fn mirror(&self) -> Mirror {
        Mirror::new(&self.mirror_dir, self.upstream.to_str().unwrap())
    }

    /// One follow poll of `main`.
    async fn follow(&self, db: &MirrorDb) -> LineOutcome {
        let config = FollowConfig {
            repo: REPO.to_owned(),
            lines: vec![FollowLine {
                branch: "main".to_owned(),
                line: "main".to_owned(),
            }],
            allow_lossy: Vec::new(),
            batch: 16,
            interval: POLL_INTERVAL,
        };
        let mut f = Follower::new(&self.api, cx(), db, self.mirror(), config).with_clock(clock);
        let mut out = f.poll_once().await.unwrap();
        out.pop().unwrap().1
    }

    /// The tip of graft's `main`.
    async fn tip(&self) -> Option<ChangesetId> {
        current_ref(&self.api, &cx(), REPO, "main").await.unwrap()
    }

    fn bridge<'a>(
        &'a self,
        db: &'a MirrorDb,
        forge: &'a HttpForge<StaticToken>,
    ) -> Bridge<'a, Api, HttpForge<StaticToken>> {
        let config = BridgeConfig {
            repo: REPO.to_owned(),
            line: "main".to_owned(),
            base_branch: "main".to_owned(),
        };
        Bridge::new(&self.api, cx(), db, self.mirror(), forge, config)
            .unwrap()
            .with_clock(clock)
    }
}

/// A draft on top of `parent`: one file, with the work item and session.
async fn draft(
    api: &Api,
    parent: ChangesetId,
    file: &str,
    body: &[u8],
    message: &str,
) -> ChangesetId {
    let blob = ObjectId::hash(ObjectKind::Blob, body);
    api.put_objects(
        &cx(),
        PutObjects {
            repo: REPO.to_owned(),
            blobs: vec![Frame::encode(ObjectKind::Blob, body)],
        },
    )
    .await
    .unwrap();
    let mut metadata = Metadata::new();
    metadata.insert(WORK_ITEM_KEY.to_owned(), WORK_ITEM.to_owned());
    metadata.insert(SESSION_KEY.to_owned(), SESSION.to_owned());
    let created = api
        .create_changeset(
            &cx(),
            CreateChangeset {
                repo: REPO.to_owned(),
                parents: vec![parent],
                changes: vec![Change::Put {
                    path: RepoPath::new(file).unwrap(),
                    kind: EntryKind::File,
                    id: blob,
                }],
                author: Author {
                    name: "Tess".to_owned(),
                    email: "tess@example.com".to_owned(),
                    kind: AuthorKind::Agent,
                },
                authored_at: Timestamp {
                    ms: 1_700_000_000_000,
                    offset_min: 60,
                },
                message: message.to_owned(),
                metadata,
                inline_blobs: Vec::new(),
                workspace: Some(WS.to_owned()),
            },
        )
        .await
        .unwrap();
    created.id
}

async fn state_of(api: &Api, id: ChangesetId) -> DraftState {
    let found = api
        .get_changesets(
            &cx(),
            GetChangesets {
                repo: REPO.to_owned(),
                ids: vec![id],
            },
        )
        .await
        .unwrap();
    found.found[0].state
}

/// One pull request in the fake GitHub.
#[derive(Clone, Debug)]
struct FakePr {
    number: u64,
    branch: String,
    open: bool,
    merged: Option<String>,
    node: String,
}

/// A fake GitHub API over the upstream, for one repository.
struct FakeGitHub {
    server: MockServer,
    upstream: PathBuf,
    prs: Arc<Mutex<Vec<FakePr>>>,
    /// The bodies of every `POST /pulls`.
    creates: Arc<Mutex<Vec<Value>>>,
    /// Successful merge-queue entries, by PR number.
    enqueued: Arc<Mutex<Vec<u64>>>,
    /// A failing check run on every head, when set: (name, url).
    failing_check: Arc<Mutex<Option<(String, String)>>>,
}

fn pr_json(pr: &FakePr, upstream: &Path) -> Value {
    json!({
        "number": pr.number,
        "html_url": format!("https://github.com/{OWNER}/{GH_REPO}/pull/{}", pr.number),
        "node_id": pr.node,
        "state": if pr.open { "open" } else { "closed" },
        "merged": pr.merged.is_some(),
        "merge_commit_sha": pr.merged,
        "head": { "sha": branch_sha(upstream, &pr.branch) },
    })
}

impl FakeGitHub {
    async fn start(upstream: &Path) -> FakeGitHub {
        let gh = FakeGitHub {
            server: MockServer::start().await,
            upstream: upstream.to_owned(),
            prs: Arc::new(Mutex::new(Vec::new())),
            creates: Arc::new(Mutex::new(Vec::new())),
            enqueued: Arc::new(Mutex::new(Vec::new())),
            failing_check: Arc::new(Mutex::new(None)),
        };
        gh.mount_defaults().await;
        gh
    }

    fn uri(&self) -> String {
        self.server.uri()
    }

    fn forge(&self) -> HttpForge<StaticToken> {
        let token = SecretToken::new(TOKEN.to_owned()).unwrap();
        HttpForge::new(
            &self.uri(),
            OWNER,
            GH_REPO,
            StaticToken::new(token),
            retry(),
        )
        .unwrap()
    }

    async fn mount_defaults(&self) {
        let upstream = self.upstream.clone();
        let prs = self.prs.clone();
        let creates = self.creates.clone();
        Mock::given(method("POST"))
            .and(path(format!("/repos/{OWNER}/{GH_REPO}/pulls")))
            .respond_with(move |req: &Request| {
                let body: Value = req.body_json().unwrap();
                creates.lock().unwrap().push(body.clone());
                let mut prs = prs.lock().unwrap();
                let number = prs.len() as u64 + 1;
                let pr = FakePr {
                    number,
                    branch: body["head"].as_str().unwrap().to_owned(),
                    open: true,
                    merged: None,
                    node: format!("PR_node_{number}"),
                };
                let json = pr_json(&pr, &upstream);
                prs.push(pr);
                ResponseTemplate::new(201).set_body_json(json)
            })
            .mount(&self.server)
            .await;

        let upstream = self.upstream.clone();
        let prs = self.prs.clone();
        Mock::given(method("GET"))
            .and(path(format!("/repos/{OWNER}/{GH_REPO}/pulls")))
            .respond_with(move |req: &Request| {
                let head = req
                    .url
                    .query_pairs()
                    .find(|(k, _)| k == "head")
                    .map(|(_, v)| v.into_owned())
                    .unwrap_or_default();
                let branch = head.split_once(':').map(|(_, b)| b).unwrap_or("");
                let found: Vec<Value> = prs
                    .lock()
                    .unwrap()
                    .iter()
                    .filter(|p| p.branch == branch)
                    .map(|p| pr_json(p, &upstream))
                    .collect();
                ResponseTemplate::new(200).set_body_json(found)
            })
            .mount(&self.server)
            .await;

        let upstream = self.upstream.clone();
        let prs = self.prs.clone();
        Mock::given(method("GET"))
            .and(path_regex(format!(r"^/repos/{OWNER}/{GH_REPO}/pulls/\d+$")))
            .respond_with(move |req: &Request| {
                let number: u64 = req.url.path().rsplit('/').next().unwrap().parse().unwrap();
                let prs = prs.lock().unwrap();
                match prs.iter().find(|p| p.number == number) {
                    Some(p) => ResponseTemplate::new(200).set_body_json(pr_json(p, &upstream)),
                    None => {
                        ResponseTemplate::new(404).set_body_json(json!({"message": "Not Found"}))
                    }
                }
            })
            .mount(&self.server)
            .await;

        let prs = self.prs.clone();
        let enqueued = self.enqueued.clone();
        Mock::given(method("POST"))
            .and(path("/graphql"))
            .respond_with(move |req: &Request| {
                let body: Value = req.body_json().unwrap();
                let id = body["variables"]["id"].as_str().unwrap().to_owned();
                let prs = prs.lock().unwrap();
                match prs.iter().find(|p| p.node == id) {
                    Some(p) => {
                        enqueued.lock().unwrap().push(p.number);
                        ResponseTemplate::new(200).set_body_json(
                            json!({"data": {"enqueuePullRequest": {"clientMutationId": null}}}),
                        )
                    }
                    None => ResponseTemplate::new(200)
                        .set_body_json(json!({"errors": [{"message": "unknown node"}]})),
                }
            })
            .mount(&self.server)
            .await;

        let failing = self.failing_check.clone();
        Mock::given(method("GET"))
            .and(path_regex(format!(
                r"^/repos/{OWNER}/{GH_REPO}/commits/[0-9a-f]{{40}}/check-runs$"
            )))
            .respond_with(move |_: &Request| {
                let runs: Vec<Value> = match failing.lock().unwrap().clone() {
                    Some((name, url)) => vec![json!({
                        "name": name, "conclusion": "failure", "html_url": url
                    })],
                    None => vec![json!({
                        "name": "build", "conclusion": "success", "html_url": "https://ci.example/ok"
                    })],
                };
                ResponseTemplate::new(200).set_body_json(json!({ "check_runs": runs }))
            })
            .mount(&self.server)
            .await;
    }

    fn enqueues(&self) -> Vec<u64> {
        self.enqueued.lock().unwrap().clone()
    }

    fn creates(&self) -> Vec<Value> {
        self.creates.lock().unwrap().clone()
    }

    fn set_pr(&self, number: u64, f: impl FnOnce(&mut FakePr)) {
        let mut prs = self.prs.lock().unwrap();
        let pr = prs
            .iter_mut()
            .find(|p| p.number == number)
            .expect("PR exists");
        f(pr);
    }

    /// The PR was merged outside graft, at `merge`.
    fn merge(&self, number: u64, merge: GitSha) {
        self.set_pr(number, |p| {
            p.open = false;
            p.merged = Some(merge.to_hex());
        });
    }

    /// The PR was closed without merging.
    fn close(&self, number: u64) {
        self.set_pr(number, |p| p.open = false);
    }

    async fn answer_pulls_with(&self, status: u16, retry_after: Option<&str>, times: Option<u64>) {
        let mut resp = ResponseTemplate::new(status).set_body_json(json!({"message": "slow down"}));
        if let Some(ra) = retry_after {
            resp = resp.insert_header("retry-after", ra);
        }
        let mut mock = Mock::given(method("POST"))
            .and(path(format!("/repos/{OWNER}/{GH_REPO}/pulls")))
            .respond_with(resp)
            .with_priority(1);
        if let Some(n) = times {
            mock = mock.up_to_n_times(n);
        }
        mock.mount(&self.server).await;
    }

    async fn pull_requests_seen(&self) -> usize {
        self.server
            .received_requests()
            .await
            .unwrap()
            .iter()
            .filter(|r| r.method.as_str() == "POST" && r.url.path().ends_with("/pulls"))
            .count()
    }
}

/// Lands a draft on `main` upstream by a squash commit that names `head`,
/// as GitHub would after the merge queue. Returns the merge commit.
fn squash_on_main(w: &World, head: ChangesetId) -> GitSha {
    w.commit(&format!("Add feature\n\nGraft-Changeset: {head}"))
}

/// The base commit, followed into the line, and the workspace that holds
/// the drafts opened on it.
async fn base(w: &World, db: &MirrorDb) -> ChangesetId {
    w.commit("base");
    w.push_main();
    assert!(matches!(w.follow(db).await, LineOutcome::Imported { .. }));
    let tip = w.tip().await.unwrap();
    w.api
        .open_workspace(
            &cx(),
            OpenWorkspace {
                repo: REPO.to_owned(),
                name: Some(WS.to_owned()),
                line: "main".to_owned(),
                at: None,
                lease_ms: 0,
            },
        )
        .await
        .unwrap();
    tip
}

#[tokio::test(flavor = "current_thread")]
async fn land_pr_merge_follow_landed() {
    let w = World::new().await;
    let db = w.open_db();
    let gh = FakeGitHub::start(&w.upstream).await;
    let forge = gh.forge();
    let tip = base(&w, &db).await;
    let head = draft(&w.api, tip, "feature.txt", b"feature\n", "Add feature").await;

    let mut bridge = w.bridge(&db, &forge);
    let rec = bridge.submit("ws1", head).await.unwrap();
    assert_eq!(rec.state, BridgeState::Open, "detail: {}", rec.detail);
    assert!(rec.enqueued);
    assert_eq!(rec.pr_number, Some(1));
    assert_eq!(gh.enqueues(), vec![1]);
    let branch = format!("graft/ws1/{}", short(head));
    assert_eq!(rec.branch, branch);
    assert_eq!(
        Some(branch_sha(&w.upstream, &branch)),
        rec.export_sha.map(|s| s.to_hex()),
        "the branch holds the exported head"
    );
    let body = gh.creates()[0]["body"].as_str().unwrap().to_owned();
    assert!(body.contains(WORK_ITEM), "PR body names the work item");
    assert!(body.contains(SESSION), "PR body names the session");

    // Merged outside graft; the merge commit names the head.
    let merge = squash_on_main(&w, head);
    w.push_main();
    gh.merge(1, merge);
    let tracked = bridge.track().await.unwrap();
    assert_eq!(tracked.len(), 1);
    assert_eq!(
        tracked[0].state,
        BridgeState::Merged,
        "merged, waiting for follow"
    );

    // Follow imports the merge commit; the next track lands it.
    assert!(matches!(w.follow(&db).await, LineOutcome::Imported { .. }));
    bridge.track().await.unwrap();
    let rec = bridge.request(head).unwrap().unwrap();
    assert_eq!(rec.state, BridgeState::Landed, "detail: {}", rec.detail);
    let landed = rec.landed.unwrap();
    assert_eq!(
        w.tip().await,
        Some(landed),
        "the line holds the squashed commit"
    );
    assert_eq!(db.sha_of(landed).unwrap(), Some(merge));
}

#[tokio::test(flavor = "current_thread")]
async fn a_closed_pr_is_rejected_with_its_url() {
    let w = World::new().await;
    let db = w.open_db();
    let gh = FakeGitHub::start(&w.upstream).await;
    let forge = gh.forge();
    let tip = base(&w, &db).await;
    let head = draft(&w.api, tip, "feature.txt", b"feature\n", "Add feature").await;

    let mut bridge = w.bridge(&db, &forge);
    bridge.submit("ws1", head).await.unwrap();
    gh.close(1);
    bridge.track().await.unwrap();
    let rec = bridge.request(head).unwrap().unwrap();
    assert_eq!(rec.state, BridgeState::Rejected);
    assert!(
        rec.detail.contains("closed without merging"),
        "{}",
        rec.detail
    );
    assert!(
        rec.detail
            .contains("https://github.com/indexarr/rusttorrent/pull/1")
    );
    assert!(rec.state.is_terminal());
}

#[tokio::test(flavor = "current_thread")]
async fn a_failing_check_rejects_with_the_check_url() {
    let w = World::new().await;
    let db = w.open_db();
    let gh = FakeGitHub::start(&w.upstream).await;
    let forge = gh.forge();
    let tip = base(&w, &db).await;
    let head = draft(&w.api, tip, "feature.txt", b"feature\n", "Add feature").await;

    let mut bridge = w.bridge(&db, &forge);
    bridge.submit("ws1", head).await.unwrap();
    *gh.failing_check.lock().unwrap() =
        Some(("test".to_owned(), "https://ci.example/run/9".to_owned()));
    bridge.track().await.unwrap();
    let rec = bridge.request(head).unwrap().unwrap();
    assert_eq!(rec.state, BridgeState::Rejected);
    assert!(
        rec.detail.contains("https://ci.example/run/9"),
        "{}",
        rec.detail
    );
}

#[tokio::test(flavor = "current_thread")]
async fn a_restart_mid_lifecycle_resumes() {
    let w = World::new().await;
    let db = w.open_db();
    let gh = FakeGitHub::start(&w.upstream).await;
    let forge = gh.forge();
    let tip = base(&w, &db).await;
    let head = draft(&w.api, tip, "feature.txt", b"feature\n", "Add feature").await;

    {
        let mut bridge = w.bridge(&db, &forge);
        let rec = bridge.submit("ws1", head).await.unwrap();
        assert_eq!(rec.state, BridgeState::Open);
    }
    // A fresh process: new DB handle, new bridge, same files.
    drop(db);
    let db = w.open_db();
    let mut bridge = w.bridge(&db, &forge);
    let tracked = bridge.track().await.unwrap();
    assert!(tracked.iter().all(|r| r.state == BridgeState::Open));
    assert_eq!(gh.creates().len(), 1, "the PR was not opened twice");
    assert_eq!(gh.enqueues(), vec![1], "enqueued once");

    gh.close(1);
    bridge.track().await.unwrap();
    assert_eq!(
        bridge.request(head).unwrap().unwrap().state,
        BridgeState::Rejected
    );
}

#[tokio::test(flavor = "current_thread")]
async fn duplicate_submits_do_not_repeat_work() {
    let w = World::new().await;
    let db = w.open_db();
    let gh = FakeGitHub::start(&w.upstream).await;
    let forge = gh.forge();
    let tip = base(&w, &db).await;
    let head = draft(&w.api, tip, "feature.txt", b"feature\n", "Add feature").await;

    let mut bridge = w.bridge(&db, &forge);
    let first = bridge.submit("ws1", head).await.unwrap();
    let again = bridge.submit("ws1", head).await.unwrap();
    assert_eq!(first, again);
    bridge.track().await.unwrap();
    assert_eq!(gh.creates().len(), 1);
    assert_eq!(gh.enqueues(), vec![1]);
    assert_eq!(gh.pull_requests_seen().await, 1);
}

#[tokio::test(flavor = "current_thread")]
async fn an_existing_pr_on_the_branch_is_adopted() {
    // A crash between opening the PR and recording it must not open a second.
    let w = World::new().await;
    let db = w.open_db();
    let gh = FakeGitHub::start(&w.upstream).await;
    let forge = gh.forge();
    let tip = base(&w, &db).await;
    let head = draft(&w.api, tip, "feature.txt", b"feature\n", "Add feature").await;

    let mut bridge = w.bridge(&db, &forge);
    let rec = bridge.submit("ws1", head).await.unwrap();
    // Forget the PR and the enqueue, as if the DB write never happened.
    let mut lost = rec.clone();
    lost.state = BridgeState::Pushed;
    lost.pr_number = None;
    lost.pr_url = None;
    lost.node_id = None;
    lost.enqueued = false;
    db.put_pull(&lost).unwrap();
    let rec = bridge.submit("ws1", head).await.unwrap();
    assert_eq!(rec.state, BridgeState::Open);
    assert_eq!(rec.pr_number, Some(1));
    assert_eq!(gh.creates().len(), 1, "adopted, not opened again");
}

#[tokio::test(flavor = "current_thread")]
async fn a_diverged_branch_is_not_overwritten() {
    let w = World::new().await;
    let db = w.open_db();
    let gh = FakeGitHub::start(&w.upstream).await;
    let forge = gh.forge();
    let tip = base(&w, &db).await;
    let head = draft(&w.api, tip, "feature.txt", b"feature\n", "Add feature").await;

    // Someone else already put commits on the branch we would use.
    let other = w.commit("someone else's work");
    git(
        &w.work,
        &[
            "push",
            "-q",
            "origin",
            &format!("{}:refs/heads/graft/ws1/{}", other.to_hex(), short(head)),
        ],
    );

    let mut bridge = w.bridge(&db, &forge);
    let rec = bridge.submit("ws1", head).await.unwrap();
    assert_eq!(rec.state, BridgeState::Rejected);
    assert!(rec.detail.contains("not overwritten"), "{}", rec.detail);
    assert_eq!(
        branch_sha(&w.upstream, &format!("graft/ws1/{}", short(head))),
        other.to_hex(),
        "the other commits are still there"
    );
    assert!(gh.creates().is_empty(), "no PR for a refused branch");
}

#[tokio::test(flavor = "current_thread")]
async fn a_force_pushed_upstream_stops_the_bridge() {
    let w = World::new().await;
    let db = w.open_db();
    let gh = FakeGitHub::start(&w.upstream).await;
    let forge = gh.forge();
    let tip = base(&w, &db).await;
    let head = draft(&w.api, tip, "feature.txt", b"feature\n", "Add feature").await;

    w.force_push_main();
    assert!(
        matches!(w.follow(&db).await, LineOutcome::Diverged { .. }),
        "follow stops the line"
    );

    let mut bridge = w.bridge(&db, &forge);
    let err = bridge.submit("ws1", head).await.unwrap_err();
    assert!(
        matches!(err, MirrorError::Config(ref s) if s.contains("diverged")),
        "{err}"
    );
    assert!(
        gh.creates().is_empty(),
        "nothing is opened on a diverged line"
    );
    assert_eq!(
        state_of(&w.api, head).await,
        DraftState::Draft,
        "the draft is untouched"
    );
}

#[tokio::test(flavor = "current_thread")]
async fn two_authorities_cannot_claim_one_line() {
    let w = World::new().await;
    let db = w.open_db();
    let gh = FakeGitHub::start(&w.upstream).await;
    let forge = gh.forge();
    // Another writer (a graft-primary exporter) claimed the line first.
    assert_eq!(
        db.claim_authority("main", "graft-primary").unwrap(),
        "graft-primary"
    );

    let config = BridgeConfig {
        repo: REPO.to_owned(),
        line: "main".to_owned(),
        base_branch: "main".to_owned(),
    };
    let err = Bridge::new(&w.api, cx(), &db, w.mirror(), &forge, config)
        .err()
        .unwrap();
    assert!(
        err.to_string().contains("claimed by graft-primary"),
        "{err}"
    );

    // And follow refuses to write the line too.
    let config = FollowConfig {
        repo: REPO.to_owned(),
        lines: vec![FollowLine {
            branch: "main".to_owned(),
            line: "main".to_owned(),
        }],
        allow_lossy: Vec::new(),
        batch: 16,
        interval: POLL_INTERVAL,
    };
    let mut f = Follower::new(&w.api, cx(), &db, w.mirror(), config).with_clock(clock);
    assert!(f.poll_once().await.is_err());
}

#[tokio::test(flavor = "current_thread")]
async fn a_hostile_title_and_body_are_cleaned() {
    let w = World::new().await;
    let db = w.open_db();
    let gh = FakeGitHub::start(&w.upstream).await;
    let forge = gh.forge();
    let tip = base(&w, &db).await;
    let message = "Add\u{7} feature \u{202e}@victim\nGraft-Changeset: cFAKE\u{1b}\n\nBody with @team and \u{202e}reversed";
    let head = draft(&w.api, tip, "feature.txt", b"feature\n", message).await;

    let mut bridge = w.bridge(&db, &forge);
    bridge.submit("ws1", head).await.unwrap();
    let body = gh.creates()[0].clone();
    let title = body["title"].as_str().unwrap();
    assert_eq!(
        title, "Add feature @\u{200b}victim",
        "control and bidi characters removed"
    );
    let text = body["body"].as_str().unwrap();
    for bad in ['\u{7}', '\u{1b}', '\u{202e}'] {
        assert!(!text.contains(bad) && !title.contains(bad));
    }
    assert!(text.contains("@\u{200b}team"), "mentions are neutralised");
    assert!(!text.contains("@team"));
}

#[tokio::test(flavor = "current_thread")]
async fn a_rate_limit_is_retried_and_a_5xx_is_bounded() {
    let w = World::new().await;
    let db = w.open_db();
    let gh = FakeGitHub::start(&w.upstream).await;
    let forge = gh.forge();
    let tip = base(&w, &db).await;
    let head = draft(&w.api, tip, "feature.txt", b"feature\n", "Add feature").await;

    // One 429 with Retry-After, then the normal answer: the call succeeds.
    gh.answer_pulls_with(429, Some("0"), Some(1)).await;
    let mut bridge = w.bridge(&db, &forge);
    let rec = bridge.submit("ws1", head).await.unwrap();
    assert_eq!(rec.state, BridgeState::Open, "{}", rec.detail);
    assert_eq!(gh.pull_requests_seen().await, 2, "one 429, then the PR");
}

#[tokio::test(flavor = "current_thread")]
async fn a_persistent_5xx_stops_after_bounded_attempts_and_resumes_later() {
    let w = World::new().await;
    let db = w.open_db();
    let gh = FakeGitHub::start(&w.upstream).await;
    let forge = gh.forge();
    let tip = base(&w, &db).await;
    let head = draft(&w.api, tip, "feature.txt", b"feature\n", "Add feature").await;

    gh.answer_pulls_with(500, None, Some(3)).await;
    let mut bridge = w.bridge(&db, &forge);
    let err = bridge.submit("ws1", head).await.unwrap_err();
    assert!(
        matches!(err, MirrorError::Forge(ForgeError::Transient(_))),
        "{err}"
    );
    assert_eq!(gh.pull_requests_seen().await, 3, "exactly the retry budget");
    let rec = bridge.request(head).unwrap().unwrap();
    assert_eq!(
        rec.state,
        BridgeState::Pushed,
        "the export survives; the PR is retried"
    );

    // GitHub recovers: the next submit opens the PR without re-exporting.
    let rec = bridge.submit("ws1", head).await.unwrap();
    assert_eq!(rec.state, BridgeState::Open);
    assert_eq!(gh.creates().len(), 1);
}

#[tokio::test(flavor = "current_thread")]
async fn a_stale_base_is_refused_before_anything_is_pushed() {
    let w = World::new().await;
    let db = w.open_db();
    let gh = FakeGitHub::start(&w.upstream).await;
    let forge = gh.forge();
    let tip = base(&w, &db).await;
    // Two drafts on the old tip. The line then moves on, so the stack is no
    // longer based on the tip and must be synced before it can be bridged.
    let stale = draft(&w.api, tip, "a.txt", b"a\n", "first").await;
    let head = draft(&w.api, stale, "b.txt", b"b\n", "second").await;
    w.commit("moves main");
    w.push_main();
    w.follow(&db).await;

    let mut bridge = w.bridge(&db, &forge);
    let err = bridge.submit("ws1", head).await.unwrap_err();
    assert!(matches!(err, MirrorError::Config(_)), "{err}");
    assert!(gh.creates().is_empty());
}

#[tokio::test(flavor = "current_thread")]
async fn the_merge_queue_is_retried_then_rejected() {
    let w = World::new().await;
    let db = w.open_db();
    let gh = FakeGitHub::start(&w.upstream).await;
    let forge = gh.forge();
    let tip = base(&w, &db).await;
    let head = draft(&w.api, tip, "feature.txt", b"feature\n", "Add feature").await;

    // The queue refuses outright: the request is rejected with the reason.
    Mock::given(method("POST"))
        .and(path("/graphql"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "errors": [{"message": "merge queue is closed"}]
        })))
        .with_priority(1)
        .mount(&gh.server)
        .await;
    let mut bridge = w.bridge(&db, &forge);
    let rec = bridge.submit("ws1", head).await.unwrap();
    assert_eq!(rec.state, BridgeState::Rejected);
    assert!(
        rec.detail.contains("merge queue is closed"),
        "{}",
        rec.detail
    );
}

#[tokio::test(flavor = "current_thread")]
async fn a_merge_that_does_not_name_the_head_is_rejected() {
    let w = World::new().await;
    let db = w.open_db();
    let gh = FakeGitHub::start(&w.upstream).await;
    let forge = gh.forge();
    let tip = base(&w, &db).await;
    let head = draft(&w.api, tip, "feature.txt", b"feature\n", "Add feature").await;

    let mut bridge = w.bridge(&db, &forge);
    bridge.submit("ws1", head).await.unwrap();
    // Someone merged the PR with an unrelated commit message.
    let merge = w.commit("unrelated squash");
    w.push_main();
    gh.merge(1, merge);
    assert!(matches!(w.follow(&db).await, LineOutcome::Imported { .. }));
    bridge.track().await.unwrap();
    let rec = bridge.request(head).unwrap().unwrap();
    assert_eq!(rec.state, BridgeState::Rejected);
    assert!(
        rec.detail.contains("does not name the stack head"),
        "{}",
        rec.detail
    );
    assert!(rec.landed.is_none());
}

#[tokio::test(flavor = "current_thread")]
async fn a_stack_is_exported_bottom_to_head() {
    let w = World::new().await;
    let db = w.open_db();
    let gh = FakeGitHub::start(&w.upstream).await;
    let forge = gh.forge();
    let tip = base(&w, &db).await;
    let bottom = draft(&w.api, tip, "a.txt", b"a\n", "first").await;
    let head = draft(&w.api, bottom, "b.txt", b"b\n", "second").await;

    let mut bridge = w.bridge(&db, &forge);
    let rec = bridge.submit("ws1", head).await.unwrap();
    assert_eq!(rec.state, BridgeState::Open, "{}", rec.detail);
    assert_eq!(rec.covered, vec![bottom, head]);
    let log = git(
        &w.mirror_dir,
        &["log", "--format=%s", &rec.export_sha.unwrap().to_hex()],
    );
    assert_eq!(
        log.lines().take(2).collect::<Vec<_>>(),
        vec!["second", "first"]
    );
    assert_eq!(gh.creates()[0]["head"], json!(rec.branch));
}

#[test]
fn cleaning_keeps_text_and_drops_what_could_reroute_it() {
    use graft_mirror::bridge::clean_text;
    assert_eq!(clean_text("a\u{7}b", 10, false), "ab");
    assert_eq!(clean_text("x\ny", 10, true), "x\ny");
    assert_eq!(clean_text("x\ny", 10, false), "xy");
    assert_eq!(clean_text("a\u{2066}b", 10, false), "ab");
    assert_eq!(clean_text("abcdef", 3, false), "abc");
}
