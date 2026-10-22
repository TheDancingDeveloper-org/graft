//! The client's write path against an in-process server with workspaces
//! (G2.5): commit, amend, recovery after an interrupted commit, lease
//! heartbeats, sync and land (direct and through the PR bridge).

use std::collections::BTreeMap;
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use graft_client::fetch::FetchOptions;
use graft_client::workcopy::{
    CommitLimits, CommitOptions, DEFAULT_CACHE_BYTES, FileStatus, LandOptions, LandOutcome,
    Recovered, WcConfig, WorkingCopy, clone,
};
use graft_client::workspace::{DEFAULT_LEASE_MS, stack, with_heartbeat};
use graft_client::{Endpoint, Error, Remote, Sparse};
use graft_model::{Author, AuthorKind, BlobId, Change, ChangesetId, RepoPath, Timestamp};
use graft_proto::api::WorkspaceApi;
use graft_proto::types::{DraftState, SetHead, WorkspaceRef};
use graft_proto::{ApiError, CallContext};
use graft_testkit::fake_api::admin_cx;
use graft_testkit::server::{Rpc, SnapshotRepo, TestServer};

const REPO: &str = "r";

type Files = BTreeMap<String, Vec<u8>>;

fn cx() -> CallContext {
    admin_cx()
}

fn p(s: &str) -> RepoPath {
    RepoPath::new(s).unwrap()
}

fn files(pairs: &[(&str, &str)]) -> Files {
    pairs
        .iter()
        .map(|(k, v)| (k.to_string(), v.as_bytes().to_vec()))
        .collect()
}

fn base_files() -> Files {
    files(&[
        ("README.md", "hello\n"),
        ("src/lib.rs", "fn a() {}\n"),
        ("src/util.rs", "fn u() {}\n"),
        ("docs/guide.md", "guide\n"),
    ])
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

fn amend(message: Option<&str>) -> CommitOptions {
    CommitOptions {
        message: message.map(str::to_owned),
        amend: true,
        ..opts("unused")
    }
}

struct Env {
    repo: Arc<SnapshotRepo>,
    server: TestServer,
    remote: Remote,
    dir: tempfile::TempDir,
}

impl Env {
    async fn new(start: &Files) -> Env {
        let repo = Arc::new(SnapshotRepo::new());
        repo.commit("main", start).await;
        let server = TestServer::tcp_workspaces(repo.clone()).await;
        let remote = Remote::connect(Endpoint::parse(server.url()))
            .await
            .unwrap();
        Env {
            repo,
            server,
            remote,
            dir: tempfile::tempdir().unwrap(),
        }
    }

    /// A working copy of `main` holding `sparse`, with its workspace open.
    async fn clone(&self, name: &str, sparse: Sparse) -> WorkingCopy {
        let config = WcConfig {
            endpoint: None,
            repo: REPO.to_owned(),
            workspace: None,
        };
        let (mut wc, _) = clone(
            &self.remote,
            &cx(),
            &self.dir.path().join(name),
            config,
            "main",
            None,
            sparse,
            &FetchOptions::default(),
        )
        .await
        .unwrap();
        wc.begin(&self.remote, &cx(), DEFAULT_LEASE_MS)
            .await
            .unwrap();
        wc
    }

    fn reopen(&self, wc: &WorkingCopy) -> WorkingCopy {
        WorkingCopy::open(wc.root()).unwrap()
    }

    async fn commit(&self, wc: &mut WorkingCopy, o: &CommitOptions) -> Result<ChangesetId, Error> {
        wc.begin(&self.remote, &cx(), DEFAULT_LEASE_MS).await?;
        Ok(wc.commit(&self.remote, &cx(), o).await?.id)
    }

    fn workspace(&self, wc: &WorkingCopy) -> String {
        wc.config().workspace.clone().unwrap()
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

fn puts(server: &TestServer) -> Vec<usize> {
    server
        .calls()
        .into_iter()
        .filter_map(|c| match c {
            Rpc::PutObjects(n) => Some(n),
            _ => None,
        })
        .collect()
}

#[tokio::test]
async fn commit_uploads_only_the_blobs_the_server_lacks() {
    let env = Env::new(&base_files()).await;
    let mut wc = env.clone("wc", Sparse::full()).await;
    let ws = env.workspace(&wc);
    let base = wc.state().base;
    env.server.take_calls();

    // One new content, one content the server already has (a copy of a
    // file), one deletion.
    write(wc.root(), "src/new.rs", "fn new() {}\n");
    write(wc.root(), "src/copy.rs", "fn u() {}\n");
    fs::remove_file(wc.root().join("docs/guide.md")).unwrap();
    wc.begin(&env.remote, &cx(), DEFAULT_LEASE_MS)
        .await
        .unwrap();
    let done = wc.commit(&env.remote, &cx(), &opts("add")).await.unwrap();

    assert_eq!(done.upload.blobs, 2);
    assert_eq!(done.upload.uploaded, 1);
    assert_eq!(puts(&env.server), vec![1]);
    let has: Vec<Vec<graft_model::ObjectId>> = env
        .server
        .calls()
        .into_iter()
        .filter_map(|c| match c {
            Rpc::HasObjects(ids) => Some(ids),
            _ => None,
        })
        .collect();
    assert_eq!(has.len(), 1);
    assert_eq!(has[0].len(), 2);
    assert_eq!(
        done.changes,
        vec![
            Change::Delete {
                path: p("docs/guide.md")
            },
            Change::Put {
                path: p("src/copy.rs"),
                kind: graft_model::EntryKind::File,
                id: BlobId::hash(b"fn u() {}\n").0
            },
            Change::Put {
                path: p("src/new.rs"),
                kind: graft_model::EntryKind::File,
                id: BlobId::hash(b"fn new() {}\n").0
            },
        ]
    );
    assert_eq!(done.parent, base);

    // Both sides agree: the server head is the draft, the working copy is
    // clean against it, and nothing is pending.
    let view = env.repo.workspace(&ws).unwrap();
    assert_eq!(view.head, Some(done.id));
    assert_eq!(env.repo.draft_state(done.id).0, DraftState::Draft);
    let wc = env.reopen(&wc);
    assert_eq!(wc.state().head, Some(done.id));
    assert_eq!(wc.state().base, base);
    assert!(wc.status().unwrap().is_empty());
    assert!(!wc.root().join(".graft/pending").exists());

    // A second commit stacks on the first.
    let mut wc = wc;
    write(wc.root(), "src/new.rs", "fn new() { 2 }\n");
    let second = env.commit(&mut wc, &opts("more")).await.unwrap();
    let drafts = stack(&env.remote, &cx(), REPO, base, Some(second))
        .await
        .unwrap();
    let ids: Vec<ChangesetId> = drafts.iter().map(|v| v.changeset.id()).collect();
    assert_eq!(ids, vec![done.id, second]);
}

#[tokio::test]
async fn nothing_to_commit_sends_nothing() {
    let env = Env::new(&base_files()).await;
    let mut wc = env.clone("wc", Sparse::full()).await;
    env.server.take_calls();
    assert!(matches!(
        wc.commit(&env.remote, &cx(), &opts("none")).await,
        Err(Error::NothingToCommit)
    ));
    // Rewriting a file with the same bytes is not a change either.
    write(wc.root(), "README.md", "hello\n");
    assert!(matches!(
        wc.commit(&env.remote, &cx(), &opts("none")).await,
        Err(Error::NothingToCommit)
    ));
    assert!(env.server.calls().is_empty());
}

#[tokio::test]
async fn committing_in_a_sparse_slice_never_touches_paths_outside_it() {
    let env = Env::new(&base_files()).await;
    let mut wc = env.clone("wc", Sparse::new([p("src")], false)).await;
    assert!(!wc.root().join("docs").exists());

    // Files outside the slice: one the slice never had, one shadowing a
    // real path, both unreadable, so reading either would fail the commit.
    write(wc.root(), "docs/guide.md", "local edit outside\n");
    write(wc.root(), "README.md", "outside too\n");
    for f in ["docs/guide.md", "README.md"] {
        fs::set_permissions(wc.root().join(f), fs::Permissions::from_mode(0o000)).unwrap();
    }
    write(wc.root(), "src/lib.rs", "fn a() { changed }\n");
    let id = env.commit(&mut wc, &opts("slice")).await.unwrap();

    let c = env.repo.fake().changeset(id).unwrap();
    let paths: Vec<&str> = c.changes.iter().map(|c| c.path().as_str()).collect();
    assert_eq!(paths, vec!["src/lib.rs"]);
    // The snapshot outside the slice is the base's.
    let snap = env.repo.fake().snapshot(c.root);
    assert_eq!(
        snap["docs/guide.md"].1,
        BlobId::hash(b"guide\n").0,
        "outside the slice is untouched"
    );
    assert_eq!(snap["README.md"].1, BlobId::hash(b"hello\n").0);
}

#[tokio::test]
async fn amend_supersedes_the_old_head() {
    let env = Env::new(&base_files()).await;
    let mut wc = env.clone("wc", Sparse::full()).await;
    let ws = env.workspace(&wc);
    let base = wc.state().base;

    write(wc.root(), "src/lib.rs", "fn a() { 1 }\n");
    write(wc.root(), "src/extra.rs", "extra\n");
    let first = env.commit(&mut wc, &opts("first")).await.unwrap();

    // Amend: change one path of the old head again, put another back as the
    // base had it, and add a new one.
    write(wc.root(), "src/lib.rs", "fn a() { 2 }\n");
    fs::remove_file(wc.root().join("src/extra.rs")).unwrap();
    write(wc.root(), "src/other.rs", "other\n");
    wc.begin(&env.remote, &cx(), DEFAULT_LEASE_MS)
        .await
        .unwrap();
    let done = wc.commit(&env.remote, &cx(), &amend(None)).await.unwrap();

    assert_eq!(done.superseded, Some(first));
    assert_eq!(done.parent, base);
    let paths: Vec<&str> = done.changes.iter().map(|c| c.path().as_str()).collect();
    assert_eq!(paths, vec!["src/lib.rs", "src/other.rs"]);
    let c = env.repo.fake().changeset(done.id).unwrap();
    assert_eq!(c.parents, vec![base]);
    assert_eq!(c.message, "first", "the old message is kept");

    assert_eq!(
        env.repo.draft_state(first),
        (DraftState::Superseded, Some(done.id))
    );
    assert_eq!(env.repo.draft_state(done.id).0, DraftState::Draft);
    assert_eq!(env.repo.workspace(&ws).unwrap().head, Some(done.id));
    let wc = env.reopen(&wc);
    assert_eq!(wc.state().head, Some(done.id));
    assert!(wc.status().unwrap().is_empty());

    // Amending away every change leaves nothing to commit.
    let mut wc = wc;
    write(wc.root(), "src/lib.rs", "fn a() {}\n");
    fs::remove_file(wc.root().join("src/other.rs")).unwrap();
    assert!(matches!(
        env.commit(&mut wc, &amend(Some("x"))).await,
        Err(Error::NothingToCommit)
    ));
}

#[tokio::test]
async fn amend_without_a_draft_is_refused() {
    let env = Env::new(&base_files()).await;
    let mut wc = env.clone("wc", Sparse::full()).await;
    write(wc.root(), "src/lib.rs", "x\n");
    assert!(matches!(
        env.commit(&mut wc, &amend(Some("m"))).await,
        Err(Error::NoDraft)
    ));
}

#[tokio::test]
async fn an_interrupted_commit_is_finished_by_the_next_command() {
    let env = Env::new(&base_files()).await;
    let mut wc = env.clone("wc", Sparse::full()).await;
    let ws = env.workspace(&wc);
    write(wc.root(), "src/lib.rs", "fn a() { 1 }\n");

    // The server creates the draft, then the connection fails before the
    // client hears back: the local side has not moved.
    env.repo
        .fail_next("GetChangesets", ApiError::Unavailable { retry_after_ms: 0 });
    wc.begin(&env.remote, &cx(), DEFAULT_LEASE_MS)
        .await
        .unwrap();
    let err = wc.commit(&env.remote, &cx(), &opts("one")).await;
    assert!(matches!(err, Err(Error::Api(ApiError::Unavailable { .. }))));
    let server_head = env.repo.workspace(&ws).unwrap().head;
    assert!(server_head.is_some());
    let mut wc = env.reopen(&wc);
    assert_eq!(wc.state().head, None);
    assert!(wc.root().join(".graft/pending").exists());

    // The next command finishes it, and the two sides agree.
    let begun = wc
        .begin(&env.remote, &cx(), DEFAULT_LEASE_MS)
        .await
        .unwrap();
    assert_eq!(
        begun.recovered,
        Some(Recovered::Finished(server_head.unwrap()))
    );
    assert_eq!(wc.state().head, server_head);
    assert!(wc.status().unwrap().is_empty());
    assert!(!wc.root().join(".graft/pending").exists());
}

#[tokio::test]
async fn an_interrupted_commit_that_cannot_succeed_rolls_back() {
    let env = Env::new(&base_files()).await;
    let mut wc = env.clone("wc", Sparse::full()).await;
    write(wc.root(), "src/lib.rs", "fn a() { 1 }\n");

    // The create never reached the server.
    env.repo.fail_next(
        "CreateChangeset",
        ApiError::Unavailable { retry_after_ms: 0 },
    );
    wc.begin(&env.remote, &cx(), DEFAULT_LEASE_MS)
        .await
        .unwrap();
    assert!(wc.commit(&env.remote, &cx(), &opts("one")).await.is_err());
    assert!(wc.root().join(".graft/pending").exists());

    // When it is sent again the server refuses it for good: dropped.
    env.repo.fail_next(
        "CreateChangeset",
        ApiError::InvalidChange {
            path: String::new(),
            rule: "test".into(),
        },
    );
    let mut wc = env.reopen(&wc);
    let begun = wc
        .begin(&env.remote, &cx(), DEFAULT_LEASE_MS)
        .await
        .unwrap();
    assert_eq!(begun.recovered, Some(Recovered::RolledBack));
    assert_eq!(wc.state().head, None);
    assert!(!wc.root().join(".graft/pending").exists());
    assert_eq!(
        wc.status().unwrap(),
        vec![FileStatus::Modified(p("src/lib.rs"))]
    );

    // A pending file cut short by a crash is dropped too.
    fs::write(
        wc.root().join(".graft/pending"),
        "graft-pending 1\nworkspace x\n",
    )
    .unwrap();
    let begun = wc
        .begin(&env.remote, &cx(), DEFAULT_LEASE_MS)
        .await
        .unwrap();
    assert_eq!(begun.recovered, Some(Recovered::RolledBack));

    // And the commit then goes through normally.
    let id = wc
        .commit(&env.remote, &cx(), &opts("one"))
        .await
        .unwrap()
        .id;
    assert_eq!(wc.state().head, Some(id));
}

#[tokio::test]
async fn a_failed_upload_changes_nothing_and_a_retry_completes_it() {
    let env = Env::new(&base_files()).await;
    let mut wc = env.clone("wc", Sparse::full()).await;
    write(wc.root(), "a.txt", "a\n");
    write(wc.root(), "b.txt", "b\n");
    let mut o = opts("two");
    // One blob per batch: the first batch lands, the second fails.
    o.limits.put_batch_bytes = 1;
    env.repo
        .fail_next("PutObjects", ApiError::Unavailable { retry_after_ms: 0 });
    env.server.take_calls();
    wc.begin(&env.remote, &cx(), DEFAULT_LEASE_MS)
        .await
        .unwrap();
    assert!(wc.commit(&env.remote, &cx(), &o).await.is_err());
    assert!(!wc.root().join(".graft/pending").exists());
    assert_eq!(wc.state().head, None);

    env.server.take_calls();
    let done = wc.commit(&env.remote, &cx(), &o).await.unwrap();
    // The fake failed the first call before storing anything, so both
    // are sent again; an upload is idempotent either way.
    assert_eq!(done.upload.uploaded, 2);
    assert_eq!(puts(&env.server), vec![1, 1]);
}

#[tokio::test]
async fn reserved_names_huge_files_and_special_files_are_refused() {
    let env = Env::new(&base_files()).await;
    let mut wc = env.clone("wc", Sparse::full()).await;
    env.server.take_calls();

    for name in [".GRAFT/config", ".Graft/x", "\u{FF0E}graft/x"] {
        write(wc.root(), name, "evil\n");
        let err = env.commit(&mut wc, &opts("evil")).await;
        assert!(
            matches!(err, Err(Error::Api(ApiError::InvalidPath { .. }))),
            "{name}: {err:?}"
        );
        fs::remove_dir_all(wc.root().join(name.split('/').next().unwrap())).unwrap();
    }

    write(wc.root(), "big.bin", &"x".repeat(100));
    let mut o = opts("big");
    o.limits.max_blob_bytes = 64;
    let err = env.commit(&mut wc, &o).await;
    assert!(matches!(
        err,
        Err(Error::Api(ApiError::LimitExceeded { .. }))
    ));
    assert!(puts(&env.server).is_empty());
    assert!(!env.server.calls().contains(&Rpc::Repo("CreateChangeset")));
}

#[tokio::test]
async fn a_symlink_out_of_the_working_copy_is_committed_as_a_link_not_followed() {
    let env = Env::new(&base_files()).await;
    let mut wc = env.clone("wc", Sparse::full()).await;
    let outside = tempfile::tempdir().unwrap();
    fs::write(outside.path().join("secret"), "do not upload\n").unwrap();
    std::os::unix::fs::symlink(outside.path(), wc.root().join("escape")).unwrap();
    let id = env.commit(&mut wc, &opts("link")).await.unwrap();

    let c = env.repo.fake().changeset(id).unwrap();
    assert_eq!(c.changes.len(), 1);
    match &c.changes[0] {
        Change::Put { path, kind, id } => {
            assert_eq!(path.as_str(), "escape");
            assert_eq!(*kind, graft_model::EntryKind::Symlink);
            let target = outside.path().as_os_str().to_str().unwrap();
            assert_eq!(*id, BlobId::hash(target.as_bytes()).0);
        }
        other => panic!("{other:?}"),
    }
    assert!(
        env.repo
            .fake()
            .blob(BlobId::hash(b"do not upload\n"))
            .is_none()
    );
}

#[tokio::test]
async fn a_lapsed_lease_reopens_the_workspace_and_adopts_its_drafts() {
    let env = Env::new(&base_files()).await;
    let mut wc = env.clone("wc", Sparse::full()).await;
    let ws = env.workspace(&wc);
    write(wc.root(), "src/lib.rs", "fn a() { 1 }\n");
    let first = env.commit(&mut wc, &opts("one")).await.unwrap();

    env.repo.expire_workspace(&ws);
    let begun = wc
        .begin(&env.remote, &cx(), DEFAULT_LEASE_MS)
        .await
        .unwrap();
    assert!(begun.opened);
    assert_eq!(begun.workspace.name, ws);
    assert_eq!(begun.workspace.head, Some(first));
    assert_eq!(env.repo.draft_state(first).0, DraftState::Draft);
}

#[tokio::test]
async fn a_head_moved_elsewhere_is_reported_not_overwritten() {
    let env = Env::new(&base_files()).await;
    let mut wc = env.clone("wc", Sparse::full()).await;
    let ws = env.workspace(&wc);
    write(wc.root(), "src/lib.rs", "fn a() { 1 }\n");
    let first = env.commit(&mut wc, &opts("one")).await.unwrap();
    env.remote
        .set_head(
            &cx(),
            SetHead {
                workspace: WorkspaceRef {
                    repo: REPO.into(),
                    name: ws.clone(),
                },
                expected: Some(first),
                new: None,
            },
        )
        .await
        .unwrap();
    write(wc.root(), "src/lib.rs", "fn a() { 2 }\n");
    let err = env.commit(&mut wc, &opts("two")).await;
    assert!(matches!(
        err,
        Err(Error::Diverged { server: None, local: Some(l), .. }) if l == first
    ));
}

#[tokio::test]
async fn heartbeats_renew_the_lease_while_work_runs() {
    let env = Env::new(&base_files()).await;
    let wc = env.clone("wc", Sparse::full()).await;
    let ws = WorkspaceRef {
        repo: REPO.into(),
        name: env.workspace(&wc),
    };
    env.server.take_calls();
    let out = with_heartbeat(
        &env.remote,
        &cx(),
        ws,
        DEFAULT_LEASE_MS,
        Duration::from_millis(40),
        async {
            tokio::time::sleep(Duration::from_millis(300)).await;
            7
        },
    )
    .await;
    assert_eq!(out, 7);
    let renewals = env
        .server
        .calls()
        .iter()
        .filter(|c| **c == Rpc::Workspace("RenewLease"))
        .count();
    assert!(renewals >= 4, "{renewals} renewals");
}

#[tokio::test]
async fn sync_fast_forwards_a_working_copy_with_no_stack() {
    let env = Env::new(&base_files()).await;
    let mut wc = env.clone("wc", Sparse::full()).await;
    let ws = env.workspace(&wc);
    let mut next = base_files();
    next.insert("src/lib.rs".into(), b"fn a() { theirs }\n".to_vec());
    let tip = env.repo.commit("main", &next).await;

    // A local change on the path the line changed: refused, nothing moves.
    write(wc.root(), "src/lib.rs", "mine\n");
    let cache = wc.open_cache(DEFAULT_CACHE_BYTES).unwrap();
    wc.begin(&env.remote, &cx(), DEFAULT_LEASE_MS)
        .await
        .unwrap();
    let err = wc
        .sync(
            &env.remote,
            &cache,
            &cx(),
            DEFAULT_LEASE_MS,
            &FetchOptions::default(),
        )
        .await;
    assert!(matches!(err, Err(Error::Dirty(_))));
    assert_ne!(env.repo.workspace(&ws).unwrap().base, tip);
    assert_eq!(read(wc.root(), "src/lib.rs"), "mine\n");

    write(wc.root(), "src/lib.rs", "fn a() {}\n");
    let out = wc
        .sync(
            &env.remote,
            &cache,
            &cx(),
            DEFAULT_LEASE_MS,
            &FetchOptions::default(),
        )
        .await
        .unwrap();
    assert_eq!((out.base, out.head), (tip, None));
    assert_eq!(read(wc.root(), "src/lib.rs"), "fn a() { theirs }\n");
    assert_eq!(env.repo.workspace(&ws).unwrap().base, tip);
    let wc = env.reopen(&wc);
    assert_eq!(wc.state().base, tip);
    assert!(wc.status().unwrap().is_empty());
}

#[tokio::test]
async fn sync_with_a_stack_rebases_on_the_server_or_changes_nothing() {
    let env = Env::new(&base_files()).await;
    let mut wc = env.clone("wc", Sparse::full()).await;
    let ws = env.workspace(&wc);
    write(wc.root(), "src/util.rs", "fn u() { mine }\n");
    let draft = env.commit(&mut wc, &opts("mine")).await.unwrap();

    // The line changes another file: the stack rebases, and the working
    // copy gets their change and keeps ours.
    let mut next = base_files();
    next.insert("docs/guide.md".into(), b"guide 2\n".to_vec());
    let tip = env.repo.commit("main", &next).await;
    let cache = wc.open_cache(DEFAULT_CACHE_BYTES).unwrap();
    wc.begin(&env.remote, &cx(), DEFAULT_LEASE_MS)
        .await
        .unwrap();
    let out = wc
        .sync(
            &env.remote,
            &cache,
            &cx(),
            DEFAULT_LEASE_MS,
            &FetchOptions::default(),
        )
        .await
        .unwrap();
    assert_eq!(out.base, tip);
    let head = out.head.unwrap();
    assert_ne!(head, draft);
    assert_eq!(env.repo.workspace(&ws).unwrap().head, Some(head));
    assert_eq!(read(wc.root(), "docs/guide.md"), "guide 2\n");
    assert_eq!(read(wc.root(), "src/util.rs"), "fn u() { mine }\n");
    assert!(wc.status().unwrap().is_empty());

    // The line changes our file: a conflict, and nothing moves.
    next.insert("src/util.rs".into(), b"fn u() { theirs }\n".to_vec());
    env.repo.commit("main", &next).await;
    wc.begin(&env.remote, &cx(), DEFAULT_LEASE_MS)
        .await
        .unwrap();
    let err = wc
        .sync(
            &env.remote,
            &cache,
            &cx(),
            DEFAULT_LEASE_MS,
            &FetchOptions::default(),
        )
        .await;
    assert!(matches!(err, Err(Error::Api(ApiError::Conflict { .. }))));
    assert_eq!(wc.state().head, Some(head));
    assert_eq!(read(wc.root(), "src/util.rs"), "fn u() { mine }\n");
}

#[tokio::test]
async fn land_on_a_direct_line_moves_the_working_copy_to_the_landed_change() {
    let env = Env::new(&base_files()).await;
    let mut wc = env.clone("wc", Sparse::full()).await;
    let ws = env.workspace(&wc);
    write(wc.root(), "src/lib.rs", "fn a() { landed }\n");
    let draft = env.commit(&mut wc, &opts("land me")).await.unwrap();

    // Two contended answers first: retried.
    for _ in 0..2 {
        env.repo.fail_next(
            "Land",
            ApiError::Contended {
                line: "main".into(),
                tip: draft,
            },
        );
    }
    let cache = wc.open_cache(DEFAULT_CACHE_BYTES).unwrap();
    let lo = LandOptions {
        poll: Duration::from_millis(5),
        ..LandOptions::default()
    };
    wc.begin(&env.remote, &cx(), DEFAULT_LEASE_MS)
        .await
        .unwrap();
    let out = wc
        .land(&env.remote, &cache, &cx(), &lo, &FetchOptions::default())
        .await
        .unwrap();
    assert!(matches!(out, LandOutcome::Landed { landed, .. } if landed == draft));
    assert_eq!(env.repo.tip("main"), Some(draft));
    assert_eq!(env.repo.draft_state(draft).0, DraftState::Landed);
    assert_eq!((wc.state().base, wc.state().head), (draft, None));
    let view = env.repo.workspace(&ws).unwrap();
    assert_eq!((view.base, view.head), (draft, None));

    // Nothing left to land.
    assert!(matches!(
        wc.land(&env.remote, &cache, &cx(), &lo, &FetchOptions::default())
            .await,
        Err(Error::NoDraft)
    ));
}

#[tokio::test]
async fn contention_past_the_wait_is_reported() {
    let env = Env::new(&base_files()).await;
    let mut wc = env.clone("wc", Sparse::full()).await;
    write(wc.root(), "src/lib.rs", "x\n");
    let draft = env.commit(&mut wc, &opts("m")).await.unwrap();
    for _ in 0..50 {
        env.repo.fail_next(
            "Land",
            ApiError::Contended {
                line: "main".into(),
                tip: draft,
            },
        );
    }
    let cache = wc.open_cache(DEFAULT_CACHE_BYTES).unwrap();
    let lo = LandOptions {
        contended_wait: Duration::from_millis(50),
        poll: Duration::from_millis(5),
        ..LandOptions::default()
    };
    let err = wc
        .land(&env.remote, &cache, &cx(), &lo, &FetchOptions::default())
        .await;
    assert!(matches!(err, Err(Error::Api(ApiError::Contended { .. }))));
    assert_eq!(wc.state().head, Some(draft));
}

#[tokio::test]
async fn land_through_the_pr_bridge_queues_then_waits_for_the_merge() {
    let env = Env::new(&base_files()).await;
    env.repo
        .set_git_primary("main", "https://github.example/o/r");
    let mut wc = env.clone("wc", Sparse::full()).await;
    let ws = env.workspace(&wc);
    write(wc.root(), "src/lib.rs", "fn a() { pr }\n");
    let draft = env.commit(&mut wc, &opts("via PR")).await.unwrap();
    let cache = wc.open_cache(DEFAULT_CACHE_BYTES).unwrap();

    // Without a wait: queued, and nothing local moves.
    let out = wc
        .land(
            &env.remote,
            &cache,
            &cx(),
            &LandOptions::default(),
            &FetchOptions::default(),
        )
        .await
        .unwrap();
    let url = "https://github.example/o/r/pull/1";
    assert_eq!(out, LandOutcome::Queued { pr_url: url.into() });
    assert_eq!(wc.state().head, Some(draft));

    // Someone else lands meanwhile, so the PR merges as a squash on top.
    let mut theirs = base_files();
    theirs.insert("docs/guide.md".into(), b"theirs\n".to_vec());
    env.repo.commit("main", &theirs).await;

    // With a wait: asked again until the merge.
    let repo = env.repo.clone();
    let merger = tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(150)).await;
        repo.merge_pr("https://github.example/o/r/pull/1").await
    });
    let lo = LandOptions {
        wait: Some(Duration::from_secs(10)),
        poll: Duration::from_millis(20),
        ..LandOptions::default()
    };
    let out = wc
        .land(&env.remote, &cache, &cx(), &lo, &FetchOptions::default())
        .await
        .unwrap();
    let merged = merger.await.unwrap();
    assert!(matches!(out, LandOutcome::Landed { landed, rebased: true, .. } if landed == merged));
    assert_eq!(
        env.repo.pr_urls(),
        vec![url.to_owned()],
        "one PR, asked again"
    );
    assert_eq!((wc.state().base, wc.state().head), (merged, None));
    assert_eq!(read(wc.root(), "docs/guide.md"), "theirs\n");
    assert_eq!(read(wc.root(), "src/lib.rs"), "fn a() { pr }\n");
    assert!(wc.status().unwrap().is_empty());
    assert_eq!(env.repo.workspace(&ws).unwrap().base, merged);
}

#[tokio::test]
async fn a_closed_pr_is_rejected_and_an_open_one_times_out() {
    let env = Env::new(&base_files()).await;
    env.repo
        .set_git_primary("main", "https://github.example/o/r");
    let mut wc = env.clone("wc", Sparse::full()).await;
    write(wc.root(), "src/lib.rs", "x\n");
    env.commit(&mut wc, &opts("m")).await.unwrap();
    let cache = wc.open_cache(DEFAULT_CACHE_BYTES).unwrap();
    let lo = LandOptions {
        wait: Some(Duration::from_millis(100)),
        poll: Duration::from_millis(20),
        ..LandOptions::default()
    };
    let err = wc
        .land(&env.remote, &cache, &cx(), &lo, &FetchOptions::default())
        .await;
    assert!(matches!(err, Err(Error::StillQueued { .. })));

    env.repo.close_pr("https://github.example/o/r/pull/1");
    let err = wc
        .land(&env.remote, &cache, &cx(), &lo, &FetchOptions::default())
        .await;
    assert!(matches!(err, Err(Error::Api(ApiError::Rejected { .. }))));
}

#[tokio::test]
async fn a_landed_stack_with_local_changes_in_the_way_finishes_on_the_next_land() {
    let env = Env::new(&base_files()).await;
    env.repo
        .set_git_primary("main", "https://github.example/o/r");
    let mut wc = env.clone("wc", Sparse::full()).await;
    write(wc.root(), "src/lib.rs", "fn a() { pr }\n");
    env.commit(&mut wc, &opts("m")).await.unwrap();
    let cache = wc.open_cache(DEFAULT_CACHE_BYTES).unwrap();
    let lo = LandOptions::default();
    wc.land(&env.remote, &cache, &cx(), &lo, &FetchOptions::default())
        .await
        .unwrap();
    let mut theirs = base_files();
    theirs.insert("docs/guide.md".into(), b"theirs\n".to_vec());
    env.repo.commit("main", &theirs).await;
    let merged = env.repo.merge_pr("https://github.example/o/r/pull/1").await;

    write(wc.root(), "docs/guide.md", "my local edit\n");
    let err = wc
        .land(&env.remote, &cache, &cx(), &lo, &FetchOptions::default())
        .await;
    assert!(matches!(err, Err(Error::LandedDirty { landed, .. }) if landed == merged));

    write(wc.root(), "docs/guide.md", "guide\n");
    let out = wc
        .land(&env.remote, &cache, &cx(), &lo, &FetchOptions::default())
        .await
        .unwrap();
    assert!(matches!(out, LandOutcome::Landed { landed, .. } if landed == merged));
    assert_eq!(read(wc.root(), "docs/guide.md"), "theirs\n");
}
