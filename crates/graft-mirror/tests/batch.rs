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
    comments: Mutex<Vec<(u64, String)>>,
    /// PR numbers GitHub is merging (queued or auto-merge).
    merging: Mutex<Vec<u64>>,
    /// A branch update made immediately after the next PR read.
    race_push: Mutex<Option<(usize, String, String)>>,
}

impl FakeGitHub {
    fn new(upstream: &Path) -> FakeGitHub {
        FakeGitHub {
            upstream: upstream.to_owned(),
            prs: Mutex::new(Vec::new()),
            enqueued: Mutex::new(Vec::new()),
            comments: Mutex::new(Vec::new()),
            merging: Mutex::new(Vec::new()),
            race_push: Mutex::new(None),
        }
    }

    fn pull(&self, pr: &FakePr) -> Pull {
        let pull = Pull {
            number: pr.number,
            url: format!("https://github.com/o/r/pull/{}", pr.number),
            node_id: format!("PR_{}", pr.number),
            open: pr.open,
            merged: pr.merged.is_some(),
            merge_commit: pr.merged.clone(),
            head_sha: support::branch(&self.upstream, &pr.new.head).unwrap_or_default(),
            body: pr.new.body.clone(),
        };
        let mut race = self.race_push.lock().unwrap();
        if let Some((reads, _, _)) = race.as_mut()
            && *reads > 0
        {
            *reads -= 1;
        } else if let Some((_, branch, sha)) = race.take() {
            git(
                &self.upstream,
                &["update-ref", &format!("refs/heads/{branch}"), &sha],
            );
        }
        pull
    }

    fn prs_mut(&self) -> std::sync::MutexGuard<'_, Vec<FakePr>> {
        self.prs.lock().unwrap()
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

    /// The PR is rebase-merged: each branch commit is replayed onto `main`
    /// with a new committer, as GitHub's rebase merge does.
    fn rebase_merge(&self, number: u64) -> String {
        let mut prs = self.prs.lock().unwrap();
        let pr = prs.iter_mut().find(|p| p.number == number).unwrap();
        let head = format!("refs/heads/{}", pr.new.head);
        let list = git(
            &self.upstream,
            &["rev-list", "--reverse", &format!("refs/heads/main..{head}")],
        );
        let mut tip = git(&self.upstream, &["rev-parse", "refs/heads/main"]);
        for c in list.lines() {
            let tree = git(&self.upstream, &["rev-parse", &format!("{c}^{{tree}}")]);
            let message = git(&self.upstream, &["log", "-1", "--format=%B", c]);
            tip = git(
                &self.upstream,
                &["commit-tree", &tree, "-p", &tip, "-m", &message],
            );
        }
        git(&self.upstream, &["update-ref", "refs/heads/main", &tip]);
        pr.open = false;
        pr.merged = Some(tip.clone());
        tip
    }

    /// The PR is merged with a merge commit: `main` and the branch head as
    /// parents, the head's tree.
    fn merge_commit(&self, number: u64) -> String {
        let mut prs = self.prs.lock().unwrap();
        let pr = prs.iter_mut().find(|p| p.number == number).unwrap();
        let head = git(
            &self.upstream,
            &["rev-parse", &format!("refs/heads/{}", pr.new.head)],
        );
        let tree = git(&self.upstream, &["rev-parse", &format!("{head}^{{tree}}")]);
        let sha = git(
            &self.upstream,
            &[
                "commit-tree",
                &tree,
                "-p",
                "refs/heads/main",
                "-p",
                &head,
                "-m",
                "Merge pull request",
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

    fn comments(&self) -> Vec<(u64, String)> {
        self.comments.lock().unwrap().clone()
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

    async fn update_pull(&self, number: u64, body: &str) -> Result<(), ForgeError> {
        let mut prs = self.prs.lock().unwrap();
        let pr = prs
            .iter_mut()
            .find(|pr| pr.number == number)
            .ok_or_else(|| ForgeError::Refused("status 404: Not Found".into()))?;
        pr.new.body = body.to_owned();
        Ok(())
    }

    async fn enqueue(&self, node_id: &str) -> Result<(), ForgeError> {
        self.enqueued.lock().unwrap().push(node_id.to_owned());
        Ok(())
    }

    async fn check_runs(&self, _sha: &str) -> Result<Vec<CheckRun>, ForgeError> {
        Ok(Vec::new())
    }

    async fn merging(&self, node_id: &str) -> Result<bool, ForgeError> {
        let merging = self.merging.lock().unwrap();
        Ok(merging.iter().any(|n| format!("PR_{n}") == node_id))
    }

    async fn close_pr(&self, number: u64, comment: &str) -> Result<(), ForgeError> {
        self.comments
            .lock()
            .unwrap()
            .push((number, comment.to_owned()));
        self.close(number);
        Ok(())
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
        self.batcher_with(db, |_| {})
    }

    /// A batcher with its config changed by `edit`.
    fn batcher_with<'w>(
        &'w self,
        db: &'w MirrorDb,
        edit: impl FnOnce(&mut BatchConfig),
    ) -> Batcher<'w, Api, FakeGitHub> {
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
            tags: None,
        };
        let mut config = BatchConfig::new(LINE, base, follow);
        edit(&mut config);
        Batcher::new(
            &self.api,
            cx(),
            db,
            Mirror::new(&self.mirror_dir, self.upstream.to_str().unwrap()),
            &self.github,
            config,
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
        self.land_by(files, message, n, "tess@example.com").await
    }

    /// [`World::land`] with the author's `email`.
    async fn land_by(
        &self,
        files: &[(&str, &str)],
        message: &str,
        n: i64,
        email: &str,
    ) -> ChangesetId {
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
                        email: email.to_owned(),
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
async fn a_batch_is_one_pr_and_the_next_forks_from_its_merge() {
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

    // One commit per land on GitHub's current main, holding the batch's
    // net change.
    let squash = support::branch(&w.upstream, "graft/batch-1").unwrap();
    assert_eq!(squash, rec.squash_sha.to_hex());
    assert_eq!(
        git(&w.upstream, &["rev-parse", &format!("{squash}~2")]),
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
async fn a_land_after_the_cut_is_appended_without_changing_earlier_shas() {
    let w = World::new().await;
    w.start().await;
    w.land(&[("a.txt", "a1\n")], "add a", 1).await;
    w.land(&[("b.txt", "b1\n")], "add b", 2).await;
    let first = opened(w.batcher().run().await.unwrap().outcome);
    let main = support::branch(&w.upstream, "main").unwrap();
    let before = chain_of(&w, &main, &first.squash_sha.to_hex());

    let appended = w.land(&[("c.txt", "c1\n")], "review fix", 3).await;
    let run = w.batcher().run().await.unwrap();
    let BatchOutcome::Waiting(rec) = run.outcome else {
        panic!("expected the open batch to wait")
    };

    assert_eq!((rec.number, rec.lands), (first.number, first.lands + 1));
    assert_eq!(w.github.prs().len(), 1);
    let after = chain_of(&w, &main, &rec.squash_sha.to_hex());
    assert_eq!(&after[..before.len()], before);
    assert_eq!(after.len(), before.len() + 1);
    let body = &w.github.prs()[0].new.body;
    assert!(
        body.contains(&format!("{} (appended)", short(appended))),
        "{body}"
    );
}

#[tokio::test]
async fn a_line_rewind_is_refused_instead_of_appended() {
    let w = World::new().await;
    w.start().await;
    let a = w.land(&[("a.txt", "a1\n")], "add a", 1).await;
    w.land(&[("b.txt", "b1\n")], "add b", 2).await;
    let rec = opened(w.batcher().run().await.unwrap().outcome);
    let sibling = w
        .api
        .create_changeset(
            &cx(),
            CreateChangeset {
                repo: REPO.to_owned(),
                parents: vec![a],
                changes: vec![Change::Put {
                    path: RepoPath::new("other.txt").unwrap(),
                    kind: EntryKind::File,
                    id: graft_model::ObjectId::hash(ObjectKind::Blob, b"other\n"),
                }],
                author: Author {
                    name: "Tess".to_owned(),
                    email: "tess@example.com".to_owned(),
                    kind: AuthorKind::Agent,
                },
                authored_at: Timestamp {
                    ms: 1_700_000_003_000,
                    offset_min: 0,
                },
                message: "divergent land".to_owned(),
                metadata: Metadata::new(),
                inline_blobs: vec![Frame::encode(ObjectKind::Blob, b"other\n")],
                workspace: None,
            },
        )
        .await
        .unwrap()
        .id;
    let mut divergent = rec.clone();
    divergent.tip = sibling;
    w.db.put_batch(&divergent).unwrap();

    let err = w.batcher().run().await.unwrap_err();
    assert!(
        matches!(err, MirrorError::Config(ref message) if message.contains("moved other than forward")),
        "{err:?}"
    );
    assert_eq!(
        support::branch(&w.upstream, &rec.branch).unwrap(),
        rec.squash_sha.to_hex()
    );
}

#[tokio::test]
async fn a_batch_github_is_merging_is_not_appended() {
    let w = World::new().await;
    w.start().await;
    w.land(&[("a.txt", "a1\n")], "add a", 1).await;
    let first = opened(w.batcher().run().await.unwrap().outcome);
    let head = support::branch(&w.upstream, &first.branch).unwrap();
    let body = w.github.prs()[0].new.body.clone();
    w.github
        .merging
        .lock()
        .unwrap()
        .push(first.pr_number.unwrap());
    let extra = w.land(&[("b.txt", "b1\n")], "review fix", 2).await;

    let run = w.batcher().run().await.unwrap();
    // One open batch per line: while GitHub is merging it, the next land
    // waits. It is not appended, and it is not cut into a second PR.
    assert!(run.settled.is_none(), "a merging batch is not settled");
    let BatchOutcome::Waiting(rec) = run.outcome else {
        panic!("expected to wait, got {:?}", run.outcome)
    };
    assert_eq!(rec.number, first.number);
    assert_eq!(rec.lands, first.lands);
    assert_eq!(w.github.prs().len(), 1);
    assert_eq!(support::branch(&w.upstream, &first.branch).unwrap(), head);
    assert_eq!(w.github.prs()[0].new.body, body);
    assert!(!w.github.prs()[0].new.body.contains(&short(extra)));
    assert_eq!(w.db.batch_commits(first.number).unwrap().len(), 1);
}

#[tokio::test]
async fn a_merged_batch_is_not_appended_and_the_next_land_starts_a_batch() {
    let w = World::new().await;
    w.start().await;
    w.land(&[("a.txt", "a1\n")], "add a", 1).await;
    let first = opened(w.batcher().run().await.unwrap().outcome);
    w.github.squash_merge(first.pr_number.unwrap());
    let extra = w.land(&[("b.txt", "b1\n")], "after merge", 2).await;

    let run = w.batcher().run().await.unwrap();
    assert_eq!(run.settled.unwrap().state, BatchState::Merged);
    let rec = opened(run.outcome);
    assert_eq!(rec.number, first.number + 1);
    assert_eq!(rec.lands, 1);
    assert!(w.github.prs()[1].new.body.contains(&short(extra)));
    assert!(!w.github.prs()[0].new.body.contains("(appended)"));
}

#[tokio::test]
async fn a_crash_after_the_record_re_pushes_with_the_lease_and_refreshes_the_body() {
    let w = World::new().await;
    w.start().await;
    w.land(&[("a.txt", "a1\n")], "add a", 1).await;
    let first = opened(w.batcher().run().await.unwrap().outcome);
    let stale_body = w.github.prs()[0].new.body.clone();
    let extra = w.land(&[("b.txt", "b1\n")], "review fix", 2).await;
    let run = w.batcher().run().await.unwrap();
    let BatchOutcome::Waiting(rec) = run.outcome else {
        panic!("expected the append to wait")
    };
    // The database and the branch head moved; the PR body did not, as if
    // the run died after the push.
    let head = rec.squash_sha.to_hex();
    git(
        &w.upstream,
        &[
            "update-ref",
            &format!("refs/heads/{}", rec.branch),
            &first.squash_sha.to_hex(),
        ],
    );
    {
        let mut prs = w.github.prs_mut();
        prs[0].new.body = stale_body.clone();
    }
    assert_ne!(support::branch(&w.upstream, &rec.branch).unwrap(), head);

    let again = w.batcher().run().await.unwrap();
    let BatchOutcome::Waiting(rec) = again.outcome else {
        panic!("expected to wait, got {:?}", again.outcome)
    };
    assert_eq!(rec.lands, first.lands + 1);
    assert_eq!(support::branch(&w.upstream, &rec.branch).unwrap(), head);
    let body = &w.github.prs()[0].new.body;
    assert!(
        body.contains(&format!("{} (appended)", short(extra))),
        "{body}"
    );
    assert_eq!(w.github.prs().len(), 1);
    // A second run with the body already current pushes nothing new.
    let third = w.batcher().run().await.unwrap();
    assert!(matches!(third.outcome, BatchOutcome::Waiting(_)));
    assert_eq!(support::branch(&w.upstream, &rec.branch).unwrap(), head);
}

#[tokio::test]
async fn a_concurrent_batch_branch_push_is_refused_by_the_lease() {
    let w = World::new().await;
    w.start().await;
    w.land(&[("a.txt", "a1\n")], "add a", 1).await;
    let rec = opened(w.batcher().run().await.unwrap().outcome);
    let head = rec.squash_sha.to_hex();
    let tree = git(&w.upstream, &["rev-parse", &format!("{head}^{{tree}}")]);
    let raced = git(
        &w.upstream,
        &["commit-tree", &tree, "-p", &head, "-m", "concurrent push"],
    );
    *w.github.race_push.lock().unwrap() = Some((1, rec.branch.clone(), raced.clone()));
    w.land(&[("b.txt", "b1\n")], "review fix", 2).await;

    let err = w.batcher().run().await.unwrap_err();
    assert!(
        matches!(err, MirrorError::Config(ref message) if message.contains("did not fast-forward from leased head")),
        "{err:?}"
    );
    assert_eq!(support::branch(&w.upstream, &rec.branch).unwrap(), raced);
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
        tags: None,
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

fn short(id: ChangesetId) -> String {
    id.to_string()[..13].to_owned()
}

#[tokio::test]
async fn an_excluded_land_is_reverted_and_appended_to_the_open_batch() {
    let w = World::new().await;
    w.start().await;
    let a = w.land(&[("a.txt", "a1\n")], "add a", 1).await;
    let b = w.land(&[("b.txt", "b1\n")], "add b", 2).await;
    let c = w.land(&[("c.txt", "c1\n")], "add c", 3).await;
    let first = opened(w.batcher().run().await.unwrap().outcome);
    assert_eq!(first.lands, 3);

    let run = w
        .batcher_with(&w.db, |c| {
            c.exclude = vec![b];
            c.drop_reason = Some("breaks the build".to_owned());
        })
        .run()
        .await
        .unwrap();
    let [(dropped, revert)] = run.dropped[..] else {
        panic!("one land dropped: {:?}", run.dropped)
    };
    assert_eq!(dropped, b);
    assert_eq!(w.tip(LINE).await, Some(revert), "the revert is a land");
    assert!(run.settled.is_none());
    let BatchOutcome::Waiting(rec) = run.outcome else {
        panic!("expected the open batch to wait")
    };
    assert_eq!((rec.number, rec.lands), (1, 4), "a, b, c and b's revert");
    assert!(w.github.comments().is_empty());
    assert!(w.github.prs()[0].open);
    let squash = support::branch(&w.upstream, "graft/batch-1").unwrap();
    let main = support::branch(&w.upstream, "main").unwrap();
    assert_eq!(w.changed(&main, &squash), ["a.txt", "c.txt"]);
    let body = &w.github.prs()[0].new.body;
    for text in [
        "Dropped: 1 land",
        &format!(
            "`{}` reverted by `{}`: breaks the build",
            short(b),
            short(revert)
        ),
        &short(a),
        &short(c),
        &format!("{} (appended)", short(revert)),
    ] {
        assert!(body.contains(text), "{text} missing from {body}");
    }

    // Excluding it again changes nothing: it is already reverted.
    let run = w
        .batcher_with(&w.db, |c| c.exclude = vec![b])
        .run()
        .await
        .unwrap();
    assert!(run.dropped.is_empty());
    assert!(matches!(run.outcome, BatchOutcome::Waiting(_)));
    // A changeset outside the batch is refused.
    let outside = w.tip("main").await.unwrap();
    let err = w
        .batcher_with(&w.db, |c| c.exclude = vec![outside])
        .run()
        .await;
    assert!(err.is_err());
}

#[tokio::test]
async fn max_lands_cuts_the_batch_and_the_rest_wait_for_the_next() {
    let w = World::new().await;
    w.start().await;
    let a = w.land(&[("a.txt", "a1\n")], "add a", 1).await;
    let b = w.land(&[("b.txt", "b1\n")], "add b", 2).await;
    let c = w.land(&[("c.txt", "c1\n")], "add c", 3).await;
    let capped = |c: &mut BatchConfig| c.max_lands = Some(2);
    let rec = opened(w.batcher_with(&w.db, capped).run().await.unwrap().outcome);
    assert_eq!((rec.lands, rec.tip), (2, b));
    let squash = support::branch(&w.upstream, "graft/batch-1").unwrap();
    let main = support::branch(&w.upstream, "main").unwrap();
    assert_eq!(w.changed(&main, &squash), ["a.txt", "b.txt"]);
    assert!(rec.body.contains("Cut at 2 lands: 1 newer land waits"));
    assert!(rec.body.contains(&short(a)) && !rec.body.contains(&short(c)));

    let merged = w.github.squash_merge(1);
    let run = w.batcher_with(&w.db, capped).run().await.unwrap();
    assert_eq!(run.settled.unwrap().state, BatchState::Merged);
    let rec = opened(run.outcome);
    assert_eq!((rec.lands, rec.tip), (1, c));
    let squash = support::branch(&w.upstream, "graft/batch-2").unwrap();
    assert_eq!(w.changed(&merged, &squash), ["c.txt"]);
}

#[tokio::test]
async fn a_cut_before_a_sync_exports_against_the_base_the_line_held_there() {
    let w = World::new().await;
    w.start().await;
    let first_main = support::branch(&w.upstream, "main").unwrap();
    let a = w.land(&[("a.txt", "a1\n")], "add a", 1).await;
    w.push_main(&[("m.txt", "m1\n")], "upstream");
    // Held back by age, but the line syncs onto the moved main.
    let run = w
        .batcher_with(&w.db, |c| c.max_age_ms = Some(i64::MAX))
        .run()
        .await
        .unwrap();
    assert!(run.synced.is_some());
    assert!(matches!(run.outcome, BatchOutcome::NotDue { lands: 1, .. }));
    w.land(&[("b.txt", "b1\n")], "add b", 2).await;
    let rec = opened(
        w.batcher_with(&w.db, |c| c.max_lands = Some(1))
            .run()
            .await
            .unwrap()
            .outcome,
    );
    assert_eq!((rec.lands, rec.tip), (1, a));
    assert_eq!(rec.base_sha.to_hex(), first_main);
    let squash = support::branch(&w.upstream, "graft/batch-1").unwrap();
    assert_eq!(w.changed(&first_main, &squash), ["a.txt"]);
}

#[tokio::test]
async fn max_age_holds_a_young_batch_until_it_is_old_enough_or_full() {
    let w = World::new().await;
    w.start().await;
    w.land(&[("a.txt", "a1\n")], "add a", 1).await;
    // World::land authors land n at 1_700_000_000_000 + n s.
    let age = clock() - 1_700_000_001_000 + 1;
    let run = w
        .batcher_with(&w.db, |c| c.max_age_ms = Some(age))
        .run()
        .await
        .unwrap();
    assert!(matches!(run.outcome, BatchOutcome::NotDue { lands: 1, .. }));
    assert!(w.github.prs().is_empty());
    // Full by count: due regardless of age.
    w.land(&[("b.txt", "b1\n")], "add b", 2).await;
    let rec = opened(
        w.batcher_with(&w.db, |c| {
            c.max_age_ms = Some(i64::MAX);
            c.max_lands = Some(2);
        })
        .run()
        .await
        .unwrap()
        .outcome,
    );
    assert_eq!(rec.lands, 2);
}

#[tokio::test]
async fn a_cut_never_splits_a_dropped_land_from_its_revert() {
    let w = World::new().await;
    w.start().await;
    let a = w.land(&[("a.txt", "a1\n")], "add a", 1).await;
    w.land(&[("b.txt", "b1\n")], "add b", 2).await;
    let run = w
        .batcher_with(&w.db, |c| {
            c.exclude = vec![a];
            c.max_lands = Some(1);
        })
        .run()
        .await
        .unwrap();
    let revert = run.dropped[0].1;
    let rec = opened(run.outcome);
    assert_eq!((rec.lands, rec.tip), (3, revert));
    let squash = support::branch(&w.upstream, "graft/batch-1").unwrap();
    let main = support::branch(&w.upstream, "main").unwrap();
    assert_eq!(w.changed(&main, &squash), ["b.txt"]);
}

#[tokio::test]
async fn a_drop_that_conflicts_part_way_leaves_the_open_batch_unchanged() {
    let w = World::new().await;
    w.start().await;
    let b = w
        .land(&[("shared.txt", "one\nB\nthree\n")], "edit shared", 1)
        .await;
    w.land(&[("shared.txt", "one\nC\nthree\n")], "edit shared again", 2)
        .await;
    let a = w.land(&[("a.txt", "a1\n")], "add a", 3).await;
    let first = opened(w.batcher().run().await.unwrap().outcome);
    let first_head = first.squash_sha;

    // Newest first: a reverts, then b conflicts with the edit after it.
    let run = w
        .batcher_with(&w.db, |c| c.exclude = vec![b, a])
        .run()
        .await
        .unwrap();
    assert_eq!(run.dropped.len(), 1);
    assert_eq!(run.dropped[0].0, a);
    assert!(matches!(run.outcome, BatchOutcome::Conflict { .. }));
    assert!(run.settled.is_none());
    assert!(w.github.prs()[0].open);
    assert_eq!(
        support::branch(&w.upstream, &first.branch).unwrap(),
        first_head.to_hex()
    );
}

#[tokio::test]
async fn a_revert_left_by_an_interrupted_run_is_appended_to_the_open_batch() {
    let w = World::new().await;
    w.start().await;
    let a = w.land(&[("a.txt", "a1\n")], "add a", 1).await;
    w.land(&[("b.txt", "b1\n")], "add b", 2).await;
    opened(w.batcher().run().await.unwrap().outcome);
    // A run reverted a on the line, then died before closing the PR.
    let tip = w.tip(LINE).await;
    let revert = w
        .api
        .revert(
            &cx(),
            graft_proto::types::Revert {
                repo: REPO.to_owned(),
                changeset: a,
                onto: tip.unwrap(),
                workspace: None,
            },
        )
        .await
        .unwrap();
    move_line(&w.api, LINE, tip, revert).await;

    let run = w.batcher().run().await.unwrap();
    assert!(run.dropped.is_empty());
    assert!(run.settled.is_none());
    let BatchOutcome::Waiting(rec) = run.outcome else {
        panic!("expected the open batch to wait")
    };
    let squash = support::branch(&w.upstream, &rec.branch).unwrap();
    let main = support::branch(&w.upstream, "main").unwrap();
    assert_eq!(w.changed(&main, &squash), ["b.txt"]);
    assert!(rec.body.contains("(appended)"), "{}", rec.body);
}

#[tokio::test]
async fn a_batch_github_is_merging_is_not_closed() {
    let w = World::new().await;
    w.start().await;
    let a = w.land(&[("a.txt", "a1\n")], "add a", 1).await;
    let first = opened(w.batcher().run().await.unwrap().outcome);
    w.github
        .merging
        .lock()
        .unwrap()
        .push(first.pr_number.unwrap());
    let run = w
        .batcher_with(&w.db, |c| c.exclude = vec![a])
        .run()
        .await
        .unwrap();
    assert_eq!(run.dropped.len(), 1, "the line still drops it");
    match run.outcome {
        BatchOutcome::Waiting(rec) => {
            assert!(rec.detail.contains("waiting for review"), "{}", rec.detail)
        }
        other => panic!("expected to wait, got {other:?}"),
    }
    assert!(w.github.prs()[0].open);
    assert!(w.github.comments().is_empty());
}

#[tokio::test]
async fn a_merged_cut_before_a_sync_does_not_reapply_its_lands() {
    let w = World::new().await;
    w.start().await;
    w.land(&[("a.txt", "a1\n")], "add a", 1).await;
    w.push_main(&[("m.txt", "m1\n")], "upstream");
    // Held by age, the line syncs onto the moved main; then b lands and a
    // one-land batch is cut before the sync.
    let held = |c: &mut BatchConfig| c.max_age_ms = Some(i64::MAX);
    let run = w.batcher_with(&w.db, held).run().await.unwrap();
    assert!(run.synced.is_some());
    w.land(&[("b.txt", "b1\n")], "add b", 2).await;
    let rec = opened(
        w.batcher_with(&w.db, |c| c.max_lands = Some(1))
            .run()
            .await
            .unwrap()
            .outcome,
    );
    assert_eq!(rec.lands, 1);
    // The operator merges it, main moves on, and an agent edits the file
    // the merged batch added: re-applying the batch would conflict on it.
    w.github.squash_merge(rec.pr_number.unwrap());
    git(&w.work, &["fetch", "-q", "origin"]);
    git(&w.work, &["reset", "-q", "--hard", "origin/main"]);
    w.push_main(&[("n.txt", "n1\n")], "after the merge");
    w.land(&[("a.txt", "a2\n")], "more a", 3).await;

    let run = w.batcher().run().await.unwrap();
    assert_eq!(run.settled.unwrap().state, BatchState::Merged);
    assert!(run.synced.is_some());
    let rec = opened(run.outcome);
    assert_eq!(rec.lands, 2, "b and the new a edit");
    let squash = support::branch(&w.upstream, &rec.branch).unwrap();
    let main = support::branch(&w.upstream, "main").unwrap();
    assert_eq!(w.changed(&main, &squash), ["a.txt", "b.txt"]);
    assert_eq!(w.show(&squash, "a.txt"), "a2");
    assert!(w.files_at(&squash).contains(&"n.txt".to_owned()));
    // The sync that rebuilt the starting point cleared it: nothing unlanded
    // is kept as a sync point across runs.
    let state = w.db.batch_line(LINE).unwrap().unwrap();
    assert_eq!(state.sync_batch, None);
    assert_ne!(state.sync_from, state.start);
}

#[test]
fn a_db_from_before_batch_cuts_opens_and_its_lines_are_not_cut() {
    let tmp = tempfile::tempdir().unwrap();
    let path = tmp.path().join("old.sqlite");
    let id = ChangesetId(graft_model::ObjectId([3; 32]));
    {
        let conn = rusqlite::Connection::open(&path).unwrap();
        conn.execute_batch(
            "CREATE TABLE batch_lines (
                line TEXT PRIMARY KEY,
                synced BLOB NOT NULL,
                sync_from BLOB NOT NULL,
                start BLOB NOT NULL
            ) WITHOUT ROWID;",
        )
        .unwrap();
        conn.execute(
            "INSERT INTO batch_lines VALUES ('integration', ?1, ?1, ?1)",
            [&id.0.0[..]],
        )
        .unwrap();
    }
    let db = MirrorDb::open(&path).unwrap();
    let state = db.batch_line(LINE).unwrap().unwrap();
    assert_eq!((state.synced, state.start), (id, id));
    assert_eq!(state.start_base, None, "unknown, not guessed");
    assert_eq!(state.sync_batch, None);
    db.put_batch_drop(LINE, id, id, "why").unwrap();
    assert_eq!(
        db.batch_drop_reason(LINE, id).unwrap().as_deref(),
        Some("why")
    );
    let mut next = state;
    next.start_base = Some(id);
    next.sync_batch = Some(7);
    db.set_batch_line(LINE, &next).unwrap();
    drop(db);
    let db = MirrorDb::open(&path).unwrap();
    let back = db.batch_line(LINE).unwrap().unwrap();
    assert_eq!((back.start_base, back.sync_batch), (Some(id), Some(7)));
}

#[tokio::test]
async fn a_batch_of_three_lands_is_three_commits_and_a_rebase_merge_maps_back() {
    let w = World::new().await;
    w.start().await;
    let a = w.land(&[("a.txt", "a1\n")], "add a", 1).await;
    let b = w
        .land(&[("b.txt", "b1\n")], "add b\n\nwith a body", 2)
        .await;
    let c = w.land(&[("a.txt", "a2\n")], "change a", 3).await;
    let main = support::branch(&w.upstream, "main").unwrap();

    let rec = opened(w.batcher().run().await.unwrap().outcome);
    assert_eq!(rec.lands, 3);
    let commits = w.db.batch_commits(rec.number).unwrap();
    assert_eq!(
        commits.iter().map(|c| c.land).collect::<Vec<_>>(),
        [a, b, c],
        "one commit per land, oldest first"
    );
    let head = support::branch(&w.upstream, "graft/batch-1").unwrap();
    assert_eq!(head, commits[2].sha.to_hex());
    let chain = git(
        &w.upstream,
        &["rev-list", "--reverse", &format!("{main}..{head}")],
    );
    let chain: Vec<&str> = chain.lines().collect();
    assert_eq!(
        chain,
        commits.iter().map(|c| c.sha.to_hex()).collect::<Vec<_>>()
    );
    // Each commit is its land's own change, with its author and trailers.
    assert_eq!(w.changed(&main, chain[0]), ["a.txt"]);
    assert_eq!(w.changed(chain[0], chain[1]), ["b.txt"]);
    assert_eq!(w.changed(chain[1], chain[2]), ["a.txt"]);
    assert_eq!(w.show(chain[2], "a.txt"), "a2");
    let message = git(&w.upstream, &["log", "-1", "--format=%B", chain[1]]);
    assert!(message.starts_with("add b\n\nwith a body\n"), "{message}");
    for text in [
        format!("Graft-Changeset: {b}"),
        "Graft-Session: ses_2".to_owned(),
        "Graft-Work-Item: WI-2".to_owned(),
        "Graft-Batch: 1".to_owned(),
    ] {
        assert!(message.contains(&text), "{text} missing from {message}");
    }
    assert!(!message.contains("Graft-Checks"), "{message}");
    let who = git(
        &w.upstream,
        &["log", "-1", "--format=%an <%ae> %at", chain[1]],
    );
    assert_eq!(who, "Tess <tess@example.com> 1700000002");
    let body = &w.github.prs()[0].new.body;
    assert!(body.contains("one commit per land"), "{body}");
    for id in [a, b, c] {
        assert!(body.contains(&short(id)), "the land table keeps every land");
    }

    // land-pr rebase-merges it: new shas, same trees; each maps back.
    let merged = w.github.rebase_merge(1);
    let run = w.batcher().run().await.unwrap();
    let settled = run.settled.expect("batch 1 settled");
    assert_eq!(settled.state, BatchState::Merged);
    assert_eq!(settled.merge_sha.unwrap().to_hex(), merged);
    let mapped = w.db.batch_commits(1).unwrap();
    let new = git(
        &w.upstream,
        &["rev-list", "--reverse", &format!("{main}..{merged}")],
    );
    let new: Vec<&str> = new.lines().collect();
    assert_eq!(new.len(), 3);
    for (c, sha) in mapped.iter().zip(&new) {
        assert_eq!(c.merged.map(|s| s.to_hex()).as_deref(), Some(*sha));
        assert_ne!(c.merged, Some(c.sha), "a rebase-merge makes new commits");
    }
    assert_eq!([mapped[0].land, mapped[1].land, mapped[2].land], [a, b, c]);
}

#[tokio::test]
async fn a_land_with_no_author_email_is_authored_by_the_mirror() {
    let w = World::new().await;
    w.start().await;
    w.land_by(&[("a.txt", "a1\n")], "add a", 1, "").await;
    opened(w.batcher().run().await.unwrap().outcome);
    let head = support::branch(&w.upstream, "graft/batch-1").unwrap();
    let who = git(&w.upstream, &["log", "-1", "--format=%an <%ae>", &head]);
    assert_eq!(who, "graft <graft@graft.invalid>", "never Name <unknown>");
    let message = git(&w.upstream, &["log", "-1", "--format=%B", &head]);
    assert!(message.contains("Graft-Session: ses_1"), "{message}");
}

#[tokio::test]
async fn a_dropped_land_and_its_revert_are_each_a_commit() {
    let w = World::new().await;
    w.start().await;
    let a = w.land(&[("a.txt", "a1\n")], "add a", 1).await;
    let b = w.land(&[("b.txt", "b1\n")], "add b", 2).await;
    let main = support::branch(&w.upstream, "main").unwrap();
    let rec = opened(
        w.batcher_with(&w.db, |c| c.exclude = vec![a])
            .run()
            .await
            .unwrap()
            .outcome,
    );
    let commits = w.db.batch_commits(rec.number).unwrap();
    assert_eq!(commits.len(), 3, "a, b and a's revert");
    assert_eq!([commits[0].land, commits[1].land], [a, b]);
    let head = commits[2].sha.to_hex();
    assert_eq!(w.changed(&main, &head), ["b.txt"]);
    assert_eq!(w.changed(&commits[1].sha.to_hex(), &head), ["a.txt"]);
}

/// The batch branch's commits after `main`, oldest first.
fn chain_of(w: &World, main: &str, head: &str) -> Vec<String> {
    git(
        &w.upstream,
        &["rev-list", "--reverse", &format!("{main}..{head}")],
    )
    .lines()
    .map(str::to_owned)
    .collect()
}

#[tokio::test]
async fn a_land_commit_merges_by_line_and_never_clobbers_an_upstream_change() {
    let w = World::new().await;
    w.start().await;
    let a = w
        .land(&[("shared.txt", "ONE\ntwo\nthree\n")], "line one", 1)
        .await;
    // GitHub's main changes another line of the same file.
    let main = w.push_main(&[("shared.txt", "one\ntwo\nTHREE\n")], "line three");
    let rec = opened(w.batcher().run().await.unwrap().outcome);
    let commits = w.db.batch_commits(rec.number).unwrap();
    assert_eq!(commits.len(), 1, "per-land, not the squash fallback");
    assert_eq!(
        w.show(&commits[0].sha.to_hex(), "shared.txt"),
        "ONE\ntwo\nTHREE",
        "the land's commit keeps main's change"
    );

    // Recut with a later land on the same file: the earlier land's commit
    // still keeps main's line, not only the batch's last commit.
    w.github.close(rec.pr_number.unwrap());
    let b = w
        .land(&[("shared.txt", "ONE\nTWO\nTHREE\n")], "line two", 2)
        .await;
    let rec = opened(w.batcher().run().await.unwrap().outcome);
    let commits = w.db.batch_commits(rec.number).unwrap();
    assert_eq!([commits[0].land, commits[1].land], [a, b]);
    let chain = chain_of(&w, &main, &rec.squash_sha.to_hex());
    assert_eq!(w.show(&chain[0], "shared.txt"), "ONE\ntwo\nTHREE");
    assert_eq!(w.show(&chain[1], "shared.txt"), "ONE\nTWO\nTHREE");
    assert_eq!(w.changed(&main, &chain[0]), ["shared.txt"]);
}

#[tokio::test]
async fn a_land_that_does_not_replay_squashes_the_batch_and_says_why() {
    let w = World::new().await;
    w.start().await;
    // a and its undo net to no change on line two, so the line syncs; but a
    // alone conflicts with main's line two.
    w.land(&[("shared.txt", "one\nX\nthree\n")], "x", 1).await;
    w.land(
        &[("b.txt", "b\n"), ("shared.txt", "one\ntwo\nthree\n")],
        "undo x, add b",
        2,
    )
    .await;
    let main = w.push_main(&[("shared.txt", "one\nY\nthree\n")], "y");
    let rec = opened(w.batcher().run().await.unwrap().outcome);
    assert_eq!(rec.lands, 2);
    assert!(w.db.batch_commits(rec.number).unwrap().is_empty());
    let head = rec.squash_sha.to_hex();
    assert_eq!(
        chain_of(&w, &main, &head),
        std::slice::from_ref(&head),
        "one commit"
    );
    assert_eq!(w.show(&head, "shared.txt"), "one\nY\nthree");
    let body = &w.github.prs()[0].new.body;
    assert!(body.contains("squashed into one commit"), "{body}");
    assert!(body.contains("does not merge onto the base"), "{body}");
    let message = git(&w.upstream, &["log", "-1", "--format=%B", &head]);
    assert!(
        message.contains("does not merge onto the base"),
        "{message}"
    );
}

#[tokio::test]
async fn a_merge_commit_maps_each_land_to_its_own_commit() {
    let w = World::new().await;
    w.start().await;
    w.land(&[("a.txt", "a1\n")], "add a", 1).await;
    w.land(&[("b.txt", "b1\n")], "add b", 2).await;
    let rec = opened(w.batcher().run().await.unwrap().outcome);
    let merged = w.github.merge_commit(rec.pr_number.unwrap());
    let run = w.batcher().run().await.unwrap();
    let settled = run.settled.expect("settled");
    assert_eq!(settled.state, BatchState::Merged);
    assert_eq!(settled.merge_sha.unwrap().to_hex(), merged);
    let mapped = w.db.batch_commits(rec.number).unwrap();
    assert_eq!(mapped.len(), 2);
    for c in &mapped {
        assert_eq!(c.merged, Some(c.sha), "a merge commit keeps the shas");
    }
}

#[tokio::test]
async fn a_batch_carries_at_most_500_lands_and_the_rest_wait() {
    let w = World::new().await;
    w.start().await;
    for n in 0..501 {
        w.land(&[(&format!("f{n}.txt"), "x\n")], &format!("add f{n}"), n)
            .await;
    }
    let rec = opened(w.batcher().run().await.unwrap().outcome);
    assert_eq!(rec.lands, 500);
    let commits = w.db.batch_commits(rec.number).unwrap();
    assert_eq!(commits.len(), 500);
    assert_eq!(commits.last().unwrap().sha, rec.squash_sha);
    let body = &w.github.prs()[0].new.body;
    assert!(
        body.contains("Cut at 500 lands: 1 newer land waits"),
        "{body}"
    );
}

#[tokio::test]
async fn a_retried_push_pushes_the_recorded_shas() {
    let w = World::new().await;
    w.start().await;
    for n in 1..=3 {
        w.land(&[(&format!("f{n}.txt"), "x\n")], "add", n).await;
    }
    let main = support::branch(&w.upstream, "main").unwrap();
    let rec = opened(w.batcher().run().await.unwrap().outcome);
    let recorded = w.db.batch_commits(rec.number).unwrap();
    // As if the run stopped after recording, before the push reached
    // GitHub.
    git(
        &w.upstream,
        &["update-ref", "-d", "refs/heads/graft/batch-1"],
    );
    let mut planned = rec.clone();
    planned.state = BatchState::Planned;
    planned.pr_number = None;
    planned.pr_url = None;
    w.db.put_batch(&planned).unwrap();
    let again = w.batcher().track().await.unwrap().unwrap();
    assert_eq!(again.state, BatchState::Open);
    let head = support::branch(&w.upstream, "graft/batch-1").unwrap();
    assert_eq!(head, rec.squash_sha.to_hex());
    assert_eq!(
        chain_of(&w, &main, &head),
        recorded.iter().map(|c| c.sha.to_hex()).collect::<Vec<_>>()
    );
    assert_eq!(w.db.batch_commits(rec.number).unwrap(), recorded);
}
