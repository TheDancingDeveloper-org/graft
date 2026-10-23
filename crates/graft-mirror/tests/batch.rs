//! Batch mode (decision D21) against a local bare repository standing in for
//! GitHub's git, and a fake GitHub API holding the PRs. The graft side is an
//! in-process `graft-engine` over the memory store.

mod support;

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use graft_engine::{Engine, EngineConfig};
use graft_mirror::batch::{BatchConfig, BatchOutcome, BatchRecord, BatchState, Batcher};
use graft_mirror::credentials::SecretToken;
use graft_mirror::follow::{FollowConfig, FollowLine, POLL_INTERVAL};
use graft_mirror::github::{CheckRun, Forge, ForgeError, NewPull, Pull};
use graft_mirror::{Mirror, MirrorDb, MirrorError, current_ref};
use graft_model::{
    Author, AuthorKind, Change, ChangesetId, EntryKind, Metadata, ObjectKind, RepoPath, Timestamp,
};
use graft_proto::api::{AdminApi, RepoApi};
use graft_proto::types::{Attest, CreateChangeset};
use graft_proto::{CallContext, Frame};
use graft_store::memory::MemoryStore;
use graft_testkit::clock::FakeClock;
use support::{REPO, clock, cx, git, move_line};

type Api = Engine<MemoryStore, MemoryStore>;

const LINE: &str = "integration";

/// One PR the fake GitHub holds.
#[derive(Clone, Debug)]
struct FakePr {
    number: u64,
    new: NewPull,
    open: bool,
    merged: Option<String>,
}

/// A fake GitHub API: PRs in memory, branch heads read from the bare
/// upstream. It records every call that would write to GitHub.
struct FakeGitHub {
    upstream: PathBuf,
    prs: Mutex<Vec<FakePr>>,
    enqueued: Mutex<Vec<String>>,
}

impl FakeGitHub {
    fn new(upstream: &Path) -> FakeGitHub {
        FakeGitHub {
            upstream: upstream.to_owned(),
            prs: Mutex::new(Vec::new()),
            enqueued: Mutex::new(Vec::new()),
        }
    }

    fn pull(&self, pr: &FakePr) -> Pull {
        Pull {
            number: pr.number,
            url: format!("https://github.com/o/r/pull/{}", pr.number),
            node_id: format!("PR_{}", pr.number),
            open: pr.open,
            merged: pr.merged.is_some(),
            merge_commit: pr.merged.clone(),
            head_sha: support::branch(&self.upstream, &pr.new.head).unwrap_or_default(),
        }
    }

    fn prs(&self) -> Vec<FakePr> {
        self.prs.lock().unwrap().clone()
    }

    /// The operator squash-merges PR `number`: a new commit on `main` with
    /// the PR head's tree, as GitHub's squash merge makes it.
    fn squash_merge(&self, number: u64) -> String {
        let mut prs = self.prs.lock().unwrap();
        let pr = prs.iter_mut().find(|p| p.number == number).unwrap();
        let tree = git(
            &self.upstream,
            &["rev-parse", &format!("refs/heads/{}^{{tree}}", pr.new.head)],
        );
        let message = format!("{} (#{number})", pr.new.title);
        let sha = git(
            &self.upstream,
            &[
                "commit-tree",
                &tree,
                "-p",
                "refs/heads/main",
                "-m",
                &message,
            ],
        );
        git(&self.upstream, &["update-ref", "refs/heads/main", &sha]);
        pr.open = false;
        pr.merged = Some(sha.clone());
        sha
    }

    fn close(&self, number: u64) {
        let mut prs = self.prs.lock().unwrap();
        prs.iter_mut().find(|p| p.number == number).unwrap().open = false;
    }
}

impl Forge for FakeGitHub {
    async fn find_pull(&self, branch: &str) -> Result<Option<Pull>, ForgeError> {
        let prs = self.prs.lock().unwrap();
        Ok(prs
            .iter()
            .find(|p| p.new.head == branch)
            .map(|p| self.pull(p)))
    }

    async fn create_pull(&self, new: &NewPull) -> Result<Pull, ForgeError> {
        let mut prs = self.prs.lock().unwrap();
        let pr = FakePr {
            number: prs.len() as u64 + 1,
            new: new.clone(),
            open: true,
            merged: None,
        };
        prs.push(pr.clone());
        Ok(self.pull(&pr))
    }

    async fn get_pull(&self, number: u64) -> Result<Pull, ForgeError> {
        let prs = self.prs.lock().unwrap();
        prs.iter()
            .find(|p| p.number == number)
            .map(|p| self.pull(p))
            .ok_or_else(|| ForgeError::Refused("status 404: Not Found".into()))
    }

    async fn enqueue(&self, node_id: &str) -> Result<(), ForgeError> {
        self.enqueued.lock().unwrap().push(node_id.to_owned());
        Ok(())
    }

    async fn check_runs(&self, _sha: &str) -> Result<Vec<CheckRun>, ForgeError> {
        Ok(Vec::new())
    }

    async fn token(&self) -> Result<SecretToken, ForgeError> {
        Err(ForgeError::Refused(
            "a local upstream needs no token".into(),
        ))
    }
}

/// GitHub's git (bare), a work tree that pushes to it as other people do,
/// the mirror's directory and DB, and an engine.
struct World {
    _tmp: tempfile::TempDir,
    upstream: PathBuf,
    work: PathBuf,
    mirror_dir: PathBuf,
    db: MirrorDb,
    api: Api,
    github: FakeGitHub,
}

impl World {
    async fn new() -> World {
        let tmp = tempfile::tempdir().unwrap();
        let upstream = tmp.path().join("github.git");
        let work = tmp.path().join("work");
        support::github(&upstream, true);
        std::fs::create_dir_all(&work).unwrap();
        git(&work, &["init", "-q", "-b", "main"]);
        git(
            &work,
            &["remote", "add", "origin", upstream.to_str().unwrap()],
        );
        let store = Arc::new(MemoryStore::new());
        let api = Engine::new(
            store.clone(),
            store,
            Arc::new(FakeClock::at(1_000_000)),
            EngineConfig::default(),
        );
        api.create_repo(&cx(), REPO.to_owned()).await.unwrap();
        let db = MirrorDb::open(&tmp.path().join("mirror.sqlite")).unwrap();
        World {
            mirror_dir: tmp.path().join("mirror.git"),
            github: FakeGitHub::new(&upstream),
            _tmp: tmp,
            upstream,
            work,
            db,
            api,
        }
    }

    /// Someone pushes a commit writing `files` to GitHub's `main`.
    fn push_main(&self, files: &[(&str, &str)], message: &str) -> String {
        for (path, body) in files {
            std::fs::write(self.work.join(path), body).unwrap();
        }
        git(&self.work, &["add", "-A"]);
        git(&self.work, &["commit", "-q", "-m", message]);
        git(&self.work, &["push", "-q", "origin", "HEAD:main"]);
        git(&self.work, &["rev-parse", "HEAD"])
    }

    fn batcher(&self) -> Batcher<'_, Api, FakeGitHub> {
        self.batcher_on(&self.db)
    }

    /// A batcher on this world's graft and GitHub with another mirror DB.
    fn batcher_on<'w>(&'w self, db: &'w MirrorDb) -> Batcher<'w, Api, FakeGitHub> {
        let base = FollowLine {
            branch: "main".to_owned(),
            line: "main".to_owned(),
        };
        let follow = FollowConfig {
            repo: REPO.to_owned(),
            lines: vec![base.clone()],
            allow_lossy: Vec::new(),
            batch: 256,
            interval: POLL_INTERVAL,
        };
        Batcher::new(
            &self.api,
            cx(),
            db,
            Mirror::new(&self.mirror_dir, self.upstream.to_str().unwrap()),
            &self.github,
            BatchConfig::new(LINE, base, follow),
        )
        .unwrap()
        .with_clock(clock)
    }

    async fn tip(&self, line: &str) -> Option<ChangesetId> {
        current_ref(&self.api, &cx(), REPO, line).await.unwrap()
    }

    /// Lands one changeset writing `files` on the integration line, with
    /// the session and work item an agent's token stamps.
    async fn land(&self, files: &[(&str, &str)], message: &str, n: i64) -> ChangesetId {
        let tip = self.tip(LINE).await;
        let mut changes = Vec::new();
        let mut inline = Vec::new();
        for (path, body) in files {
            changes.push(Change::Put {
                path: RepoPath::new(path).unwrap(),
                kind: EntryKind::File,
                id: graft_model::ObjectId::hash(ObjectKind::Blob, body.as_bytes()),
            });
            inline.push(Frame::encode(ObjectKind::Blob, body.as_bytes()));
        }
        let mut metadata = Metadata::new();
        metadata.insert("vogt.session".to_owned(), format!("ses_{n}"));
        metadata.insert("vogt.work_item".to_owned(), format!("WI-{n}"));
        let id = self
            .api
            .create_changeset(
                &cx(),
                CreateChangeset {
                    repo: REPO.to_owned(),
                    parents: tip.into_iter().collect(),
                    changes,
                    author: Author {
                        name: "Tess".to_owned(),
                        email: "tess@example.com".to_owned(),
                        kind: AuthorKind::Agent,
                    },
                    authored_at: Timestamp {
                        ms: 1_700_000_000_000 + n * 1000,
                        offset_min: 0,
                    },
                    message: message.to_owned(),
                    metadata,
                    inline_blobs: inline,
                    workspace: None,
                },
            )
            .await
            .unwrap()
            .id;
        move_line(&self.api, LINE, tip, id).await;
        id
    }

    /// GitHub's `main` once, imported, and the integration line forked from
    /// graft's `main`.
    async fn start(&self) {
        self.push_main(
            &[
                ("readme.md", "hello\n"),
                ("shared.txt", "one\ntwo\nthree\n"),
            ],
            "init",
        );
        // A first run follows main (the integration line does not exist
        // yet, so it stops there).
        assert!(self.batcher().run().await.is_err());
        let main = self.tip("main").await.expect("main followed");
        move_line(&self.api, LINE, None, main).await;
    }

    fn files_at(&self, sha: &str) -> Vec<String> {
        git(&self.upstream, &["ls-tree", "-r", "--name-only", sha])
            .lines()
            .map(str::to_owned)
            .collect()
    }

    fn changed(&self, from: &str, to: &str) -> Vec<String> {
        git(&self.upstream, &["diff", "--name-only", from, to])
            .lines()
            .map(str::to_owned)
            .collect()
    }

    fn show(&self, sha: &str, path: &str) -> String {
        git(&self.upstream, &["show", &format!("{sha}:{path}")])
    }
}

fn attest_cx() -> CallContext {
    let mut c = cx();
    c.principal.scopes.push(format!("repo:{REPO}:attest"));
    c
}

fn opened(outcome: BatchOutcome) -> BatchRecord {
    match outcome {
        BatchOutcome::Opened(rec) => rec,
        other => panic!("expected a new batch, got {other:?}"),
    }
}

#[tokio::test]
async fn a_batch_is_one_squashed_pr_and_the_next_forks_from_its_merge() {
    let w = World::new().await;
    w.start().await;
    let a = w.land(&[("a.txt", "a1\n")], "add a", 1).await;
    let b = w
        .land(&[("b.txt", "b1\n")], "add b\n\nwith a body", 2)
        .await;
    w.api
        .attest(
            &attest_cx(),
            Attest {
                repo: REPO.to_owned(),
                changeset: b,
                check: "ci/test".to_owned(),
                passed: true,
                url: "https://ci.example/1".to_owned(),
            },
        )
        .await
        .unwrap();
    // GitHub's main moves under the batch.
    let moved = w.push_main(&[("c.txt", "c1\n")], "someone else");

    let before = w.tip(LINE).await;
    let run = w.batcher().run().await.unwrap();
    assert!(run.settled.is_none());
    let synced = run.synced.expect("the line synced onto the moved main");
    assert_eq!(w.tip(LINE).await, Some(synced));
    let view = w
        .api
        .get_changesets(
            &cx(),
            graft_proto::types::GetChangesets {
                repo: REPO.to_owned(),
                ids: vec![synced],
            },
        )
        .await
        .unwrap()
        .found
        .remove(0);
    assert_eq!(
        view.changeset.first_parent(),
        before,
        "a sync is a fast-forward of the line"
    );
    let rec = opened(run.outcome);
    assert_eq!(rec.number, 1);
    assert_eq!(rec.state, BatchState::Open);
    assert_eq!(rec.lands, 2, "the sync changeset is not a land");
    assert_eq!(rec.branch, "graft/batch-1");
    assert_eq!(rec.base_sha.to_hex(), moved);

    // One commit on GitHub's current main, holding the batch's net change.
    let squash = support::branch(&w.upstream, "graft/batch-1").unwrap();
    assert_eq!(squash, rec.squash_sha.to_hex());
    assert_eq!(
        git(&w.upstream, &["rev-parse", &format!("{squash}^@")]),
        moved
    );
    assert_eq!(w.changed(&moved, &squash), ["a.txt", "b.txt"]);
    assert!(w.files_at(&squash).contains(&"c.txt".to_owned()));
    let message = git(&w.upstream, &["log", "-1", "--format=%B", &squash]);
    assert!(message.contains("Graft-Batch: 1"), "{message}");

    // The PR: into main, from the batch branch, listing every land.
    let prs = w.github.prs();
    assert_eq!(prs.len(), 1);
    let pr = &prs[0].new;
    assert_eq!(
        (pr.head.as_str(), pr.base.as_str()),
        ("graft/batch-1", "main")
    );
    for id in [a, b] {
        assert!(pr.body.contains(&id.to_string()[..13]), "{}", pr.body);
    }
    for text in ["ses_1", "WI-2", "ci/test passed", "Graft-Batch: 1", "add b"] {
        assert!(pr.body.contains(text), "{text} missing from {}", pr.body);
    }
    assert!(!pr.body.contains("with a body"), "only subjects are listed");

    // While it is open, a run waits and opens nothing.
    match w.batcher().run().await.unwrap().outcome {
        BatchOutcome::Waiting(r) => assert_eq!(r.number, 1),
        other => panic!("expected to wait, got {other:?}"),
    }
    assert_eq!(w.github.prs().len(), 1);

    // The operator squash-merges it. Meanwhile an agent lands again,
    // touching a file the batch also changed.
    let merged = w.github.squash_merge(1);
    let d = w
        .land(&[("a.txt", "a2\n"), ("d.txt", "d1\n")], "more a, add d", 3)
        .await;

    let run = w.batcher().run().await.unwrap();
    let settled = run.settled.expect("batch 1 settled");
    assert_eq!(settled.state, BatchState::Merged);
    assert_eq!(settled.merge_sha.unwrap().to_hex(), merged);
    // The merge's snapshot is batch 1's tip, which the line already holds:
    // absorbing it needs no line move.
    assert!(run.synced.is_none());
    let rec = opened(run.outcome);
    assert_eq!(rec.number, 2);
    assert_eq!(rec.lands, 1, "batch 2 counts from batch 1's tip");
    assert_eq!(rec.base_sha.to_hex(), merged);
    let squash = support::branch(&w.upstream, "graft/batch-2").unwrap();
    assert_eq!(w.changed(&merged, &squash), ["a.txt", "d.txt"]);
    assert_eq!(w.show(&squash, "a.txt"), "a2");
    let body = &w.github.prs()[1].new.body;
    assert!(body.contains(&d.to_string()[..13]));
    assert!(!body.contains(&a.to_string()[..13]));

    assert!(
        w.github.enqueued.lock().unwrap().is_empty(),
        "batch mode never queues or merges"
    );
}

#[tokio::test]
async fn a_closed_pr_is_rejected_and_its_lands_stay_in_the_next_batch() {
    let w = World::new().await;
    w.start().await;
    w.land(&[("a.txt", "a1\n")], "add a", 1).await;
    let rec = opened(w.batcher().run().await.unwrap().outcome);
    let tip = w.tip(LINE).await;
    w.github.close(rec.pr_number.unwrap());

    w.land(&[("b.txt", "b1\n")], "add b", 2).await;
    let run = w.batcher().run().await.unwrap();
    let settled = run.settled.expect("batch 1 settled");
    assert_eq!(settled.state, BatchState::Rejected);
    assert!(settled.detail.contains("closed without merging"));
    assert!(
        run.synced.is_none(),
        "main did not move; the line is as it was"
    );
    let rec = opened(run.outcome);
    assert_eq!((rec.number, rec.lands), (2, 2));
    assert_ne!(w.tip(LINE).await, tip);
    let squash = support::branch(&w.upstream, "graft/batch-2").unwrap();
    let main = support::branch(&w.upstream, "main").unwrap();
    assert_eq!(w.changed(&main, &squash), ["a.txt", "b.txt"]);
    // Batch 1's branch is left as it was.
    assert!(support::branch(&w.upstream, "graft/batch-1").is_some());
}

#[tokio::test]
async fn a_conflicting_base_move_is_reported_and_nothing_moves() {
    let w = World::new().await;
    w.start().await;
    w.land(
        &[("shared.txt", "one\nTWO (graft)\nthree\n")],
        "edit shared",
        1,
    )
    .await;
    w.push_main(
        &[("shared.txt", "one\nTWO (github)\nthree\n")],
        "edit shared upstream",
    );
    let tip = w.tip(LINE).await;

    let run = w.batcher().run().await.unwrap();
    match run.outcome {
        BatchOutcome::Conflict { paths, .. } => assert_eq!(paths, ["shared.txt"]),
        other => panic!("expected a conflict, got {other:?}"),
    }
    assert_eq!(w.tip(LINE).await, tip, "nothing moved");
    assert!(w.github.prs().is_empty());
    assert!(support::branch(&w.upstream, "graft/batch-1").is_none());
}

#[tokio::test]
async fn a_line_move_that_merges_by_line_syncs_cleanly() {
    let w = World::new().await;
    w.start().await;
    w.land(&[("shared.txt", "ONE\ntwo\nthree\n")], "edit top", 1)
        .await;
    let up = w.push_main(&[("shared.txt", "one\ntwo\nTHREE\n")], "edit bottom");
    let rec = opened(w.batcher().run().await.unwrap().outcome);
    assert_eq!(rec.base_sha.to_hex(), up);
    let squash = rec.squash_sha.to_hex();
    assert_eq!(w.show(&squash, "shared.txt"), "ONE\ntwo\nTHREE");
    assert_eq!(w.changed(&up, &squash), ["shared.txt"]);
}

#[tokio::test]
async fn nothing_to_export_makes_no_batch() {
    let w = World::new().await;
    w.start().await;
    let run = w.batcher().run().await.unwrap();
    assert_eq!(run.outcome, BatchOutcome::NothingToExport);
    // A base move alone is absorbed, and still nothing is exported.
    w.push_main(&[("c.txt", "c1\n")], "upstream only");
    let run = w.batcher().run().await.unwrap();
    assert_eq!(run.outcome, BatchOutcome::NothingToExport);
    assert!(w.github.prs().is_empty());
}

#[tokio::test]
async fn an_interrupted_batch_resumes_with_the_same_commit() {
    let w = World::new().await;
    w.start().await;
    w.land(&[("a.txt", "a1\n")], "add a", 1).await;
    let rec = opened(w.batcher().run().await.unwrap().outcome);
    // As if the run stopped after recording, before pushing.
    let mut planned = rec.clone();
    planned.state = BatchState::Planned;
    planned.pr_number = None;
    planned.pr_url = None;
    w.db.put_batch(&planned).unwrap();
    let again = w.batcher().track().await.unwrap().unwrap();
    assert_eq!(again.state, BatchState::Open);
    assert_eq!(again.squash_sha, rec.squash_sha);
    assert_eq!(
        again.pr_number, rec.pr_number,
        "the PR on the branch is adopted"
    );
    assert_eq!(w.github.prs().len(), 1);
}

#[tokio::test]
async fn the_base_line_cannot_be_batched() {
    let w = World::new().await;
    let base = FollowLine {
        branch: "main".to_owned(),
        line: "main".to_owned(),
    };
    let follow = FollowConfig {
        repo: REPO.to_owned(),
        lines: vec![base.clone()],
        allow_lossy: Vec::new(),
        batch: 256,
        interval: POLL_INTERVAL,
    };
    let refused = Batcher::new(
        &w.api,
        cx(),
        &w.db,
        Mirror::new(&w.mirror_dir, w.upstream.to_str().unwrap()),
        &w.github,
        BatchConfig::new("main", base, follow),
    );
    assert!(refused.is_err());
}

#[tokio::test]
async fn after_a_merge_a_moved_main_and_new_lands_sync_without_conflict() {
    let w = World::new().await;
    w.start().await;
    w.land(&[("a.txt", "a1\n")], "add a", 1).await;
    let rec = opened(w.batcher().run().await.unwrap().outcome);
    w.github.squash_merge(rec.pr_number.unwrap());
    // GitHub's main moves past the merge, and the batch's file changes again
    // on the line: the next sync diffs from batch 1's tip, so neither
    // conflicts with the merged copy of batch 1.
    git(&w.work, &["fetch", "-q", "origin"]);
    git(&w.work, &["reset", "-q", "--hard", "origin/main"]);
    let up = w.push_main(&[("e.txt", "e1\n")], "after the merge");
    w.land(&[("a.txt", "a2\n")], "change a again", 2).await;

    let run = w.batcher().run().await.unwrap();
    assert_eq!(run.settled.unwrap().state, BatchState::Merged);
    assert!(run.synced.is_some(), "the line absorbed the moved main");
    let rec = opened(run.outcome);
    assert_eq!((rec.number, rec.lands), (2, 1));
    let main = support::branch(&w.upstream, "main").unwrap();
    assert_eq!(main, up);
    assert_eq!(
        rec.base_sha.to_hex(),
        up,
        "batch 2 is based on the moved main"
    );
    let squash = rec.squash_sha.to_hex();
    assert_eq!(w.changed(&main, &squash), ["a.txt"]);
    assert_eq!(w.show(&squash, "a.txt"), "a2");
}

#[tokio::test]
async fn an_empty_db_is_reported_as_empty_not_graft_moved() {
    let w = World::new().await;
    w.start().await;
    let path = w._tmp.path().join("empty.sqlite");
    let empty = MirrorDb::open(&path).unwrap();
    let err = w.batcher_on(&empty).run().await.unwrap_err();
    match &err {
        MirrorError::Unmapped { db, line, empty } => {
            assert_eq!(db, &path);
            assert_eq!(line, "main");
            assert!(*empty);
        }
        e => panic!("expected Unmapped, got {e}"),
    }
    let text = err.to_string();
    assert!(
        text.contains("is empty") && text.contains("empty.sqlite"),
        "{text}"
    );
    assert!(!text.contains("graft-moved"), "{text}");
}

#[tokio::test]
async fn another_repos_db_is_reported_as_unmapped_not_graft_moved() {
    let w = World::new().await;
    w.start().await;
    let other = World::new().await;
    other.push_main(&[("elsewhere.txt", "not this repo\n")], "other init");
    assert!(other.batcher().run().await.is_err());
    assert!(other.db.commit_count().unwrap() > 0);
    let err = w.batcher_on(&other.db).run().await.unwrap_err();
    match &err {
        MirrorError::Unmapped { db, line, empty } => {
            assert_eq!(db, other.db.path());
            assert_eq!(line, "main");
            assert!(!*empty);
        }
        e => panic!("expected Unmapped, got {e}"),
    }
    assert!(err.to_string().contains("another repo"), "{err}");
}
