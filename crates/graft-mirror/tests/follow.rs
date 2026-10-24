//! Import and follow against local bare repositories standing in for GitHub.
//! The graft side is an in-process `graft-engine` over the memory store,
//! called through the API traits directly.

use std::cell::Cell;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::Arc;

use graft_engine::{Engine, EngineConfig};
use graft_git::GitSha;
use graft_git::import::ImportConfig;
use graft_git::map::MirrorMap;
use graft_mirror::follow::{Divergence, FollowConfig, FollowLine, Follower, LineOutcome};
use graft_mirror::import::import_repo;
use graft_mirror::{Mirror, MirrorDb, current_ref, tag_ref};
use graft_model::ChangesetId;
use graft_proto::api::AdminApi;
use graft_proto::{CallContext, Principal, PrincipalKind};
use graft_store::memory::MemoryStore;
use graft_telemetry::Metrics;
use graft_testkit::clock::FakeClock;

const REPO: &str = "r";

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

thread_local! {
    static NOW: Cell<i64> = const { Cell::new(1_700_000_000_000) };
}

fn clock() -> i64 {
    NOW.with(Cell::get)
}

fn advance(ms: i64) {
    NOW.with(|n| n.set(n.get() + ms));
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

    /// Commits a change to a file of its own and returns the sha.
    fn commit(&self) -> GitSha {
        self.n.set(self.n.get() + 1);
        let n = self.n.get();
        std::fs::write(self.work.join(format!("f{n}.txt")), format!("file {n}\n")).unwrap();
        std::fs::write(self.work.join("shared.txt"), format!("rev {n}\n")).unwrap();
        git(&self.work, &["add", "-A"]);
        git(&self.work, &["commit", "-q", "-m", &format!("commit {n}")]);
        GitSha::from_hex(&git(&self.work, &["rev-parse", "HEAD"])).unwrap()
    }

    fn push(&self) {
        git(&self.work, &["push", "-q", "origin", "main"]);
    }

    fn force_push(&self) {
        git(&self.work, &["push", "-q", "--force", "origin", "main"]);
    }

    fn open_db(&self) -> MirrorDb {
        MirrorDb::open(&self.db_path).unwrap()
    }

    fn follower<'a>(&'a self, db: &'a MirrorDb, metrics: &Metrics) -> Follower<'a, Api> {
        let config = FollowConfig {
            repo: REPO.to_owned(),
            lines: vec![FollowLine {
                branch: "main".to_owned(),
                line: "main".to_owned(),
            }],
            allow_lossy: Vec::new(),
            batch: 2,
            interval: graft_mirror::follow::POLL_INTERVAL,
            tags: Some("*".to_owned()),
        };
        let mirror = Mirror::new(&self.mirror_dir, self.upstream.to_str().unwrap());
        Follower::new(&self.api, cx(), db, mirror, config)
            .with_metrics(metrics.clone())
            .with_clock(clock)
    }

    async fn tip(&self) -> Option<ChangesetId> {
        current_ref(&self.api, &cx(), REPO, "main").await.unwrap()
    }

    /// The git sha graft's `main` holds, through the mirror DB.
    async fn tip_sha(&self, db: &MirrorDb) -> Option<GitSha> {
        let tip = self.tip().await?;
        db.sha_of(tip).unwrap()
    }
}

fn metrics() -> Metrics {
    graft_telemetry::registry().unwrap().1
}

fn divergence(m: &Metrics) -> i64 {
    m.mirror_divergence.with_label_values(&["main"]).get()
}

fn lag_seconds(m: &Metrics) -> f64 {
    m.mirror_lag_seconds.with_label_values(&["main"]).get()
}

fn lag_changesets(m: &Metrics) -> i64 {
    m.mirror_lag_changesets.with_label_values(&["main"]).get()
}

async fn poll(f: &mut Follower<'_, Api>) -> LineOutcome {
    let mut out = f.poll_once().await.unwrap();
    assert_eq!(out.len(), 1);
    let (line, outcome) = out.pop().unwrap();
    assert_eq!(line, "main");
    outcome
}

#[tokio::test(flavor = "current_thread")]
async fn new_commits_appear_within_one_poll() {
    let w = World::new().await;
    let db = w.open_db();
    let m = metrics();

    let a = w.commit();
    w.push();
    let mut f = w.follower(&db, &m);
    match poll(&mut f).await {
        LineOutcome::Imported { commits, from, .. } => {
            assert_eq!(commits, 1);
            assert_eq!(from, None, "the line is created");
        }
        other => panic!("expected an import, got {other:?}"),
    }
    assert_eq!(w.tip_sha(&db).await, Some(a));

    drop(f);
    w.commit();
    w.commit();
    let c = w.commit();
    w.push();
    let mut f = w.follower(&db, &m);
    let before = w.tip().await;
    match poll(&mut f).await {
        LineOutcome::Imported { commits, from, to } => {
            assert_eq!(commits, 3, "only the new commits, in batches of 2");
            assert_eq!(from, before);
            assert_eq!(Some(to), w.tip().await);
        }
        other => panic!("expected an import, got {other:?}"),
    }
    assert_eq!(w.tip_sha(&db).await, Some(c));
    assert_eq!(db.commit_count().unwrap(), 4);

    assert_eq!(poll(&mut f).await, LineOutcome::UpToDate);
    assert_eq!(divergence(&m), 0);
    assert_eq!(lag_seconds(&m), 0.0);
    assert_eq!(lag_changesets(&m), 0);
}

#[tokio::test(flavor = "current_thread")]
async fn force_push_raises_divergence_and_changes_nothing() {
    let w = World::new().await;
    let db = w.open_db();
    let m = metrics();
    let a = w.commit();
    let b = w.commit();
    w.push();
    let mut f = w.follower(&db, &m);
    assert!(matches!(poll(&mut f).await, LineOutcome::Imported { .. }));
    let tip = w.tip().await;
    assert_eq!(w.tip_sha(&db).await, Some(b));

    // Rewrite history upstream: drop `b`, commit something else on `a`.
    git(&w.work, &["reset", "-q", "--hard", &a.to_hex()]);
    let stray = w.commit();
    w.force_push();
    let imported = db.commit_count().unwrap();

    advance(5_000);
    assert_eq!(
        poll(&mut f).await,
        LineOutcome::Diverged {
            reason: Divergence::NonFastForward,
            upstream: stray,
        }
    );
    assert_eq!(w.tip().await, tip, "graft's line did not move");
    assert_eq!(db.commit_count().unwrap(), imported, "nothing imported");
    assert_eq!(db.changeset_of(stray).unwrap(), None);
    assert_eq!(divergence(&m), 1);

    // The line stays stopped, and its lag keeps growing.
    advance(30_000);
    assert_eq!(
        poll(&mut f).await,
        LineOutcome::Diverged {
            reason: Divergence::Held,
            upstream: stray,
        }
    );
    assert_eq!(w.tip().await, tip);
    assert_eq!(divergence(&m), 1);
    assert_eq!(lag_seconds(&m), 30.0);

    // Upstream restored to a fast-forward of graft: the line resumes.
    git(&w.work, &["reset", "-q", "--hard", &b.to_hex()]);
    let d = w.commit();
    w.force_push();
    match poll(&mut f).await {
        LineOutcome::Imported { commits, from, .. } => {
            assert_eq!(commits, 1);
            assert_eq!(from, tip);
        }
        other => panic!("expected an import, got {other:?}"),
    }
    assert_eq!(w.tip_sha(&db).await, Some(d));
    assert_eq!(divergence(&m), 0);
    assert_eq!(lag_seconds(&m), 0.0);
}

#[tokio::test(flavor = "current_thread")]
async fn restart_resumes_from_the_mirror_db_without_duplicates() {
    let w = World::new().await;
    w.commit();
    w.commit();
    w.push();
    {
        let db = w.open_db();
        let m = metrics();
        let mut f = w.follower(&db, &m);
        match poll(&mut f).await {
            LineOutcome::Imported { commits, .. } => assert_eq!(commits, 2),
            other => panic!("expected an import, got {other:?}"),
        }
    }

    // A new process: fresh follower, the DB reopened from disk.
    let db = w.open_db();
    let m = metrics();
    let tip = w.tip().await;
    let mut f = w.follower(&db, &m);
    assert_eq!(poll(&mut f).await, LineOutcome::UpToDate);
    assert_eq!(w.tip().await, tip);

    let c = w.commit();
    w.push();
    drop(f);
    let db = w.open_db();
    let mut f = w.follower(&db, &m);
    match poll(&mut f).await {
        LineOutcome::Imported { commits, from, .. } => {
            assert_eq!(commits, 1, "only the new commit; none re-imported");
            assert_eq!(from, tip);
        }
        other => panic!("expected an import, got {other:?}"),
    }
    assert_eq!(w.tip_sha(&db).await, Some(c));
    assert_eq!(db.commit_count().unwrap(), 3);
}

#[tokio::test(flavor = "current_thread")]
async fn import_sets_lines_and_tags_and_reruns_cleanly() {
    let w = World::new().await;
    let a = w.commit();
    git(&w.work, &["tag", "v1"]);
    let b = w.commit();
    let db = w.open_db();
    let config = ImportConfig::new(w.work.join(".git"), REPO);

    let out = import_repo(&w.api, &cx(), &db, &config).await.unwrap();
    assert_eq!(out.report.commits, 2);
    assert_eq!(out.lines_moved, ["main"]);
    assert_eq!(out.tags_created, ["v1"]);
    assert_eq!(w.tip_sha(&db).await, Some(b));
    let v1 = current_ref(&w.api, &cx(), REPO, &tag_ref("v1"))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(db.sha_of(v1).unwrap(), Some(a));

    let again = import_repo(&w.api, &cx(), &db, &config).await.unwrap();
    assert_eq!(again.report.commits, 0, "nothing re-imported");
    assert!(again.lines_moved.is_empty());
    assert!(again.tags_created.is_empty());
    assert!(again.tags_conflicting.is_empty());

    // Follow picks up where the import left off, from the same DB.
    let m = metrics();
    w.push();
    let mut f = w.follower(&db, &m);
    assert_eq!(poll(&mut f).await, LineOutcome::UpToDate);
}

#[tokio::test(flavor = "current_thread")]
async fn upstream_tags_become_immutable_graft_tags() {
    let w = World::new().await;
    let db = w.open_db();
    let m = metrics();
    let a = w.commit();
    git(&w.work, &["tag", "v1"]);
    let b = w.commit();
    git(&w.work, &["tag", "-a", "-m", "release 2", "v2"]);
    w.push();
    git(&w.work, &["push", "-q", "origin", "v1", "v2"]);

    let mut f = w.follower(&db, &m);
    assert!(matches!(poll(&mut f).await, LineOutcome::Imported { .. }));
    let tags = f.last_tags().clone();
    let id_a = db.changeset_of(a).unwrap().unwrap();
    let id_b = db.changeset_of(b).unwrap().unwrap();
    assert_eq!(
        tags.created,
        vec![("v1".to_owned(), id_a), ("v2".to_owned(), id_b)],
        "the annotated tag is peeled to its commit"
    );
    let v2 = current_ref(&w.api, &cx(), REPO, &tag_ref("v2"))
        .await
        .unwrap();
    assert_eq!(v2, Some(id_b));

    // A tag on a commit graft lacks waits; the next poll imports and tags it.
    git(&w.work, &["checkout", "-q", "-b", "side"]);
    w.commit();
    git(&w.work, &["tag", "v-side"]);
    git(&w.work, &["push", "-q", "origin", "v-side"]);
    git(&w.work, &["checkout", "-q", "main"]);
    assert_eq!(poll(&mut f).await, LineOutcome::UpToDate);
    assert_eq!(f.last_tags().waiting, 1);
    assert!(f.last_tags().created.is_empty());

    // Moved and deleted upstream: reported, graft unchanged.
    git(&w.work, &["tag", "-f", "v1", &b.to_hex()]);
    git(&w.work, &["push", "-q", "--force", "origin", "v1"]);
    git(&w.work, &["push", "-q", "origin", ":refs/tags/v2"]);
    assert_eq!(poll(&mut f).await, LineOutcome::UpToDate);
    assert_eq!(f.last_tags().moved, vec![("v1".to_owned(), b)]);
    assert_eq!(f.last_tags().deleted, vec!["v2".to_owned()]);
    let v1 = current_ref(&w.api, &cx(), REPO, &tag_ref("v1"))
        .await
        .unwrap();
    assert_eq!(v1, Some(id_a), "graft tags never move");
    let v2 = current_ref(&w.api, &cx(), REPO, &tag_ref("v2"))
        .await
        .unwrap();
    assert_eq!(v2, Some(id_b), "nor disappear");

    // Reported once, then quiet.
    assert_eq!(poll(&mut f).await, LineOutcome::UpToDate);
    assert!(f.last_tags().moved.is_empty() && f.last_tags().deleted.is_empty());

    // A restarted follower re-checks against graft and creates nothing.
    drop(f);
    let mut f = w.follower(&db, &m);
    assert_eq!(poll(&mut f).await, LineOutcome::UpToDate);
    assert!(f.last_tags().created.is_empty());
    assert_eq!(
        f.last_tags().conflicting,
        vec![("v1".to_owned(), id_a)],
        "upstream's moved v1 conflicts with graft's immutable one"
    );
}

/// WI-2370: annotated, signed and nested tags pushed after follow starts keep
/// their tag objects, so export names them by upstream's own shas.
#[tokio::test(flavor = "current_thread")]
async fn tags_pushed_after_follow_starts_export_with_upstream_shas() {
    let w = World::new().await;
    let db = w.open_db();
    let m = metrics();
    let a = w.commit();
    w.push();
    let mut f = w.follower(&db, &m);
    assert!(matches!(poll(&mut f).await, LineOutcome::Imported { .. }));

    w.commit();
    w.push();
    git(&w.work, &["tag", "-a", "-m", "release 1", "v1"]);
    git(
        &w.work,
        &["tag", "-a", "-m", "the release of v1", "v1-outer", "v1"],
    );
    git(&w.work, &["tag", "plain"]);
    // A signed tag: the signature is message bytes to git, so a stand-in
    // block exercises the same path as a real one.
    let signed = w.work.join("signed.tag");
    std::fs::write(
        &signed,
        format!(
            "object {}\ntype commit\ntag v0-signed\ntagger C O Mitter \
             <committer@example.com> 1700000000 +0000\n\nsigned release\n\
             -----BEGIN PGP SIGNATURE-----\n\nc3RhbmQtaW4=\n\
             -----END PGP SIGNATURE-----\n",
            a.to_hex()
        ),
    )
    .unwrap();
    let signed_sha = git(
        &w.work,
        &["hash-object", "-t", "tag", "-w", signed.to_str().unwrap()],
    );
    git(&w.work, &["update-ref", "refs/tags/v0-signed", &signed_sha]);
    git(&w.work, &["push", "-q", "origin", "--tags"]);

    assert!(matches!(poll(&mut f).await, LineOutcome::Imported { .. }));
    assert_eq!(f.last_tags().created.len(), 4);

    let dest = w.upstream.with_file_name("exit.git");
    graft_mirror::export::export_all(&w.api, &cx(), &db, REPO, &dest)
        .await
        .unwrap();
    for tag in ["v1", "v1-outer", "v0-signed", "plain"] {
        let r = format!("refs/tags/{tag}");
        assert_eq!(
            git(&dest, &["rev-parse", &r]),
            git(&w.upstream, &["rev-parse", &r]),
            "{tag} exports with upstream's sha"
        );
    }
    assert_eq!(git(&dest, &["cat-file", "-t", "v1-outer"]), "tag");
}
