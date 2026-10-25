//! Many working copies on one host (WI-2313): two directories bound to one
//! workspace, the workspace lifecycle when a working copy is closed or
//! deleted, and a working copy inside a git checkout. Against the real
//! engine over the in-memory store.

#[path = "support/engine.rs"]
mod engine;

use std::fs;
use std::path::Path;
use std::process::Command;
use std::sync::Arc;

use engine::EngineRepo;
use graft_client::fetch::FetchOptions;
use graft_client::workcopy::{
    CommitLimits, CommitOptions, DEFAULT_CACHE_BYTES, FileStatus, GRAFT_DIR, LandOptions,
    LandOutcome, WcConfig, WorkingCopy, clone,
};
use graft_client::workspace::DEFAULT_LEASE_MS;
use graft_client::{Error, Sparse};
use graft_model::{Author, AuthorKind, ChangesetId, RepoPath, Timestamp};
use graft_proto::api::{RepoApi, WorkspaceApi};
use graft_proto::types::{DraftState, GcReport, GetChangesets, WorkspaceRef};
use graft_proto::{ApiError, CallContext};

const REPO: &str = "r";
const RETENTION_MS: i64 = 7 * 24 * 3600 * 1000;

fn base_files() -> Vec<(&'static str, &'static [u8])> {
    vec![("README.md", b"hello\n"), ("src/lib.rs", b"fn a() {}\n")]
}

fn opts(message: &str) -> CommitOptions {
    CommitOptions {
        message: Some(message.to_owned()),
        author: Author {
            name: "Tester".into(),
            email: "tester@example.com".into(),
            kind: AuthorKind::Agent,
        },
        authored_at: Timestamp {
            ms: 1_700_000_000_000,
            offset_min: 0,
        },
        metadata: Default::default(),
        amend: false,
        limits: CommitLimits::default(),
    }
}

struct Env {
    repo: Arc<EngineRepo>,
    dir: tempfile::TempDir,
}

impl Env {
    async fn new() -> Env {
        Env {
            repo: EngineRepo::direct(REPO, &base_files()).await,
            dir: tempfile::tempdir().unwrap(),
        }
    }

    /// The context of one client process: `who` stands for its request ids,
    /// which the CLI makes unique per process (`graft_client::request_id`).
    fn cx(&self, who: &str) -> CallContext {
        let mut cx = self.repo.cx();
        cx.request_id = format!("{who}-1");
        cx
    }

    /// A clone into `name`, binding `workspace` (or a server-picked one),
    /// not yet begun.
    async fn clone_bare(&self, name: &str, workspace: Option<&str>) -> WorkingCopy {
        let config = WcConfig {
            server: None,
            endpoint: None,
            repo: REPO.to_owned(),
            workspace: workspace.map(str::to_owned),
        };
        clone(
            &*self.repo,
            &self.cx(name),
            &self.dir.path().join(name),
            config,
            "main",
            None,
            Sparse::full(),
            &FetchOptions::default(),
        )
        .await
        .unwrap()
        .0
    }

    async fn clone(&self, name: &str) -> WorkingCopy {
        let mut wc = self.clone_bare(name, None).await;
        wc.begin(&*self.repo, &self.cx(name), DEFAULT_LEASE_MS)
            .await
            .unwrap();
        wc
    }

    async fn commit(
        &self,
        wc: &mut WorkingCopy,
        who: &str,
        lease_ms: u64,
    ) -> Result<ChangesetId, Error> {
        wc.begin(&*self.repo, &self.cx(who), lease_ms).await?;
        Ok(wc.commit(&*self.repo, &self.cx(who), &opts(who)).await?.id)
    }

    async fn land(&self, wc: &mut WorkingCopy, who: &str) -> Result<LandOutcome, Error> {
        wc.begin(&*self.repo, &self.cx(who), DEFAULT_LEASE_MS)
            .await?;
        let cache = wc.open_cache(DEFAULT_CACHE_BYTES)?;
        let lo = LandOptions {
            contended_wait: std::time::Duration::ZERO,
            ..LandOptions::default()
        };
        wc.land(
            &*self.repo,
            &cache,
            &self.cx(who),
            &lo,
            &FetchOptions::default(),
        )
        .await
    }

    async fn sync(&self, wc: &mut WorkingCopy, who: &str) -> Result<(), Error> {
        wc.begin(&*self.repo, &self.cx(who), DEFAULT_LEASE_MS)
            .await?;
        let cache = wc.open_cache(DEFAULT_CACHE_BYTES)?;
        wc.sync(
            &*self.repo,
            &cache,
            &self.cx(who),
            DEFAULT_LEASE_MS,
            &FetchOptions::default(),
        )
        .await
        .map(|_| ())
    }

    fn wref(&self, wc: &WorkingCopy) -> WorkspaceRef {
        WorkspaceRef {
            repo: REPO.into(),
            name: wc.config().workspace.clone().unwrap(),
        }
    }

    async fn head(&self, wc: &WorkingCopy) -> Option<ChangesetId> {
        self.repo
            .get_workspace(&self.cx("check"), self.wref(wc))
            .await
            .unwrap()
            .head
    }

    async fn state(&self, id: ChangesetId) -> DraftState {
        let got = self
            .repo
            .get_changesets(
                &self.cx("check"),
                GetChangesets {
                    repo: REPO.into(),
                    ids: vec![id],
                },
            )
            .await
            .unwrap();
        got.found[0].state
    }
}

fn write(root: &Path, path: &str, data: &str) {
    let full = root.join(path);
    fs::create_dir_all(full.parent().unwrap()).unwrap();
    fs::write(full, data).unwrap();
}

fn read(root: &Path, path: &str) -> String {
    fs::read_to_string(root.join(path)).unwrap()
}

fn copy_dir(from: &Path, to: &Path) {
    fs::create_dir_all(to).unwrap();
    for entry in fs::read_dir(from).unwrap() {
        let entry = entry.unwrap();
        let dest = to.join(entry.file_name());
        if entry.file_type().unwrap().is_dir() {
            copy_dir(&entry.path(), &dest);
        } else {
            fs::copy(entry.path(), dest).unwrap();
        }
    }
}

fn diverged(r: &Result<impl std::fmt::Debug, Error>, server: Option<ChangesetId>) -> bool {
    matches!(r, Err(Error::Diverged { server: s, .. }) if *s == server)
}

// --- One workspace, two directories --------------------------------------

#[tokio::test]
async fn two_unnamed_clones_under_one_principal_get_their_own_workspaces() {
    let env = Env::new().await;
    let a = env.clone("a").await;
    let b = env.clone("b").await;
    assert_ne!(a.config().workspace, b.config().workspace);
}

#[tokio::test]
async fn a_copied_working_copy_cannot_overwrite_the_original() {
    let env = Env::new().await;
    let mut a = env.clone("a").await;
    write(a.root(), "src/lib.rs", "fn a() { 1 }\n");
    let d1 = env.commit(&mut a, "a", DEFAULT_LEASE_MS).await.unwrap();

    // `cp -r a b`: the copy binds the same workspace at the same head.
    copy_dir(a.root(), &env.dir.path().join("b"));
    let mut b = WorkingCopy::open(&env.dir.path().join("b")).unwrap();
    assert_eq!(b.config().workspace, a.config().workspace);

    // The original commits again; the copy's commit, sync and land are
    // each refused with `Diverged`, and the server keeps the original's head.
    write(a.root(), "src/lib.rs", "fn a() { 2 }\n");
    let d2 = env.commit(&mut a, "a", DEFAULT_LEASE_MS).await.unwrap();
    write(b.root(), "README.md", "copy\n");
    assert!(diverged(
        &env.commit(&mut b, "b", DEFAULT_LEASE_MS).await,
        Some(d2)
    ));
    assert!(diverged(&env.sync(&mut b, "b").await, Some(d2)));
    assert!(diverged(&env.land(&mut b, "b").await, Some(d2)));
    assert_eq!(env.head(&a).await, Some(d2));
    assert_eq!(b.state().head, Some(d1));
    assert_eq!(read(b.root(), "README.md"), "copy\n");

    // The original is untouched and still lands.
    assert_eq!(read(a.root(), "src/lib.rs"), "fn a() { 2 }\n");
    let out = env.land(&mut a, "a").await.unwrap();
    assert!(matches!(out, LandOutcome::Landed { landed, .. } if landed == d2));
    assert_eq!(env.repo.tip().await, d2);
    // After the land the copy is still refused, not rebased onto it.
    assert!(diverged(
        &env.commit(&mut b, "b", DEFAULT_LEASE_MS).await,
        None
    ));
}

#[tokio::test]
async fn whichever_directory_writes_first_wins() {
    let env = Env::new().await;
    let a = env.clone("a").await;
    // Both directories at the same base and no head.
    copy_dir(a.root(), &env.dir.path().join("b"));
    let mut a = a;
    let mut b = WorkingCopy::open(&env.dir.path().join("b")).unwrap();

    write(b.root(), "src/lib.rs", "fn a() { b }\n");
    let db = env.commit(&mut b, "b", DEFAULT_LEASE_MS).await.unwrap();
    write(a.root(), "src/lib.rs", "fn a() { a }\n");
    assert!(diverged(
        &env.commit(&mut a, "a", DEFAULT_LEASE_MS).await,
        Some(db)
    ));
    assert_eq!(env.head(&b).await, Some(db));
    assert_eq!(a.state().head, None);
    // The loser's edit is still on its disk, uncommitted.
    assert_eq!(
        a.status().unwrap(),
        vec![FileStatus::Modified(RepoPath::new("src/lib.rs").unwrap())]
    );
}

#[tokio::test]
async fn naming_a_live_workspace_in_a_second_clone_is_refused_at_its_first_write() {
    let env = Env::new().await;
    let mut a = env.clone("a").await;
    let ws = a.config().workspace.clone().unwrap();
    write(a.root(), "src/lib.rs", "fn a() { 1 }\n");
    let d1 = env.commit(&mut a, "a", DEFAULT_LEASE_MS).await.unwrap();

    // `graft clone --workspace <a's name>` in a second directory.
    let mut b = env.clone_bare("b", Some(&ws)).await;
    let begun = b.begin(&*env.repo, &env.cx("b"), DEFAULT_LEASE_MS).await;
    assert!(diverged(&begun, Some(d1)));
    assert_eq!(env.head(&a).await, Some(d1));
    write(a.root(), "src/lib.rs", "fn a() { 2 }\n");
    env.commit(&mut a, "a", DEFAULT_LEASE_MS).await.unwrap();
}

// --- Lifecycle --------------------------------------------------------------

#[tokio::test]
async fn closing_the_workspace_releases_it_and_its_drafts() {
    let env = Env::new().await;
    let mut a = env.clone("a").await;
    write(a.root(), "src/lib.rs", "fn a() { 1 }\n");
    let d1 = env.commit(&mut a, "a", DEFAULT_LEASE_MS).await.unwrap();
    let ws = env.wref(&a);

    // What `graft workspace close` does inside the working copy.
    env.repo
        .close_workspace(&env.cx("a"), ws.clone(), false)
        .await
        .unwrap();
    a.set_workspace(None).unwrap();
    assert!(matches!(
        env.repo.get_workspace(&env.cx("check"), ws).await,
        Err(ApiError::NotFound { .. })
    ));
    assert_eq!(env.state(d1).await, DraftState::Abandoned);

    // Collectable once the retention window passes, not before.
    assert_eq!(env.repo.gc_dry_run().await, GcReport::default());
    env.repo.clock().advance(RETENTION_MS + 1);
    assert!(env.repo.gc_dry_run().await.changesets >= 1);
}

#[tokio::test]
async fn a_deleted_working_copy_releases_its_drafts_at_the_lease() {
    const LEASE_MS: u64 = 60_000;
    let env = Env::new().await;
    let mut a = env.clone("a").await;
    write(a.root(), "src/lib.rs", "fn a() { 1 }\n");
    let d1 = env.commit(&mut a, "a", LEASE_MS).await.unwrap();
    let ws = env.wref(&a);

    // `rm -rf a`: nothing tells the server.
    fs::remove_dir_all(a.root()).unwrap();
    drop(a);
    assert_eq!(env.state(d1).await, DraftState::Draft);
    assert_eq!(env.repo.expire_leases().await, 0);

    env.repo.clock().advance(LEASE_MS as i64 + 1);
    assert_eq!(env.repo.expire_leases().await, 1);
    assert_eq!(env.state(d1).await, DraftState::Abandoned);
    assert!(matches!(
        env.repo.get_workspace(&env.cx("check"), ws).await,
        Err(ApiError::NotFound { .. })
    ));
    assert_eq!(env.repo.gc_dry_run().await, GcReport::default());
    env.repo.clock().advance(RETENTION_MS + 1);
    assert!(env.repo.gc_dry_run().await.changesets >= 1);
}

// --- Inside a git checkout ----------------------------------------------------

#[tokio::test]
async fn a_working_copy_inside_a_git_checkout_never_lists_or_commits_git_metadata() {
    let env = Env::new().await;
    let root = env.dir.path().join("g");
    // A git checkout's metadata: a directory, and the pointer file a linked
    // git worktree has instead (only one exists at a time in real life).
    write(&root, ".git/HEAD", "ref: refs/heads/main\n");
    write(&root, ".git/objects/ab/cdef", "x");
    write(&root, ".jj/repo", "x");
    let mut wc = env.clone_bare("g", None).await;
    wc.begin(&*env.repo, &env.cx("g"), DEFAULT_LEASE_MS)
        .await
        .unwrap();
    assert!(wc.status().unwrap().is_empty(), "{:?}", wc.status());

    write(&root, "src/lib.rs", "fn a() { g }\n");
    let d = env.commit(&mut wc, "g", DEFAULT_LEASE_MS).await.unwrap();
    let files = env.repo.files(d).await;
    assert!(
        files
            .keys()
            .all(|p| !p.starts_with(".git") && !p.starts_with(".jj"))
    );
    assert_eq!(read(&root, ".git/HEAD"), "ref: refs/heads/main\n");

    // `.graft/` ignores itself for the surrounding git checkout.
    let ignore = read(&root, &format!("{GRAFT_DIR}/.gitignore"));
    assert!(ignore.lines().any(|l| l == "*"));
}

#[tokio::test]
async fn a_git_worktree_pointer_file_is_never_listed() {
    let env = Env::new().await;
    let root = env.dir.path().join("w");
    write(&root, ".git", "gitdir: /elsewhere/.git/worktrees/w\n");
    let mut wc = env.clone_bare("w", None).await;
    wc.begin(&*env.repo, &env.cx("w"), DEFAULT_LEASE_MS)
        .await
        .unwrap();
    assert!(wc.status().unwrap().is_empty());
}

#[tokio::test]
async fn git_ignores_the_graft_directory() {
    if Command::new("git").arg("--version").output().is_err() {
        eprintln!("git not installed: skipped");
        return;
    }
    let env = Env::new().await;
    let root = env.dir.path().join("g");
    fs::create_dir_all(&root).unwrap();
    let git = |args: &[&str]| {
        Command::new("git")
            .args(args)
            .current_dir(&root)
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .output()
            .unwrap()
    };
    assert!(git(&["init", "-q"]).status.success());
    let _wc = env.clone_bare("g", None).await;
    let out = git(&["status", "--porcelain", "--untracked-files=all"]);
    let listed = String::from_utf8(out.stdout).unwrap();
    assert!(!listed.contains(GRAFT_DIR), "{listed}");
    assert!(listed.contains("README.md"), "{listed}");
}
