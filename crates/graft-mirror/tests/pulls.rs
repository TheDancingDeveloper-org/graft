//! Importing GitHub PRs on a graft-primary line, against a local bare
//! repository and a fake GitHub API (wiremock) that reads it: a PR counts as
//! merged once its head is on `main`, as GitHub decides.

mod support;

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use graft_engine::{Engine, EngineConfig};
use graft_git::GitSha;
use graft_git::map::MirrorMap;
use graft_mirror::credentials::{NoCredentials, SecretToken, StaticToken};
use graft_mirror::export::{Export, ExportConfig, ExportOutcome};
use graft_mirror::follow::FollowLine;
use graft_mirror::github::{HttpForge, RetryPolicy};
use graft_mirror::pulls::{ImportState, PrImport, PrImporter, pr_workspace};
use graft_mirror::{Mirror, MirrorDb, current_ref};
use graft_model::ChangesetId;
use graft_proto::ApiError;
use graft_proto::api::{AdminApi, RepoApi, WorkspaceApi};
use graft_proto::types::{DraftState, GetChangesets, WorkspaceRef};
use graft_store::memory::MemoryStore;
use graft_testkit::clock::FakeClock;
use serde_json::{Value, json};
use std::time::Duration;
use support::*;
use wiremock::matchers::{method, path_regex};
use wiremock::{Mock, MockServer, Request, ResponseTemplate};

type Api = Engine<MemoryStore, MemoryStore>;

const OWNER: &str = "indexarr";
const GH_REPO: &str = "rusttorrent";

fn engine() -> Api {
    let store = Arc::new(MemoryStore::new());
    Engine::new(
        store.clone(),
        store,
        Arc::new(FakeClock::at(1_000_000)),
        EngineConfig::default(),
    )
}

fn main_line() -> FollowLine {
    FollowLine {
        branch: "main".to_owned(),
        line: "main".to_owned(),
    }
}

#[derive(Clone, Debug)]
struct FakePull {
    number: u64,
    head_ref: String,
    open: bool,
}

#[derive(Default)]
struct Fake {
    pulls: Vec<FakePull>,
    approved: HashSet<u64>,
    comments: Vec<(u64, String)>,
    closed: Vec<u64>,
}

/// GitHub's API over the bare repository at `github`.
struct FakeGitHub {
    server: MockServer,
    state: Arc<Mutex<Fake>>,
}

fn head_of(github: &Path, number: u64) -> String {
    git(github, &["rev-parse", &format!("refs/pull/{number}/head")])
}

fn on_main(github: &Path, sha: &str) -> bool {
    std::process::Command::new("git")
        .current_dir(github)
        .args(["merge-base", "--is-ancestor", sha, "refs/heads/main"])
        .status()
        .unwrap()
        .success()
}

fn number_in(req: &Request, at: usize) -> u64 {
    req.url.path().split('/').nth(at).unwrap().parse().unwrap()
}

impl FakeGitHub {
    async fn start(github: &Path) -> FakeGitHub {
        let fake = FakeGitHub {
            server: MockServer::start().await,
            state: Arc::new(Mutex::new(Fake::default())),
        };
        let base = format!("^/repos/{OWNER}/{GH_REPO}");
        // Open PRs. A PR whose head is on main is merged, so not open.
        let (state, gh) = (fake.state.clone(), github.to_owned());
        Mock::given(method("GET"))
            .and(path_regex(format!("{base}/pulls$")))
            .respond_with(move |_: &Request| {
                let s = state.lock().unwrap();
                let list: Vec<Value> = s
                    .pulls
                    .iter()
                    .filter(|p| p.open && !on_main(&gh, &head_of(&gh, p.number)))
                    .map(|p| {
                        json!({
                            "number": p.number,
                            "html_url": format!("https://github.test/pull/{}", p.number),
                            "head": { "sha": head_of(&gh, p.number), "ref": p.head_ref },
                            "base": { "sha": git(&gh, &["rev-parse", "main"]), "ref": "main" },
                        })
                    })
                    .collect();
                ResponseTemplate::new(200).set_body_json(list)
            })
            .mount(&fake.server)
            .await;
        let state = fake.state.clone();
        Mock::given(method("GET"))
            .and(path_regex(format!(r"{base}/pulls/\d+/reviews$")))
            .respond_with(move |req: &Request| {
                let n = number_in(req, 5);
                let reviews = if state.lock().unwrap().approved.contains(&n) {
                    json!([{ "user": { "id": 1 }, "state": "APPROVED" }])
                } else {
                    json!([{ "user": { "id": 1 }, "state": "COMMENTED" }])
                };
                ResponseTemplate::new(200).set_body_json(reviews)
            })
            .mount(&fake.server)
            .await;
        let (state, gh) = (fake.state.clone(), github.to_owned());
        Mock::given(method("GET"))
            .and(path_regex(format!(r"{base}/pulls/\d+$")))
            .respond_with(move |req: &Request| {
                let n = number_in(req, 5);
                let s = state.lock().unwrap();
                let p = s.pulls.iter().find(|p| p.number == n).unwrap();
                ResponseTemplate::new(200).set_body_json(pull_json(&gh, p))
            })
            .mount(&fake.server)
            .await;
        let (state, gh) = (fake.state.clone(), github.to_owned());
        Mock::given(method("PATCH"))
            .and(path_regex(format!(r"{base}/pulls/\d+$")))
            .respond_with(move |req: &Request| {
                let n = number_in(req, 5);
                let mut s = state.lock().unwrap();
                s.closed.push(n);
                let p = s.pulls.iter_mut().find(|p| p.number == n).unwrap();
                p.open = false;
                ResponseTemplate::new(200).set_body_json(pull_json(&gh, p))
            })
            .mount(&fake.server)
            .await;
        let state = fake.state.clone();
        Mock::given(method("POST"))
            .and(path_regex(format!(r"{base}/issues/\d+/comments$")))
            .respond_with(move |req: &Request| {
                let n = number_in(req, 5);
                let body: Value = serde_json::from_slice(&req.body).unwrap();
                state
                    .lock()
                    .unwrap()
                    .comments
                    .push((n, body["body"].as_str().unwrap().to_owned()));
                ResponseTemplate::new(201).set_body_json(json!({}))
            })
            .mount(&fake.server)
            .await;
        fake
    }

    fn forge(&self) -> HttpForge<StaticToken> {
        HttpForge::new(
            &self.server.uri(),
            OWNER,
            GH_REPO,
            StaticToken::new(SecretToken::new("fake-token-for-tests-only".into()).unwrap()),
            RetryPolicy {
                attempts: 2,
                base: Duration::from_millis(1),
                max_wait: Duration::from_millis(5),
            },
        )
        .unwrap()
    }

    fn open(&self, number: u64, head_ref: &str) {
        self.state.lock().unwrap().pulls.push(FakePull {
            number,
            head_ref: head_ref.to_owned(),
            open: true,
        });
    }

    fn approve(&self, number: u64) {
        self.state.lock().unwrap().approved.insert(number);
    }

    fn close_by_author(&self, number: u64) {
        let mut s = self.state.lock().unwrap();
        s.pulls
            .iter_mut()
            .find(|p| p.number == number)
            .unwrap()
            .open = false;
    }
}

fn pull_json(gh: &Path, p: &FakePull) -> Value {
    let head = head_of(gh, p.number);
    let merged = on_main(gh, &head);
    json!({
        "number": p.number,
        "html_url": format!("https://github.test/pull/{}", p.number),
        "node_id": format!("PR_{}", p.number),
        "state": if p.open && !merged { "open" } else { "closed" },
        "merged": merged,
        "merge_commit_sha": if merged { Value::String(head.clone()) } else { Value::Null },
        "head": { "sha": head },
    })
}

struct World {
    tmp: tempfile::TempDir,
    github: PathBuf,
    mirror_dir: PathBuf,
    db: MirrorDb,
    api: Api,
    fake: FakeGitHub,
    lands: u64,
}

impl World {
    async fn new() -> World {
        let tmp = tempfile::tempdir().unwrap();
        let github = tmp.path().join("github.git");
        support::github(&github, false);
        let api = engine();
        api.create_repo(&cx(), REPO.to_owned()).await.unwrap();
        let db = MirrorDb::open(&tmp.path().join("mirror.sqlite")).unwrap();
        let fake = FakeGitHub::start(&github).await;
        let mut w = World {
            mirror_dir: tmp.path().join("mirror.git"),
            tmp,
            github,
            db,
            api,
            fake,
            lands: 0,
        };
        w.land_one().await;
        assert!(matches!(w.export().await, ExportOutcome::Pushed { .. }));
        w
    }

    fn mirror(&self) -> Mirror {
        Mirror::new(&self.mirror_dir, self.github.to_str().unwrap())
    }

    async fn export(&self) -> ExportOutcome {
        advance(1000);
        let mut export = Export::new(
            &self.api,
            cx(),
            &self.db,
            self.mirror(),
            &NoCredentials,
            ExportConfig::new(REPO, vec![main_line()]),
        )
        .unwrap()
        .with_clock(clock);
        export.poll_once().await.unwrap().pop().unwrap().1
    }

    async fn import(&self) -> Vec<PrImport> {
        let forge = self.fake.forge();
        let lander = FakeLand(&self.api);
        let mut importer = PrImporter::new(
            &self.api,
            cx(),
            &self.db,
            self.mirror(),
            &forge,
            &lander,
            REPO,
            main_line(),
        );
        importer.poll_once().await.unwrap()
    }

    async fn land_one(&mut self) -> ChangesetId {
        self.lands += 1;
        let n = self.lands;
        let files = vec![(format!("f{n}.txt"), format!("file {n}\n").into_bytes())];
        *land(&self.api, &[files], n).await.last().unwrap()
    }

    async fn tip(&self) -> ChangesetId {
        current_ref(&self.api, &cx(), REPO, "main")
            .await
            .unwrap()
            .unwrap()
    }

    /// Pushes a PR branch: `commits` commits on top of `base` (GitHub's main
    /// when `None`), as `refs/pull/<n>/head`.
    fn push_pr(&self, number: u64, base: Option<&str>, commits: u32) -> String {
        let push = rand_suffix();
        let work = self.tmp.path().join(format!("pr-{number}-{push}"));
        git(
            self.tmp.path(),
            &[
                "clone",
                "-q",
                self.github.to_str().unwrap(),
                work.to_str().unwrap(),
            ],
        );
        if let Some(base) = base {
            git(
                &work,
                &["fetch", "-q", "origin", "+refs/pull/*:refs/remotes/pull/*"],
            );
            git(&work, &["checkout", "-q", base]);
        }
        for i in 0..commits {
            let name = format!("pr{number}-{push}-{i}.txt");
            std::fs::write(work.join(name), format!("pr {number} {push} {i}\n")).unwrap();
            git(&work, &["add", "-A"]);
            git(
                &work,
                &["commit", "-q", "-m", &format!("PR {number} change {i}")],
            );
        }
        git(
            &work,
            &[
                "push",
                "-q",
                "-f",
                "origin",
                &format!("HEAD:refs/pull/{number}/head"),
            ],
        );
        git(&work, &["rev-parse", "HEAD"])
    }

    fn github_main(&self) -> String {
        branch(&self.github, "main").unwrap()
    }

    async fn workspace_head(&self, number: u64) -> Result<Option<ChangesetId>, ApiError> {
        self.api
            .get_workspace(
                &cx(),
                WorkspaceRef {
                    repo: REPO.to_owned(),
                    name: pr_workspace(number),
                },
            )
            .await
            .map(|ws| ws.head)
    }

    async fn state_of(&self, id: ChangesetId) -> DraftState {
        let got = self
            .api
            .get_changesets(
                &cx(),
                GetChangesets {
                    repo: REPO.to_owned(),
                    ids: vec![id],
                },
            )
            .await
            .unwrap();
        got.found[0].state
    }

    fn record(&self, number: u64) -> PrImport {
        self.db.pr_import(number).unwrap().unwrap()
    }
}

fn rand_suffix() -> u64 {
    use std::sync::atomic::{AtomicU64, Ordering};
    static N: AtomicU64 = AtomicU64::new(0);
    N.fetch_add(1, Ordering::Relaxed)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_approved_pr_lands_and_github_sees_it_merged_by_the_export() {
    let w = World::new().await;
    let head = w.push_pr(7, None, 2);
    w.fake.open(7, "feature");
    w.import().await;
    let rec = w.record(7);
    assert_eq!(rec.state, ImportState::Held);
    assert_eq!(rec.workspace, "github-pr/7");
    let draft =
        w.db.changeset_of(GitSha::from_hex(&head).unwrap())
            .unwrap()
            .unwrap();
    assert_eq!(rec.head, draft);
    assert_eq!(w.workspace_head(7).await.unwrap(), Some(draft));
    assert_eq!(w.state_of(draft).await, DraftState::Draft);
    // Not approved yet: still held on the next poll.
    w.import().await;
    assert_eq!(w.record(7).state, ImportState::Held);

    w.fake.approve(7);
    w.import().await;
    let rec = w.record(7);
    assert_eq!(rec.state, ImportState::Landed, "{}", rec.detail);
    assert_eq!(w.tip().await, draft, "no rebase: the PR's own changesets");
    // Export pushes the PR's own commits: GitHub sees it merged.
    assert!(matches!(w.export().await, ExportOutcome::Pushed { .. }));
    assert_eq!(w.github_main(), head);
    w.import().await;
    assert_eq!(w.record(7).state, ImportState::Done);
    let s = w.fake.state.lock().unwrap();
    assert!(
        s.closed.is_empty() && s.comments.is_empty(),
        "merged by GitHub"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_pr_landed_by_rebase_is_closed_with_a_comment_once_exported() {
    let mut w = World::new().await;
    let base = w.github_main();
    w.push_pr(8, Some(&base), 1);
    w.fake.open(8, "dependabot/cargo/serde-1.0.200");
    w.import().await;
    assert_eq!(w.record(8).state, ImportState::Held);
    // The line moves on before the approval.
    w.land_one().await;
    w.export().await;
    w.fake.approve(8);
    w.import().await;
    let rec = w.record(8);
    assert_eq!(rec.state, ImportState::Landed, "{}", rec.detail);
    let landed = rec.landed.unwrap();
    assert_ne!(landed, rec.head, "rebased onto the new tip");
    // Not exported yet: the PR stays open.
    assert!(w.fake.state.lock().unwrap().closed.is_empty());
    w.export().await;
    w.import().await;
    assert_eq!(w.record(8).state, ImportState::Done);
    let s = w.fake.state.lock().unwrap();
    assert_eq!(s.closed, [8]);
    assert!(
        s.comments[0].1.contains(&landed.to_string()),
        "{:?}",
        s.comments
    );
    let files = git(&w.github, &["ls-tree", "--name-only", "main"]);
    assert!(files.lines().any(|f| f.starts_with("pr8-")), "{files}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_new_push_to_the_pr_replaces_its_drafts() {
    let w = World::new().await;
    let first = w.push_pr(9, None, 1);
    w.fake.open(9, "feature");
    w.import().await;
    let old = w.record(9).head;
    let second = w.push_pr(9, Some(&first), 1);
    w.import().await;
    let rec = w.record(9);
    assert_eq!(rec.head_sha.to_hex(), second);
    assert_ne!(rec.head, old);
    assert_eq!(w.workspace_head(9).await.unwrap(), Some(rec.head));
    // A PR moved onto a newer base: its workspace moves there too.
    let mut w = w;
    w.land_one().await;
    w.export().await;
    w.push_pr(9, None, 1);
    w.import().await;
    let rec = w.record(9);
    assert_eq!(rec.state, ImportState::Held, "{}", rec.detail);
    assert_eq!(w.workspace_head(9).await.unwrap(), Some(rec.head));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_pr_closed_without_approval_lets_its_drafts_go() {
    let w = World::new().await;
    w.push_pr(10, None, 1);
    w.fake.open(10, "feature");
    w.import().await;
    let draft = w.record(10).head;
    w.fake.close_by_author(10);
    w.import().await;
    assert_eq!(w.record(10).state, ImportState::Closed);
    assert!(matches!(
        w.workspace_head(10).await,
        Err(ApiError::NotFound { .. })
    ));
    assert_eq!(w.state_of(draft).await, DraftState::Abandoned);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_bridges_own_branches_and_foreign_history_are_not_held() {
    let w = World::new().await;
    w.push_pr(11, None, 1);
    w.fake.open(11, "graft/ws1/0123456789ab");
    w.import().await;
    assert!(w.db.pr_import(11).unwrap().is_none());

    // A PR with no history in common with the line is refused, not held.
    let work = w.tmp.path().join("foreign");
    std::fs::create_dir_all(&work).unwrap();
    git(&work, &["init", "-q", "-b", "x"]);
    std::fs::write(work.join("x"), "x\n").unwrap();
    git(&work, &["add", "-A"]);
    git(&work, &["commit", "-q", "-m", "foreign"]);
    git(
        &work,
        &[
            "push",
            "-q",
            w.github.to_str().unwrap(),
            "x:refs/pull/12/head",
        ],
    );
    w.fake.open(12, "foreign");
    w.import().await;
    let rec = w.record(12);
    assert_eq!(rec.state, ImportState::Refused);
    assert!(rec.detail.contains("no history"), "{}", rec.detail);
    // Nothing moved on the line.
    let tip = w.tip().await;
    w.import().await;
    assert_eq!(w.tip().await, tip);
    let _ = w.api.list_refs(&cx(), Default::default()).await;
}
