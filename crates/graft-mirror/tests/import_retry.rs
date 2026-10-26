//! Import retries `Unavailable` against a fake API. The sleeper records the
//! hinted delay and returns at once, so nothing asserts wall-clock time.

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use graft_git::import::ImportConfig;
use graft_mirror::import::import_repo_waiting;
use graft_mirror::{MirrorDb, MirrorError};
use graft_model::ObjectId;
use graft_proto::api::{AdminApi, ObjectApi, RepoApi};
use graft_proto::types::{
    Changesets, Created, GetChangesets, HasObjects, ImportChangesets, PutObjects, RefView,
    ResolveRef, UpdateRef,
};
use graft_proto::{ApiError, CallContext, Principal, PrincipalKind};

const REPO: &str = "r";

struct Clock {
    slept: Mutex<Vec<Duration>>,
}

impl graft_mirror::import::Sleeper for Clock {
    async fn sleep(&self, delay: Duration) {
        self.slept.lock().unwrap().push(delay);
    }
}

/// Sheds load `fails` times on every method import calls, then succeeds.
/// `fails == u32::MAX` never succeeds.
struct Shedding<A> {
    inner: A,
    fails: AtomicU32,
    calls: AtomicU32,
}

impl<A> Shedding<A> {
    fn gate(&self) -> Result<(), ApiError> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        let left = self.fails.load(Ordering::SeqCst);
        if left == 0 {
            return Ok(());
        }
        if left != u32::MAX {
            self.fails.fetch_sub(1, Ordering::SeqCst);
        }
        Err(ApiError::Unavailable {
            retry_after_ms: 1_000,
        })
    }
}

impl<A: AdminApi> AdminApi for Shedding<A> {
    async fn create_repo(&self, cx: &CallContext, name: String) -> Result<(), ApiError> {
        self.gate()?;
        self.inner.create_repo(cx, name).await
    }

    async fn import_changesets(
        &self,
        cx: &CallContext,
        req: ImportChangesets,
    ) -> Result<Vec<Created>, ApiError> {
        self.gate()?;
        self.inner.import_changesets(cx, req).await
    }

    async fn gc(
        &self,
        cx: &CallContext,
        req: graft_proto::types::Gc,
    ) -> Result<graft_proto::types::GcReport, ApiError> {
        self.inner.gc(cx, req).await
    }

    async fn mint_token(
        &self,
        cx: &CallContext,
        req: graft_proto::types::MintToken,
    ) -> Result<String, ApiError> {
        self.inner.mint_token(cx, req).await
    }
}

impl<A: ObjectApi> ObjectApi for Shedding<A> {
    async fn get_objects(
        &self,
        cx: &CallContext,
        req: graft_proto::types::GetObjects,
    ) -> Result<graft_proto::types::Objects, ApiError> {
        self.inner.get_objects(cx, req).await
    }

    async fn has_objects(
        &self,
        cx: &CallContext,
        req: HasObjects,
    ) -> Result<Vec<ObjectId>, ApiError> {
        self.gate()?;
        self.inner.has_objects(cx, req).await
    }

    async fn put_objects(
        &self,
        cx: &CallContext,
        req: PutObjects,
    ) -> Result<Vec<ObjectId>, ApiError> {
        self.gate()?;
        self.inner.put_objects(cx, req).await
    }
}

impl<A: RepoApi> RepoApi for Shedding<A> {
    async fn create_changeset(
        &self,
        cx: &CallContext,
        req: graft_proto::types::CreateChangeset,
    ) -> Result<Created, ApiError> {
        self.inner.create_changeset(cx, req).await
    }

    async fn get_changesets(
        &self,
        cx: &CallContext,
        req: GetChangesets,
    ) -> Result<Changesets, ApiError> {
        self.gate()?;
        self.inner.get_changesets(cx, req).await
    }

    async fn resolve_ref(&self, cx: &CallContext, req: ResolveRef) -> Result<RefView, ApiError> {
        self.gate()?;
        self.inner.resolve_ref(cx, req).await
    }

    async fn list_refs(
        &self,
        cx: &CallContext,
        req: graft_proto::types::ListRefs,
    ) -> Result<Vec<RefView>, ApiError> {
        self.inner.list_refs(cx, req).await
    }

    async fn update_ref(&self, cx: &CallContext, req: UpdateRef) -> Result<RefView, ApiError> {
        self.gate()?;
        self.inner.update_ref(cx, req).await
    }

    async fn stat(
        &self,
        cx: &CallContext,
        req: graft_proto::types::PathAt,
    ) -> Result<graft_proto::types::EntryView, ApiError> {
        self.inner.stat(cx, req).await
    }

    async fn list_tree(
        &self,
        cx: &CallContext,
        req: graft_proto::types::ListTree,
    ) -> Result<graft_proto::types::Entries, ApiError> {
        self.inner.list_tree(cx, req).await
    }

    async fn read_file(
        &self,
        cx: &CallContext,
        req: graft_proto::types::PathAt,
    ) -> Result<graft_proto::types::FileContent, ApiError> {
        self.inner.read_file(cx, req).await
    }

    async fn log(
        &self,
        cx: &CallContext,
        req: graft_proto::types::Log,
    ) -> Result<graft_proto::types::History, ApiError> {
        self.inner.log(cx, req).await
    }

    async fn is_ancestor(
        &self,
        cx: &CallContext,
        req: graft_proto::types::Pair,
    ) -> Result<bool, ApiError> {
        self.inner.is_ancestor(cx, req).await
    }

    async fn merge_base(
        &self,
        cx: &CallContext,
        req: graft_proto::types::Pair,
    ) -> Result<Option<graft_model::ChangesetId>, ApiError> {
        self.inner.merge_base(cx, req).await
    }

    async fn diff(
        &self,
        cx: &CallContext,
        req: graft_proto::types::Diff,
    ) -> Result<graft_proto::types::DiffResult, ApiError> {
        self.inner.diff(cx, req).await
    }

    async fn blame(
        &self,
        cx: &CallContext,
        req: graft_proto::types::Blame,
    ) -> Result<graft_proto::types::BlameResult, ApiError> {
        self.inner.blame(cx, req).await
    }

    async fn rebase(
        &self,
        cx: &CallContext,
        req: graft_proto::types::Rebase,
    ) -> Result<graft_model::ChangesetId, ApiError> {
        self.inner.rebase(cx, req).await
    }

    async fn revert(
        &self,
        cx: &CallContext,
        req: graft_proto::types::Revert,
    ) -> Result<graft_model::ChangesetId, ApiError> {
        self.inner.revert(cx, req).await
    }

    async fn land(
        &self,
        cx: &CallContext,
        req: graft_proto::types::Land,
    ) -> Result<graft_proto::types::Landed, ApiError> {
        self.inner.land(cx, req).await
    }

    async fn attest(
        &self,
        cx: &CallContext,
        req: graft_proto::types::Attest,
    ) -> Result<(), ApiError> {
        self.inner.attest(cx, req).await
    }

    async fn watch(
        &self,
        cx: &CallContext,
        req: graft_proto::types::Watch,
    ) -> Result<graft_proto::types::JournalPage, ApiError> {
        self.inner.watch(cx, req).await
    }
}

fn engine()
-> graft_engine::Engine<graft_store::memory::MemoryStore, graft_store::memory::MemoryStore> {
    let store = Arc::new(graft_store::memory::MemoryStore::new());
    graft_engine::Engine::new(
        store.clone(),
        store,
        Arc::new(graft_testkit::clock::FakeClock::at(1_000_000)),
        graft_engine::EngineConfig::default(),
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

fn git(dir: &Path, args: &[&str]) {
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
}

struct Tree {
    _tmp: tempfile::TempDir,
    git_dir: PathBuf,
    db: PathBuf,
}

fn tree() -> Tree {
    let tmp = tempfile::tempdir().unwrap();
    let work = tmp.path().join("work");
    std::fs::create_dir(&work).unwrap();
    git(&work, &["init", "-q"]);
    std::fs::write(work.join("f"), "hello\n").unwrap();
    git(&work, &["add", "f"]);
    git(&work, &["commit", "-q", "-m", "one"]);
    let db = tmp.path().join("mirror.db");
    Tree {
        git_dir: work.join(".git"),
        db,
        _tmp: tmp,
    }
}

#[tokio::test(flavor = "current_thread")]
async fn import_succeeds_after_unavailable() {
    let tree = tree();
    let api = Shedding {
        inner: engine(),
        fails: AtomicU32::new(3),
        calls: AtomicU32::new(0),
    };
    let clock = Clock {
        slept: Mutex::new(Vec::new()),
    };
    let db = MirrorDb::open(&tree.db).unwrap();
    let config = ImportConfig::new(&tree.git_dir, REPO);
    let out = import_repo_waiting(&api, &cx(), &db, &config, &clock)
        .await
        .unwrap();
    assert_eq!(out.report.commits, 1);
    assert_eq!(out.lines_moved, ["main"]);
    let slept = clock.slept.lock().unwrap().clone();
    assert_eq!(slept, vec![Duration::from_millis(1_000); 3]);
    assert!(api.calls.load(Ordering::SeqCst) > 3);
}

#[tokio::test(flavor = "current_thread")]
async fn permanent_unavailable_fails_the_import() {
    let tree = tree();
    let api = Shedding {
        inner: engine(),
        fails: AtomicU32::new(u32::MAX),
        calls: AtomicU32::new(0),
    };
    let clock = Clock {
        slept: Mutex::new(Vec::new()),
    };
    let db = MirrorDb::open(&tree.db).unwrap();
    let config = ImportConfig::new(&tree.git_dir, REPO);
    let err = import_repo_waiting(&api, &cx(), &db, &config, &clock)
        .await
        .unwrap_err();
    assert!(matches!(
        err,
        MirrorError::Api(ApiError::Unavailable {
            retry_after_ms: 1_000
        })
    ));
    let slept = clock.slept.lock().unwrap().clone();
    assert_eq!(slept.len(), 7);
    assert!(slept.iter().all(|d| *d == Duration::from_millis(1_000)));
    // create_repo is the first call, and it never gets through.
    assert_eq!(api.calls.load(Ordering::SeqCst), 8);
}
