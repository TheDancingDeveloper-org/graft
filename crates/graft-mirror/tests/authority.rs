//! The authority switch (`docs/design.md` §11.3) against a local bare
//! repository standing in for GitHub, its hook gate, and an in-process
//! engine over the memory store; and the GitHub ruleset gate against a fake
//! API (wiremock).
//!
//! Covers a full A → B → A round trip that loses nothing and ends level;
//! the gate refusing people while frozen and admitting only the mirror while
//! graft-primary; every step's refusals (no claim, not level, not verified,
//! a side moved since verify, a re-export that does not match); aborting a
//! switch; and the ruleset bodies the GitHub gate sends.

mod support;

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::Arc;
use std::time::Duration;

use graft_engine::{Engine, EngineConfig};
use graft_git::GitSha;
use graft_git::map::MirrorMap;
use graft_mirror::authority::{
    Authority, Gate, GateState, HookGate, RulesetGate, Stage, Switcher, Unfrozen, ruleset_name,
};
use graft_mirror::credentials::{NoCredentials, SecretToken, StaticToken};
use graft_mirror::export::{Export, ExportConfig, ExportOutcome};
use graft_mirror::follow::{FollowConfig, FollowLine, Follower, LineOutcome};
use graft_mirror::github::{HttpForge, RetryPolicy};
use graft_mirror::{Mirror, MirrorDb, MirrorError, current_ref};
use graft_model::ChangesetId;
use graft_proto::api::AdminApi;
use graft_store::memory::MemoryStore;
use graft_testkit::clock::FakeClock;
use serde_json::{Value, json};
use support::*;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

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

fn main_line() -> FollowLine {
    FollowLine {
        branch: "main".to_owned(),
        line: "main".to_owned(),
    }
}

/// A bare "GitHub" behind a hook gate, a work tree people push from, the
/// mirror's directory and DB, and an engine.
struct World {
    tmp: tempfile::TempDir,
    github: PathBuf,
    work: PathBuf,
    mirror_dir: PathBuf,
    db: MirrorDb,
    api: Api,
    gate: HookGate,
    n: u64,
}

impl World {
    async fn new() -> World {
        let tmp = tempfile::tempdir().unwrap();
        let github = tmp.path().join("github.git");
        support::github(&github, true);
        let work = tmp.path().join("work");
        std::fs::create_dir_all(&work).unwrap();
        git(&work, &["init", "-q", "-b", "main"]);
        git(
            &work,
            &["remote", "add", "origin", github.to_str().unwrap()],
        );
        let api = engine();
        api.create_repo(&cx(), REPO.to_owned()).await.unwrap();
        let db = MirrorDb::open(&tmp.path().join("mirror.sqlite")).unwrap();
        World {
            mirror_dir: tmp.path().join("mirror.git"),
            gate: HookGate::new(&github),
            tmp,
            github,
            work,
            db,
            api,
            n: 0,
        }
    }

    fn mirror(&self) -> Mirror {
        Mirror::new(&self.mirror_dir, self.github.to_str().unwrap())
    }

    fn switcher(&self) -> Switcher<'_, Api, HookGate> {
        Switcher::new(
            &self.api,
            cx(),
            &self.db,
            self.mirror(),
            &self.gate,
            REPO,
            main_line(),
        )
    }

    /// A person commits a new file on top of GitHub's main and pushes it.
    /// `false` when GitHub refused the push.
    fn push(&mut self) -> bool {
        self.n += 1;
        if self.github_main().is_some() {
            git(&self.work, &["fetch", "-q", "origin"]);
            git(&self.work, &["reset", "-q", "--hard", "origin/main"]);
        }
        let file = format!("person-{}.txt", self.n);
        std::fs::write(self.work.join(&file), format!("by a person, {}\n", self.n)).unwrap();
        git(&self.work, &["add", &file]);
        git(
            &self.work,
            &["commit", "-q", "-m", &format!("person {}", self.n)],
        );
        try_git(
            &self.work,
            &["push", "-q", "origin", "HEAD:refs/heads/main"],
        )
    }

    /// Lands one native change on graft's main.
    async fn land(&mut self) -> ChangesetId {
        self.n += 1;
        let files = vec![(
            format!("native-{}.txt", self.n),
            format!("landed in graft, {}\n", self.n).into_bytes(),
        )];
        *land(&self.api, &[files], self.n).await.last().unwrap()
    }

    async fn follow(&self) -> Result<LineOutcome, MirrorError> {
        advance(1000);
        let mut follower = Follower::new(
            &self.api,
            cx(),
            &self.db,
            self.mirror(),
            FollowConfig {
                repo: REPO.to_owned(),
                lines: vec![main_line()],
                allow_lossy: Vec::new(),
                batch: 64,
                interval: Duration::from_secs(15),
            },
        )
        .with_clock(clock);
        Ok(follower.poll_once().await?.pop().unwrap().1)
    }

    fn export(&self) -> Result<Export<'_, Api, NoCredentials>, MirrorError> {
        Ok(Export::new(
            &self.api,
            cx(),
            &self.db,
            self.mirror(),
            &NoCredentials,
            ExportConfig::new(REPO, vec![main_line()]),
        )?
        .with_clock(clock))
    }

    async fn export_poll(&self) -> ExportOutcome {
        advance(1000);
        self.export()
            .unwrap()
            .poll_once()
            .await
            .unwrap()
            .pop()
            .unwrap()
            .1
    }

    async fn tip(&self) -> ChangesetId {
        current_ref(&self.api, &cx(), REPO, "main")
            .await
            .unwrap()
            .unwrap()
    }

    fn github_main(&self) -> Option<String> {
        branch(&self.github, "main")
    }

    async fn tip_sha(&self) -> String {
        self.db.sha_of(self.tip().await).unwrap().unwrap().to_hex()
    }

    /// GitHub's first-parent chain of main, oldest first.
    fn github_chain(&self) -> Vec<String> {
        git(
            &self.github,
            &["rev-list", "--first-parent", "--reverse", "refs/heads/main"],
        )
        .lines()
        .map(str::to_owned)
        .collect()
    }

    /// graft's main, oldest first, as git shas through the mirror map.
    async fn graft_chain(&self) -> Vec<String> {
        line_chain(&self.api)
            .await
            .into_iter()
            .map(|id| self.db.sha_of(id).unwrap().unwrap().to_hex())
            .collect()
    }

    fn switch_stage(&self) -> Option<String> {
        self.db.switch("main").unwrap().map(|s| s.stage)
    }
}

/// git that may fail: `false` on a non-zero exit.
fn try_git(dir: &Path, args: &[&str]) -> bool {
    Command::new("git")
        .current_dir(dir)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env_remove("GRAFT_MIRROR_PUSH")
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .expect("git runs")
        .success()
}

fn switch_err(r: Result<impl std::fmt::Debug, MirrorError>) -> String {
    match r {
        Err(MirrorError::Switch(what)) => what,
        other => panic!("expected a refused switch step, got {other:?}"),
    }
}

#[tokio::test]
async fn a_round_trip_a_to_b_to_a_loses_nothing_and_ends_level() {
    let mut w = World::new().await;

    // Mode A: people push, follow imports.
    for _ in 0..3 {
        assert!(w.push());
    }
    assert!(matches!(
        w.follow().await.unwrap(),
        LineOutcome::Imported { commits: 3, .. }
    ));
    let status = w.switcher().status().await.unwrap();
    assert_eq!(status.claim.as_deref(), Some("git-primary"));
    assert_eq!(status.gate, GateState::Open);
    assert!(status.in_sync());

    // A → B.
    let s = w.switcher().freeze().await.unwrap();
    assert_eq!(
        (s.from, s.to),
        (Authority::GitPrimary, Authority::GraftPrimary)
    );
    assert_eq!(w.gate.state("main").await.unwrap(), GateState::Frozen);
    assert!(!w.push(), "a frozen branch takes no pushes");
    assert!(switch_err(w.switcher().flip().await).contains("verify"));
    let v = w.switcher().verify(100).await.unwrap();
    assert_eq!(v.checked, 3);
    assert_eq!(v.sha.to_hex(), w.github_main().unwrap());
    let s = w.switcher().flip().await.unwrap();
    assert_eq!(s.stage, Stage::Flipped);
    assert_eq!(
        w.db.authority("main").unwrap().as_deref(),
        Some("graft-primary")
    );
    assert_eq!(w.gate.state("main").await.unwrap(), GateState::MirrorOnly);
    assert!(
        w.follow().await.is_err(),
        "follow does not write a graft-primary line"
    );
    assert_eq!(
        w.switcher().unfreeze().await.unwrap(),
        Unfrozen::Finished(Authority::GraftPrimary)
    );
    assert_eq!(w.switch_stage(), None);

    // Mode B: export starts level; graft lands, export pushes; people
    // cannot push.
    assert_eq!(w.export_poll().await, ExportOutcome::UpToDate);
    w.land().await;
    w.land().await;
    assert!(matches!(
        w.export_poll().await,
        ExportOutcome::Pushed { moves: 2, .. }
    ));
    assert_eq!(w.github_main().unwrap(), w.tip_sha().await);
    assert!(!w.push(), "only the mirror writes a graft-primary branch");

    // B → A: graft lands one more before its lands stop; export catches up.
    let s = w.switcher().freeze().await.unwrap();
    assert_eq!(
        (s.from, s.to),
        (Authority::GraftPrimary, Authority::GitPrimary)
    );
    w.land().await;
    assert!(switch_err(w.switcher().verify(100).await).contains("not caught up"));
    assert!(matches!(
        w.export_poll().await,
        ExportOutcome::Pushed { moves: 1, .. }
    ));
    let v = w.switcher().verify(100).await.unwrap();
    assert_eq!(v.checked, 6);
    let mut running = w.export().unwrap();
    w.switcher().flip().await.unwrap();
    assert_eq!(
        w.db.authority("main").unwrap().as_deref(),
        Some("git-primary")
    );
    assert_eq!(w.gate.state("main").await.unwrap(), GateState::Frozen);
    assert!(
        matches!(
            running.poll_once().await.unwrap().pop().unwrap().1,
            ExportOutcome::Failed(_)
        ),
        "a running export stops writing the line"
    );
    assert!(w.export().is_err(), "a new export refuses the line");
    assert_eq!(
        w.switcher().unfreeze().await.unwrap(),
        Unfrozen::Finished(Authority::GitPrimary)
    );
    assert_eq!(w.gate.state("main").await.unwrap(), GateState::Open);

    // Mode A again: people push on top of graft's history; follow imports.
    let before = w.tip().await;
    assert!(w.push());
    match w.follow().await.unwrap() {
        LineOutcome::Imported { commits, from, .. } => {
            assert_eq!((commits, from), (1, Some(before)));
        }
        other => panic!("follow: {other:?}"),
    }

    // Nothing lost, and both sides end on the same commits.
    assert_eq!(w.github_chain().len(), 7);
    assert_eq!(w.graft_chain().await, w.github_chain());
    assert_eq!(w.github_main().unwrap(), w.tip_sha().await);
    assert!(w.switcher().status().await.unwrap().in_sync());
    w.switcher().verify(100).await.unwrap();
}

#[tokio::test]
async fn freeze_needs_a_claim_and_verify_needs_both_sides_level() {
    let mut w = World::new().await;
    assert!(w.push());
    assert!(switch_err(w.switcher().freeze().await).contains("no authority"));

    w.follow().await.unwrap();
    assert!(w.push(), "upstream moves on before the freeze");
    w.switcher().freeze().await.unwrap();
    assert!(switch_err(w.switcher().verify(10).await).contains("not level"));
    assert_eq!(w.switch_stage().as_deref(), Some("frozen"));

    // Follow still catches up while frozen.
    assert!(matches!(
        w.follow().await.unwrap(),
        LineOutcome::Imported { commits: 1, .. }
    ));
    assert_eq!(w.switcher().verify(10).await.unwrap().checked, 2);
    assert_eq!(w.switch_stage().as_deref(), Some("verified"));
    assert!(matches!(
        w.switcher().verify(0).await,
        Err(MirrorError::Config(_))
    ));
}

#[tokio::test]
async fn flip_refuses_when_a_side_moved_since_verify() {
    let mut w = World::new().await;
    assert!(w.push());
    w.follow().await.unwrap();
    w.switcher().freeze().await.unwrap();
    w.switcher().verify(10).await.unwrap();
    w.switcher().flip().await.unwrap();
    w.switcher().unfreeze().await.unwrap();
    w.export_poll().await;

    // B → A, but graft lands after verify: flip refuses and goes back to
    // frozen, and nothing changes hands.
    w.switcher().freeze().await.unwrap();
    w.switcher().verify(10).await.unwrap();
    w.land().await;
    assert!(switch_err(w.switcher().flip().await).contains("moved since verify"));
    assert_eq!(w.switch_stage().as_deref(), Some("frozen"));
    assert_eq!(
        w.db.authority("main").unwrap().as_deref(),
        Some("graft-primary")
    );

    // Export catches up, verify again, and the flip goes through.
    w.export_poll().await;
    w.switcher().verify(10).await.unwrap();
    w.switcher().flip().await.unwrap();
    assert!(switch_err(w.switcher().flip().await).contains("unfreeze"));
    assert!(switch_err(w.switcher().freeze().await).contains("unfreeze"));
}

#[tokio::test]
async fn unfreeze_before_flip_aborts_the_switch() {
    let mut w = World::new().await;
    assert!(w.push());
    w.follow().await.unwrap();
    w.switcher().freeze().await.unwrap();
    assert!(!w.push());
    assert_eq!(
        w.switcher().unfreeze().await.unwrap(),
        Unfrozen::Aborted(Authority::GitPrimary)
    );
    assert_eq!(w.switch_stage(), None);
    assert_eq!(
        w.db.authority("main").unwrap().as_deref(),
        Some("git-primary")
    );
    assert!(w.push(), "the branch is open again");
    assert!(matches!(
        w.follow().await.unwrap(),
        LineOutcome::Imported { commits: 1, .. }
    ));
    // With no switch, unfreeze sets the holder's posture again.
    assert_eq!(
        w.switcher().unfreeze().await.unwrap(),
        Unfrozen::NoSwitch(Authority::GitPrimary)
    );
}

#[tokio::test]
async fn verify_refuses_a_commit_that_does_not_re_export() {
    let mut w = World::new().await;
    assert!(w.push());
    assert!(w.push());
    w.follow().await.unwrap();
    // The mirror DB's record of the tip no longer builds its commit.
    let tip = w.tip().await;
    let mut record = w.db.imported(tip).unwrap().unwrap();
    record.headers =
        b"author Someone Else <x@example.com> 1 +0000\ncommitter C <c@example.com> 1 +0000\n\n"
            .to_vec();
    w.db.put_imported(tip, record).unwrap();
    assert!(switch_err(w.switcher().verify(10).await).contains("re-exports"));
}

#[tokio::test]
async fn verify_refuses_a_stopped_line() {
    let mut w = World::new().await;
    assert!(w.push());
    w.follow().await.unwrap();
    let mut state = w.db.line("main").unwrap();
    state.diverged = Some(GitSha([7; 20]));
    w.db.set_line("main", &state).unwrap();
    assert!(switch_err(w.switcher().verify(10).await).contains("follow is stopped"));
}

#[tokio::test]
async fn the_hook_gate_keeps_a_foreign_hook() {
    let w = World::new().await;
    let hook = w.github.join("hooks/pre-receive");
    std::fs::write(&hook, "#!/bin/sh\nexit 0\n").unwrap();
    assert!(matches!(
        w.gate.set("main", GateState::Frozen).await,
        Err(MirrorError::Config(_))
    ));
    assert_eq!(
        std::fs::read_to_string(&hook).unwrap(),
        "#!/bin/sh\nexit 0\n"
    );
    assert!(HookGate::for_upstream(w.github.to_str().unwrap()).is_some());
    assert!(HookGate::for_upstream("https://github.com/o/r.git").is_none());
    assert!(HookGate::for_upstream(w.tmp.path().to_str().unwrap()).is_none());
}

fn forge(server: &MockServer) -> HttpForge<StaticToken> {
    HttpForge::new(
        &server.uri(),
        "o",
        "r",
        StaticToken::new(SecretToken::new("fake-token-for-tests-only".into()).unwrap()),
        RetryPolicy {
            attempts: 1,
            ..RetryPolicy::default()
        },
    )
    .unwrap()
}

#[tokio::test]
async fn the_ruleset_gate_freezes_admits_the_app_and_opens() {
    let server = MockServer::start().await;
    let name = ruleset_name("main");
    Mock::given(method("GET"))
        .and(path("/repos/o/r/rulesets"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!([
            { "id": 3, "name": "merge-queue" },
            { "id": 9, "name": name }
        ])))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/repos/o/r/rulesets/9"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "id": 9, "name": name,
            "bypass_actors": [{ "actor_id": 42, "actor_type": "Integration" }]
        })))
        .mount(&server)
        .await;
    Mock::given(method("PUT"))
        .and(path("/repos/o/r/rulesets/9"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({ "id": 9 })))
        .mount(&server)
        .await;
    Mock::given(method("DELETE"))
        .and(path("/repos/o/r/rulesets/9"))
        .respond_with(ResponseTemplate::new(204))
        .mount(&server)
        .await;
    let forge = forge(&server);

    let gate = RulesetGate::new(&forge, Some(42));
    assert_eq!(gate.state("main").await.unwrap(), GateState::MirrorOnly);
    gate.set("main", GateState::Frozen).await.unwrap();
    gate.set("main", GateState::MirrorOnly).await.unwrap();
    gate.set("main", GateState::Open).await.unwrap();
    let no_app = RulesetGate::new(&forge, None);
    assert!(matches!(
        no_app.set("main", GateState::MirrorOnly).await,
        Err(MirrorError::Config(_))
    ));

    let puts: Vec<Value> = server
        .received_requests()
        .await
        .unwrap()
        .into_iter()
        .filter(|r| r.method.as_str() == "PUT")
        .map(|r| serde_json::from_slice(&r.body).unwrap())
        .collect();
    assert_eq!(puts.len(), 2);
    assert_eq!(puts[0]["bypass_actors"], json!([]));
    assert_eq!(puts[1]["bypass_actors"][0]["actor_id"], json!(42));
    for body in &puts {
        assert_eq!(body["name"], json!(name));
        assert_eq!(
            body["conditions"]["ref_name"]["include"],
            json!(["refs/heads/main"])
        );
        assert_eq!(body["enforcement"], json!("active"));
    }
}

#[tokio::test]
async fn the_ruleset_gate_creates_its_ruleset_when_absent() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/repos/o/r/rulesets"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!([])))
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/repos/o/r/rulesets"))
        .respond_with(ResponseTemplate::new(201).set_body_json(json!({ "id": 1 })))
        .expect(1)
        .mount(&server)
        .await;
    let forge = forge(&server);
    let gate = RulesetGate::new(&forge, None);
    assert_eq!(gate.state("main").await.unwrap(), GateState::Open);
    gate.set("main", GateState::Frozen).await.unwrap();
    // Opening a branch with no ruleset calls nothing more.
    gate.set("main", GateState::Open).await.unwrap();
}
