//! Native land, conflicts, revert and stack operations (G3.5) against the
//! real engine (G3.1) on the memory store, called in process: races with
//! other writers, lost replies, and moves interrupted half way.
//!
//! The property test at the end drives random race outcomes through `land`
//! and checks that the working copy is never left on a base the line does
//! not hold, and that `land` and `sync` always carry it forward to the
//! landed change.

#[path = "support/engine.rs"]
mod engine;

use std::fs;
use std::sync::Arc;

use engine::{EngineRepo, Fault, Race};
use graft_client::fetch::FetchOptions;
use graft_client::workcopy::{
    CommitLimits, CommitOptions, ConflictFile, ConflictMode, DEFAULT_CACHE_BYTES, LandOptions,
    LandOutcome, WcConfig, WorkingCopy, clone, sidecar_path,
};
use graft_client::workspace::{DEFAULT_LEASE_MS, stack};
use graft_client::{Error, Sparse};
use graft_model::{Author, AuthorKind, ChangesetId, RepoPath, Timestamp};
use graft_proto::types::{MergeMode, StackOp};
use graft_proto::{ApiError, CallContext};
use proptest::prelude::*;

const REPO: &str = "demo";

const LIB: &str = "fn a() {}\nfn b() {}\nfn c() {}\n";

fn base_files() -> Vec<(&'static str, &'static [u8])> {
    vec![
        ("README.md", b"hello\n"),
        ("src/lib.rs", LIB.as_bytes()),
        ("assets/logo.bin", &[0xff, 0x00, 0x7f]),
        ("docs/guide.md", b"guide\n"),
    ]
}

fn p(s: &str) -> RepoPath {
    RepoPath::new(s).unwrap()
}

fn put(path: &str, data: &str) -> Race {
    vec![(path.to_owned(), Some(data.as_bytes().to_vec()))]
}

fn opts(message: Option<&str>) -> CommitOptions {
    CommitOptions {
        message: message.map(str::to_owned),
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

fn quick() -> LandOptions {
    LandOptions {
        contended_wait: std::time::Duration::ZERO,
        ..LandOptions::default()
    }
}

struct Env {
    repo: Arc<EngineRepo>,
    dir: tempfile::TempDir,
}

impl Env {
    /// `main` attested on `ci/build` and `ci/test` (decision D6).
    async fn d6() -> Env {
        Env {
            repo: EngineRepo::new(REPO, &base_files()).await,
            dir: tempfile::tempdir().unwrap(),
        }
    }

    async fn direct() -> Env {
        Env {
            repo: EngineRepo::direct(REPO, &base_files()).await,
            dir: tempfile::tempdir().unwrap(),
        }
    }

    fn cx(&self) -> CallContext {
        self.repo.cx()
    }

    async fn clone(&self, name: &str) -> WorkingCopy {
        self.clone_in(name, None).await
    }

    /// A clone in its own named workspace, for a second writer.
    async fn clone_in(&self, name: &str, workspace: Option<&str>) -> WorkingCopy {
        let config = WcConfig {
            endpoint: None,
            repo: REPO.to_owned(),
            workspace: workspace.map(str::to_owned),
        };
        let (mut wc, _) = clone(
            &*self.repo,
            &self.cx(),
            &self.dir.path().join(name),
            config,
            "main",
            None,
            Sparse::full(),
            &FetchOptions::default(),
        )
        .await
        .unwrap();
        wc.begin(&*self.repo, &self.cx(), DEFAULT_LEASE_MS)
            .await
            .unwrap();
        wc
    }

    async fn commit(
        &self,
        wc: &mut WorkingCopy,
        message: Option<&str>,
    ) -> Result<ChangesetId, Error> {
        let begun = wc.begin(&*self.repo, &self.cx(), DEFAULT_LEASE_MS).await?;
        if begun.unfinished {
            let cache = wc.open_cache(DEFAULT_CACHE_BYTES)?;
            wc.finish(
                &*self.repo,
                &cache,
                &self.cx(),
                DEFAULT_LEASE_MS,
                &FetchOptions::default(),
            )
            .await?;
        }
        Ok(wc.commit(&*self.repo, &self.cx(), &opts(message)).await?.id)
    }

    async fn land_with(&self, wc: &mut WorkingCopy, o: &LandOptions) -> Result<LandOutcome, Error> {
        wc.begin(&*self.repo, &self.cx(), DEFAULT_LEASE_MS).await?;
        let cache = wc.open_cache(DEFAULT_CACHE_BYTES)?;
        wc.land(&*self.repo, &cache, &self.cx(), o, &FetchOptions::default())
            .await
    }

    async fn land(&self, wc: &mut WorkingCopy) -> Result<LandOutcome, Error> {
        self.land_with(wc, &quick()).await
    }

    async fn sync(&self, wc: &mut WorkingCopy) -> Result<(), Error> {
        wc.begin(&*self.repo, &self.cx(), DEFAULT_LEASE_MS).await?;
        let cache = wc.open_cache(DEFAULT_CACHE_BYTES)?;
        wc.sync_writing_conflicts(
            &*self.repo,
            &cache,
            &self.cx(),
            DEFAULT_LEASE_MS,
            &FetchOptions::default(),
        )
        .await
        .map(|_| ())
    }

    async fn rewrite(&self, wc: &mut WorkingCopy, op: StackOp) -> Result<(), Error> {
        wc.begin(&*self.repo, &self.cx(), DEFAULT_LEASE_MS).await?;
        let cache = wc.open_cache(DEFAULT_CACHE_BYTES)?;
        wc.rewrite_stack(
            &*self.repo,
            &cache,
            &self.cx(),
            op,
            DEFAULT_LEASE_MS,
            &FetchOptions::default(),
        )
        .await
        .map(|_| ())
    }

    async fn revert(
        &self,
        wc: &mut WorkingCopy,
        id: ChangesetId,
    ) -> Result<graft_client::workcopy::Reverted, Error> {
        wc.begin(&*self.repo, &self.cx(), DEFAULT_LEASE_MS).await?;
        let cache = wc.open_cache(DEFAULT_CACHE_BYTES)?;
        wc.revert(
            &*self.repo,
            &cache,
            &self.cx(),
            id,
            DEFAULT_LEASE_MS,
            &FetchOptions::default(),
        )
        .await
    }

    async fn drafts(&self, wc: &WorkingCopy) -> Vec<String> {
        stack(
            &*self.repo,
            &self.cx(),
            REPO,
            wc.state().base,
            wc.state().head,
        )
        .await
        .unwrap()
        .into_iter()
        .map(|v| v.changeset.message)
        .collect()
    }

    /// The working files equal the snapshot the working copy holds.
    async fn assert_checked_out(&self, wc: &WorkingCopy) {
        let want = self.repo.files(wc.snapshot()).await;
        for (path, data) in &want {
            assert_eq!(
                &fs::read(wc.root().join(path)).unwrap(),
                data,
                "{path} differs from the snapshot"
            );
        }
        assert!(wc.status().unwrap().is_empty(), "{:?}", wc.status());
    }
}

fn read(wc: &WorkingCopy, path: &str) -> String {
    fs::read_to_string(wc.root().join(path)).unwrap()
}

fn write(wc: &WorkingCopy, path: &str, data: &str) {
    fs::write(wc.root().join(path), data).unwrap();
}

fn landed(out: LandOutcome) -> (ChangesetId, bool) {
    match out {
        LandOutcome::Landed {
            landed, rebased, ..
        } => (landed, rebased),
        other => panic!("not landed: {other:?}"),
    }
}

#[tokio::test]
async fn an_attested_stack_lands_and_the_working_copy_follows() {
    let env = Env::d6().await;
    let mut wc = env.clone("a").await;
    write(&wc, "README.md", "mine\n");
    let head = env.commit(&mut wc, Some("mine")).await.unwrap();

    // D6: no attestation, no land; nothing moves.
    let err = env.land(&mut wc).await.unwrap_err();
    assert!(
        matches!(&err, Error::Api(ApiError::Rejected { policy, .. }) if policy == "attested"),
        "{err:?}"
    );
    assert_eq!(wc.state().head, Some(head));

    env.repo.attest_all(head).await;
    let (id, rebased) = landed(env.land(&mut wc).await.unwrap());
    assert_eq!((id, rebased), (head, false));
    assert_eq!(env.repo.tip().await, head);
    assert_eq!(wc.state().base, head);
    assert_eq!(wc.state().head, None);
    assert_eq!(wc.state().moving, None);
    env.assert_checked_out(&wc).await;
}

#[tokio::test]
async fn a_lost_race_rebases_on_the_server_and_brings_in_the_other_change() {
    let env = Env::d6().await;
    let mut wc = env.clone("a").await;
    write(&wc, "README.md", "mine\n");
    let head = env.commit(&mut wc, Some("mine")).await.unwrap();
    env.repo.attest_all(head).await;
    env.repo.race(put("docs/guide.md", "theirs\n"));

    let (id, rebased) = landed(env.land(&mut wc).await.unwrap());
    assert!(rebased);
    assert_ne!(id, head);
    assert_eq!(env.repo.tip().await, id);
    assert_eq!(wc.state().base, id);
    assert_eq!(read(&wc, "docs/guide.md"), "theirs\n");
    assert_eq!(read(&wc, "README.md"), "mine\n");
    env.assert_checked_out(&wc).await;
}

#[tokio::test]
async fn a_lost_land_reply_is_safe_to_retry() {
    let env = Env::direct().await;
    let mut wc = env.clone("a").await;
    write(&wc, "README.md", "mine\n");
    let head = env.commit(&mut wc, Some("mine")).await.unwrap();
    env.repo.fail(
        "Land",
        Fault::After(ApiError::Unavailable { retry_after_ms: 0 }),
    );

    let err = env.land(&mut wc).await.unwrap_err();
    assert!(matches!(err, Error::Api(ApiError::Unavailable { .. })));
    // The line moved; the working copy did not, and is still on the line.
    assert_eq!(env.repo.tip().await, head);
    assert_eq!(wc.state().head, Some(head));
    assert!(env.repo.on_main(wc.state().base).await);

    let (id, _) = landed(env.land(&mut wc).await.unwrap());
    assert_eq!(id, head);
    assert_eq!(wc.state().base, head);
    env.assert_checked_out(&wc).await;
}

#[tokio::test]
async fn a_land_interrupted_after_the_line_moved_is_finished_by_the_next_command() {
    let env = Env::direct().await;
    let mut wc = env.clone("a").await;
    write(&wc, "README.md", "mine\n");
    let head = env.commit(&mut wc, Some("mine")).await.unwrap();
    env.repo.race(put("docs/guide.md", "theirs\n"));
    // The workspace is closed, and reopening it fails: the move is half done.
    env.repo.fail(
        "OpenWorkspace",
        Fault::Before(ApiError::Unavailable { retry_after_ms: 0 }),
    );
    let err = env.land(&mut wc).await.unwrap_err();
    assert!(matches!(err, Error::Api(ApiError::Unavailable { .. })));
    let mut wc = WorkingCopy::open(wc.root()).unwrap();
    let moving = wc.state().moving.expect("the move is recorded");
    assert!(env.repo.on_main(moving.base).await);
    assert_eq!(wc.state().head, Some(head));

    // A fresh process: `begin` reopens the workspace where the move goes,
    // and `land` reports what landed.
    let (id, rebased) = landed(env.land(&mut wc).await.unwrap());
    assert_eq!(id, moving.base);
    assert!(rebased);
    assert_eq!(wc.state().moving, None);
    assert_eq!(read(&wc, "docs/guide.md"), "theirs\n");
    env.assert_checked_out(&wc).await;
}

#[tokio::test]
async fn contended_and_refused_lands_change_nothing() {
    let env = Env::d6().await;
    let mut wc = env.clone("a").await;
    write(&wc, "README.md", "mine\n");
    let head = env.commit(&mut wc, Some("mine")).await.unwrap();
    env.repo.attest_all(head).await;
    let tip = env.repo.tip().await;
    for e in [
        ApiError::Contended {
            line: "main".into(),
            tip,
        },
        ApiError::Unauthenticated,
    ] {
        env.repo.fail("Land", Fault::Before(e.clone()));
        let err = env.land(&mut wc).await.unwrap_err();
        assert!(matches!(&err, Error::Api(got) if *got == e), "{err:?}");
        assert_eq!((wc.state().base, wc.state().head), (tip, Some(head)));
    }
    // Contended past the wait is retried until it lands.
    env.repo.fail(
        "Land",
        Fault::Before(ApiError::Contended {
            line: "main".into(),
            tip,
        }),
    );
    let o = LandOptions {
        poll: std::time::Duration::from_millis(1),
        ..LandOptions::default()
    };
    assert_eq!(landed(env.land_with(&mut wc, &o).await.unwrap()).0, head);

    // Without the land scope: denied.
    write(&wc, "README.md", "again\n");
    env.commit(&mut wc, Some("again")).await.unwrap();
    env.repo.set_scopes(&["repo:demo:write"]);
    let err = env.land(&mut wc).await.unwrap_err();
    assert!(
        matches!(&err, Error::Api(ApiError::PermissionDenied { scope, .. }) if scope == "repo:demo:land:main"),
        "{err:?}"
    );
}

#[tokio::test]
async fn after_a_conflict_sync_writes_markers_and_an_edit_commit_and_land_succeed() {
    let env = Env::d6().await;
    let mut wc = env.clone("a").await;
    write(&wc, "src/lib.rs", "fn a() { 1 }\nfn b() {}\nfn c() {}\n");
    write(&wc, "README.md", "mine\n");
    let head = env.commit(&mut wc, Some("mine")).await.unwrap();
    env.repo.attest_all(head).await;
    let theirs = "fn a() { 2 }\nfn b() {}\nfn c() {}\n";
    let tip = env.repo.advance(put("src/lib.rs", theirs)).await;

    // Land: Conflict, and the working copy and the draft are untouched.
    let err = env.land(&mut wc).await.unwrap_err();
    assert!(
        matches!(&err, Error::Api(ApiError::Conflict { onto, .. }) if *onto == tip),
        "{err:?}"
    );
    assert_eq!(wc.state().head, Some(head));
    assert_eq!(
        read(&wc, "src/lib.rs"),
        "fn a() { 1 }\nfn b() {}\nfn c() {}\n"
    );

    // A plain sync keeps the conflict typed and changes nothing.
    wc.begin(&*env.repo, &env.cx(), DEFAULT_LEASE_MS)
        .await
        .unwrap();
    let cache = wc.open_cache(DEFAULT_CACHE_BYTES).unwrap();
    let err = wc
        .sync(
            &*env.repo,
            &cache,
            &env.cx(),
            DEFAULT_LEASE_MS,
            &FetchOptions::default(),
        )
        .await
        .unwrap_err();
    drop(cache);
    assert!(
        matches!(&err, Error::Api(ApiError::Conflict { onto, .. }) if *onto == tip),
        "{err:?}"
    );
    assert_eq!((wc.state().head, wc.state().moving), (Some(head), None));
    assert!(!wc.has_conflicts());

    // Writing the conflicts: the working copy moves to the tip, with the
    // stack on top and the conflict marked.
    let Err(Error::Conflicted(c)) = env.sync(&mut wc).await else {
        panic!("expected conflicts");
    };
    assert_eq!(c.onto, tip);
    assert_eq!(
        c.files,
        vec![ConflictFile {
            path: p("src/lib.rs"),
            mode: ConflictMode::Markers
        }]
    );
    assert_eq!((wc.state().base, wc.state().head), (tip, None));
    assert_eq!(
        read(&wc, "src/lib.rs"),
        "<<<<<<< ours\nfn a() { 1 }\n||||||| base\nfn a() {}\n=======\nfn a() { 2 }\n>>>>>>> theirs\nfn b() {}\nfn c() {}\n"
    );
    assert_eq!(read(&wc, "README.md"), "mine\n");
    assert!(env.repo.on_main(wc.state().base).await);

    // Markers left in: the commit is refused.
    let err = env.commit(&mut wc, None).await.unwrap_err();
    assert!(matches!(&err, Error::Unresolved(paths) if *paths == [p("src/lib.rs")]));
    // Land and other writes wait for the conflicts.
    assert!(matches!(env.land(&mut wc).await, Err(Error::Unresolved(_))));

    // Resolve, commit (the stack's message by default), attest, land.
    write(&wc, "src/lib.rs", "fn a() { 3 }\nfn b() {}\nfn c() {}\n");
    let resolved = env.commit(&mut wc, None).await.unwrap();
    assert_eq!(env.drafts(&wc).await, ["mine"]);
    assert!(wc.conflicts().unwrap().is_none());
    env.repo.attest_all(resolved).await;
    let (id, _) = landed(env.land(&mut wc).await.unwrap());
    assert_eq!(env.repo.tip().await, id);
    let files = env.repo.files(id).await;
    assert_eq!(files["src/lib.rs"], b"fn a() { 3 }\nfn b() {}\nfn c() {}\n");
    assert_eq!(files["README.md"], b"mine\n");
    env.assert_checked_out(&wc).await;
}

#[tokio::test]
async fn land_sync_writes_the_conflicts_itself_and_merges_what_is_clean() {
    let env = Env::direct().await;
    let mut wc = env.clone("a").await;
    // Both sides edit src/lib.rs, in different places: a clean merge.
    write(&wc, "src/lib.rs", "fn a() { 1 }\nfn b() {}\nfn c() {}\n");
    write(&wc, "assets/logo.bin", "\u{0}mine");
    env.commit(&mut wc, Some("mine")).await.unwrap();
    env.repo
        .advance(vec![
            (
                "src/lib.rs".into(),
                Some(b"fn a() {}\nfn b() {}\nfn c() { 2 }\n".to_vec()),
            ),
            ("assets/logo.bin".into(), Some(b"\0theirs".to_vec())),
        ])
        .await;

    let o = LandOptions {
        sync: true,
        ..quick()
    };
    let Err(Error::Conflicted(c)) = env.land_with(&mut wc, &o).await else {
        panic!("expected conflicts");
    };
    assert_eq!(
        c.files,
        vec![
            ConflictFile {
                path: p("assets/logo.bin"),
                mode: ConflictMode::Sidecar
            },
            ConflictFile {
                path: p("src/lib.rs"),
                mode: ConflictMode::Merged
            },
        ]
    );
    assert_eq!(
        read(&wc, "src/lib.rs"),
        "fn a() { 1 }\nfn b() {}\nfn c() { 2 }\n"
    );
    // The binary holds ours, and the sidecar names all three sides.
    assert_eq!(read(&wc, "assets/logo.bin"), "\u{0}mine");
    let sidecar = sidecar_path(&p("assets/logo.bin")).unwrap();
    let note = read(&wc, sidecar.as_str());
    assert!(note.contains("cannot carry conflict markers"), "{note}");
    // The sidecar is the working copy's own: status does not list it.
    assert!(
        wc.status()
            .unwrap()
            .iter()
            .all(|s| s.path().as_str() != sidecar.as_str())
    );

    let err = env.commit(&mut wc, None).await.unwrap_err();
    assert!(matches!(&err, Error::Unresolved(paths) if *paths == [p("assets/logo.bin")]));
    fs::remove_file(wc.root().join(sidecar.as_str())).unwrap();
    env.commit(&mut wc, None).await.unwrap();
    let (id, _) = landed(env.land(&mut wc).await.unwrap());
    let files = env.repo.files(id).await;
    assert_eq!(files["assets/logo.bin"], b"\0mine");
    assert_eq!(
        files["src/lib.rs"],
        b"fn a() { 1 }\nfn b() {}\nfn c() { 2 }\n"
    );
}

#[tokio::test]
async fn a_sync_conflict_needs_a_clean_working_copy() {
    let env = Env::direct().await;
    let mut wc = env.clone("a").await;
    write(&wc, "README.md", "mine\n");
    let head = env.commit(&mut wc, Some("mine")).await.unwrap();
    env.repo.advance(put("README.md", "theirs\n")).await;
    write(&wc, "docs/guide.md", "uncommitted\n");
    let err = env.sync(&mut wc).await.unwrap_err();
    assert!(
        matches!(&err, Error::Dirty(paths) if *paths == [p("docs/guide.md")]),
        "{err:?}"
    );
    assert_eq!(wc.state().head, Some(head));
    assert_eq!(wc.state().moving, None);
    assert_eq!(read(&wc, "README.md"), "mine\n");
}

/// Two working copies edit `src/lib.rs` and commit; the first, which
/// changes the last function, lands. Returns the env and the second
/// working copy, its draft (`second`) not yet landed.
async fn two_writers_on_one_file(second: &str) -> (Env, WorkingCopy) {
    let env = Env::direct().await;
    let mut one = env.clone_in("one", Some("one")).await;
    let mut two = env.clone_in("two", Some("two")).await;
    write(&one, "src/lib.rs", "fn a() {}\nfn b() {}\nfn c() { 1 }\n");
    env.commit(&mut one, Some("one")).await.unwrap();
    write(&two, "src/lib.rs", second);
    env.commit(&mut two, Some("two")).await.unwrap();
    let (_, rebased) = landed(env.land(&mut one).await.unwrap());
    assert!(!rebased);
    (env, two)
}

#[tokio::test]
async fn land_line_merges_edits_to_separate_regions_of_one_file() {
    // The first and the last function, with an untouched line between.
    let (env, mut two) = two_writers_on_one_file("fn a() { 2 }\nfn b() {}\nfn c() {}\n").await;
    // A plain land (no sync): the server's rebase merges the file (D16).
    let (id, rebased) = landed(env.land(&mut two).await.unwrap());
    assert!(rebased);
    assert_eq!(env.repo.tip().await, id);
    let merged = "fn a() { 2 }\nfn b() {}\nfn c() { 1 }\n";
    assert_eq!(env.repo.files(id).await["src/lib.rs"], merged.as_bytes());
    assert_eq!(read(&two, "src/lib.rs"), merged);
    env.assert_checked_out(&two).await;
}

#[tokio::test]
async fn land_with_file_merge_refuses_edits_to_separate_regions() {
    let (env, mut two) = two_writers_on_one_file("fn a() { 2 }\nfn b() {}\nfn c() {}\n").await;
    let head = two.state().head;
    let o = LandOptions {
        merge: MergeMode::File,
        ..quick()
    };
    let err = env.land_with(&mut two, &o).await.unwrap_err();
    assert!(
        matches!(&err, Error::Api(ApiError::Conflict { .. })),
        "{err:?}"
    );
    assert_eq!(two.state().head, head);
    assert_eq!(
        read(&two, "src/lib.rs"),
        "fn a() { 2 }\nfn b() {}\nfn c() {}\n"
    );
}

#[tokio::test]
async fn land_line_merge_still_refuses_edits_to_the_same_line() {
    let (env, mut two) = two_writers_on_one_file("fn a() {}\nfn b() {}\nfn c() { 2 }\n").await;
    let head = two.state().head;
    let tip = env.repo.tip().await;
    let err = env.land(&mut two).await.unwrap_err();
    assert!(
        matches!(&err, Error::Api(ApiError::Conflict { onto, .. }) if *onto == tip),
        "{err:?}"
    );
    // Nothing moved: the line, the draft and the file are as they were.
    assert_eq!(env.repo.tip().await, tip);
    assert_eq!(two.state().head, head);
    assert_eq!(
        read(&two, "src/lib.rs"),
        "fn a() {}\nfn b() {}\nfn c() { 2 }\n"
    );
}

#[tokio::test]
async fn revert_adds_an_inverse_draft_that_lands() {
    let env = Env::direct().await;
    let mut wc = env.clone("a").await;
    write(&wc, "README.md", "changed\n");
    let bad = env.commit(&mut wc, Some("bad change")).await.unwrap();
    landed(env.land(&mut wc).await.unwrap());

    let reverted = env.revert(&mut wc, bad).await.unwrap();
    assert_eq!(reverted.reverted, bad);
    assert_eq!(wc.state().head, Some(reverted.id));
    assert_eq!(read(&wc, "README.md"), "hello\n");
    assert_eq!(
        env.drafts(&wc).await,
        [format!("Revert \"bad change\"\n\nThis reverts {bad}.")]
    );
    let (id, _) = landed(env.land(&mut wc).await.unwrap());
    assert_eq!(env.repo.files(id).await["README.md"], b"hello\n");

    // A local edit in the way: refused, nothing moves.
    write(&wc, "docs/guide.md", "edit\n");
    let edit = env.commit(&mut wc, Some("edit")).await.unwrap();
    landed(env.land(&mut wc).await.unwrap());
    write(&wc, "docs/guide.md", "local\n");
    let err = env.revert(&mut wc, edit).await.unwrap_err();
    assert!(matches!(err, Error::Dirty(_)), "{err:?}");
    assert_eq!(wc.state().head, None);
}

#[tokio::test]
async fn stack_operations_rewrite_the_drafts_and_the_files_follow() {
    let env = Env::direct().await;
    let mut wc = env.clone("a").await;
    let mut ids = Vec::new();
    for (path, msg) in [
        ("README.md", "one"),
        ("docs/guide.md", "two"),
        ("src/lib.rs", "three"),
    ] {
        write(&wc, path, &format!("{msg}\n"));
        ids.push(env.commit(&mut wc, Some(msg)).await.unwrap());
    }
    assert_eq!(env.drafts(&wc).await, ["one", "two", "three"]);

    // A dirty working copy is refused before the server is asked.
    write(&wc, "README.md", "dirty\n");
    let err = env
        .rewrite(&mut wc, StackOp::Drop(ids[1]))
        .await
        .unwrap_err();
    assert!(matches!(err, Error::Dirty(_)));
    write(&wc, "README.md", "one\n");

    env.rewrite(&mut wc, StackOp::Drop(ids[1])).await.unwrap();
    assert_eq!(env.drafts(&wc).await, ["one", "three"]);
    assert_eq!(read(&wc, "docs/guide.md"), "guide\n");
    env.assert_checked_out(&wc).await;

    let stack_ids = |wc: &WorkingCopy| {
        let (base, head) = (wc.state().base, wc.state().head);
        let repo = env.repo.clone();
        let cx = env.cx();
        async move {
            stack(&*repo, &cx, REPO, base, head)
                .await
                .unwrap()
                .into_iter()
                .map(|v| v.changeset.id())
                .collect::<Vec<_>>()
        }
    };
    let now = stack_ids(&wc).await;
    env.rewrite(&mut wc, StackOp::Reorder(vec![now[1], now[0]]))
        .await
        .unwrap();
    assert_eq!(env.drafts(&wc).await, ["three", "one"]);

    let now = stack_ids(&wc).await;
    env.rewrite(&mut wc, StackOp::Squash(now.clone()))
        .await
        .unwrap();
    assert_eq!(env.drafts(&wc).await, ["three\n\none"]);

    let now = stack_ids(&wc).await;
    env.rewrite(
        &mut wc,
        StackOp::Split {
            draft: now[0],
            groups: vec![vec![p("README.md")]],
        },
    )
    .await
    .unwrap();
    assert_eq!(
        env.drafts(&wc).await,
        ["three\n\none (part 1/2)", "three\n\none (part 2/2)"]
    );
    assert_eq!(read(&wc, "README.md"), "one\n");
    assert_eq!(read(&wc, "src/lib.rs"), "three\n");
    env.assert_checked_out(&wc).await;
    landed(env.land(&mut wc).await.unwrap());
}

#[tokio::test]
async fn a_lost_stack_reply_is_taken_up_by_the_next_command() {
    let env = Env::direct().await;
    let mut wc = env.clone("a").await;
    write(&wc, "README.md", "one\n");
    let one = env.commit(&mut wc, Some("one")).await.unwrap();
    write(&wc, "docs/guide.md", "two\n");
    env.commit(&mut wc, Some("two")).await.unwrap();
    env.repo.fail(
        "Stack",
        Fault::After(ApiError::Unavailable { retry_after_ms: 0 }),
    );
    let err = env.rewrite(&mut wc, StackOp::Drop(one)).await.unwrap_err();
    assert!(matches!(err, Error::Api(ApiError::Unavailable { .. })));

    let mut wc = WorkingCopy::open(wc.root()).unwrap();
    let begun = wc
        .begin(&*env.repo, &env.cx(), DEFAULT_LEASE_MS)
        .await
        .unwrap();
    assert!(begun.unfinished);
    let cache = wc.open_cache(DEFAULT_CACHE_BYTES).unwrap();
    wc.finish(
        &*env.repo,
        &cache,
        &env.cx(),
        DEFAULT_LEASE_MS,
        &FetchOptions::default(),
    )
    .await
    .unwrap();
    assert_eq!(env.drafts(&wc).await, ["two"]);
    assert_eq!(read(&wc, "README.md"), "hello\n");
    env.assert_checked_out(&wc).await;
}

// ---- the property: never stranded --------------------------------------

/// What the line does just before the land.
#[derive(Clone, Copy, Debug)]
enum Rival {
    None,
    /// Another writer lands a change to another file.
    Disjoint,
    /// Another writer lands a change to the same file.
    Same,
}

/// The operations a fault can hit during land and the moves after it.
const OPS: [&str; 10] = [
    "Land",
    "CloseWorkspace",
    "OpenWorkspace",
    "RenewLease",
    "GetWorkspace",
    "SetHead",
    "Sync",
    "Diff",
    "GetObjects",
    "Stat",
];

fn fault_strategy() -> impl Strategy<Value = (usize, bool, bool)> {
    // (operation, after the engine ran it, contended instead of unavailable)
    (0..OPS.len(), any::<bool>(), any::<bool>())
}

/// The working copy's base, and where an unfinished move goes, are on the
/// line.
async fn assert_on_the_line(env: &Env, wc: &WorkingCopy) {
    assert!(
        env.repo.on_main(wc.state().base).await,
        "stranded on base {}",
        wc.state().base
    );
    if let Some(m) = wc.state().moving {
        assert!(env.repo.on_main(m.base).await, "moving to {}", m.base);
    }
}

async fn run_race(rival: Rival, sync: bool, faults: Vec<(usize, bool, bool)>) {
    let env = Env::d6().await;
    let mut wc = env.clone("a").await;
    write(&wc, "src/lib.rs", "fn a() { 1 }\nfn b() {}\nfn c() {}\n");
    let head = env.commit(&mut wc, Some("mine")).await.unwrap();
    env.repo.attest_all(head).await;
    match rival {
        Rival::None => {}
        Rival::Disjoint => env.repo.race(put("docs/guide.md", "theirs\n")),
        Rival::Same => env
            .repo
            .race(put("src/lib.rs", "fn a() { 2 }\nfn b() {}\nfn c() {}\n")),
    }
    let tip = env.repo.tip().await;
    for (op, after, contended) in faults {
        let e = if contended {
            ApiError::Contended {
                line: "main".into(),
                tip,
            }
        } else {
            ApiError::Unavailable { retry_after_ms: 0 }
        };
        let fault = if after {
            Fault::After(e)
        } else {
            Fault::Before(e)
        };
        env.repo.fail(OPS[op], fault);
    }

    // The faulty attempt, in its own process.
    let o = LandOptions { sync, ..quick() };
    let _ = env.land_with(&mut wc, &o).await;
    let mut wc = WorkingCopy::open(wc.root()).unwrap();
    assert_on_the_line(&env, &wc).await;

    // Recovery with the network healthy: each step a fresh process.
    env.repo.clear_faults();
    for _ in 0..6 {
        let mut next = WorkingCopy::open(wc.root()).unwrap();
        let step = if let Some(c) = next
            .conflicts()
            .unwrap()
            .filter(|_| next.state().moving.is_none())
        {
            for f in c.unresolved() {
                write(
                    &next,
                    f.path.as_str(),
                    "fn a() { 3 }\nfn b() {}\nfn c() {}\n",
                );
                if let Some(s) = sidecar_path(&f.path) {
                    let _ = fs::remove_file(next.root().join(s.as_str()));
                }
            }
            match env.commit(&mut next, None).await {
                Ok(id) => {
                    env.repo.attest_all(id).await;
                    Ok(())
                }
                Err(e) => Err(e),
            }
        } else if next.state().head.is_some() || next.state().moving.is_some() {
            env.land_with(
                &mut next,
                &LandOptions {
                    sync: true,
                    ..quick()
                },
            )
            .await
            .map(|_| ())
        } else {
            wc = next;
            break;
        };
        match step {
            Ok(()) | Err(Error::Conflicted(_)) => {}
            Err(e) => panic!("recovery failed: {e:?}"),
        }
        assert_on_the_line(&env, &next).await;
        wc = next;
    }

    // Settled: on the tip, clean, with our change (or its resolution) landed.
    let wc = WorkingCopy::open(wc.root()).unwrap();
    assert_eq!(wc.state().moving, None);
    assert_eq!(wc.state().head, None, "the stack did not land");
    assert_eq!(wc.state().base, env.repo.tip().await);
    assert!(wc.conflicts().unwrap().is_none());
    let lib =
        String::from_utf8(env.repo.files(wc.state().base).await["src/lib.rs"].clone()).unwrap();
    assert!(
        lib.starts_with("fn a() { 1 }") || lib.starts_with("fn a() { 3 }"),
        "{lib}"
    );
    env.assert_checked_out(&wc).await;
}

proptest! {
    #![proptest_config(ProptestConfig { cases: 48, ..ProptestConfig::default() })]

    #[test]
    fn the_working_copy_is_never_left_on_an_unreachable_base(
        rival in prop_oneof![Just(Rival::None), Just(Rival::Disjoint), Just(Rival::Same)],
        sync in any::<bool>(),
        faults in proptest::collection::vec(fault_strategy(), 0..4),
    ) {
        let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
        rt.block_on(run_race(rival, sync, faults));
    }
}
