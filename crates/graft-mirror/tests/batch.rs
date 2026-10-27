//! Batch mode (decision D21) against a local bare repository standing in for
//! GitHub's git, and a fake GitHub API holding the PRs. The graft side is an
//! in-process `graft-engine` over the memory store.

mod support;

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use graft_engine::policy::LineChecks;
use graft_engine::{Engine, EngineConfig, LineConfig};
use graft_git::GitSha;
use graft_mirror::batch::{BatchConfig, BatchOutcome, BatchRecord, BatchState, Batcher};
use graft_mirror::credentials::SecretToken;
use graft_mirror::follow::{FollowConfig, FollowLine, POLL_INTERVAL};
use graft_mirror::github::{CheckRun, Forge, ForgeError, NewPull, Pull};
use graft_mirror::{ExportLog, MAX_RECORD_EXPORTS, Mirror, MirrorDb, MirrorError, current_ref};
use graft_model::{
    Author, AuthorKind, Change, ChangesetId, EntryKind, Metadata, ObjectKind, RepoPath, Timestamp,
};
use graft_proto::api::{AdminApi, ObjectApi, RepoApi};
use graft_proto::types::*;
use graft_proto::{ApiError, CallContext, Frame};
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
    /// `(branch, skip)`: after `skip` PR lookups for `branch` answer as
    /// usual, a merged PR graft did not open appears on it.
    foreign_pr: Mutex<Option<(String, usize)>>,
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
            foreign_pr: Mutex::new(None),
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
        let mut prs = self.prs.lock().unwrap();
        let mut foreign = self.foreign_pr.lock().unwrap();
        if let Some((b, skip)) = foreign.as_mut()
            && b == branch
        {
            if *skip == 0 {
                let main = support::branch(&self.upstream, "main").unwrap();
                let number = prs.len() as u64 + 1;
                prs.push(FakePr {
                    number,
                    new: NewPull {
                        title: "someone else's PR".to_owned(),
                        body: String::new(),
                        head: branch.to_owned(),
                        base: "main".to_owned(),
                    },
                    open: false,
                    merged: Some(main),
                });
                *foreign = None;
            } else {
                *skip -= 1;
            }
        }
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
        World::with_config(EngineConfig::default()).await
    }

    /// A world whose integration line has post-land checks on `ci/test`
    /// with no retry (design §8.7), batched with `checked` set.
    async fn checked() -> World {
        let mut line = LineConfig::direct(LINE);
        line.checks = LineChecks::PostLand {
            required: vec!["ci/test".to_owned()],
            retries: 0,
        };
        World::with_config(EngineConfig {
            lines: vec![line],
            ..EngineConfig::default()
        })
        .await
    }

    async fn with_config(config: EngineConfig) -> World {
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
            config,
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
        self.batcher_over(&self.api, db, edit)
    }

    /// [`World::batcher_with`] reading graft through `api`.
    fn batcher_over<'w, A: AdminApi + ObjectApi + RepoApi>(
        &'w self,
        api: &'w A,
        db: &'w MirrorDb,
        edit: impl FnOnce(&mut BatchConfig),
    ) -> Batcher<'w, A, FakeGitHub> {
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
        config.authors.insert(
            "Tess".to_owned(),
            graft_mirror::batch::AuthorIdentity {
                name: "Tess".to_owned(),
                email: "tess@example.com".to_owned(),
            },
        );
        edit(&mut config);
        Batcher::new(
            api,
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
        self.land_as_subject("Tess", "Tess", files, message, n, email)
            .await
    }

    /// A land whose author name is `principal` and whose email may be empty.
    /// `extra` is `(model, coauthors)` when the land records them.
    async fn land_as(
        &self,
        principal: &str,
        files: &[(&str, &str)],
        message: &str,
        n: i64,
        email: &str,
        extra: Option<(&str, &str)>,
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
        if let Some((model, coauthors)) = extra {
            if !model.is_empty() {
                metadata.insert("vogt.model".to_owned(), model.to_owned());
            }
            if !coauthors.is_empty() {
                metadata.insert("vogt.coauthors".to_owned(), coauthors.to_owned());
            }
        }
        let id = self
            .api
            .create_changeset(
                &cx(),
                CreateChangeset {
                    repo: REPO.to_owned(),
                    parents: tip.into_iter().collect(),
                    changes,
                    author: Author {
                        name: principal.to_owned(),
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
        let mut call = cx();
        call.principal.subject = principal.to_owned();
        self.api
            .update_ref(
                &call,
                UpdateRef {
                    repo: REPO.to_owned(),
                    name: LINE.to_owned(),
                    expected: tip,
                    new: id,
                    kind: RefKind::Line,
                },
            )
            .await
            .unwrap();
        id
    }

    /// Like [`World::land_as`], but the line moves under token subject
    /// `subject`. The changeset's author is still the client-supplied name.
    async fn land_as_subject(
        &self,
        subject: &str,
        claimed: &str,
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
                        name: claimed.to_owned(),
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
        let mut call = cx();
        call.principal.subject = subject.to_owned();
        self.api
            .update_ref(
                &call,
                UpdateRef {
                    repo: REPO.to_owned(),
                    name: LINE.to_owned(),
                    expected: tip,
                    new: id,
                    kind: RefKind::Line,
                },
            )
            .await
            .unwrap();
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
        matches!(err, MirrorError::Append { ref detail, .. } if detail.contains("did not fast-forward from leased head")),
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
                check_url: None,
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
            assert!(rec.detail.starts_with("sealed"), "{}", rec.detail)
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
async fn two_principals_export_as_mapped_authors_with_trailers() {
    let w = World::new().await;
    w.start().await;
    w.land_as(
        "ada",
        &[("a.txt", "a1\n")],
        "add a",
        1,
        "",
        Some(("gpt-test", "Lin <lin@example.com>")),
    )
    .await;
    w.land_as("grace", &[("b.txt", "b1\n")], "add b", 2, "", None)
        .await;
    let rec = opened(
        w.batcher_with(&w.db, |c| {
            c.authors = [
                (
                    "ada".to_owned(),
                    graft_mirror::batch::AuthorIdentity {
                        name: "Ada Lovelace".to_owned(),
                        email: "ada@example.com".to_owned(),
                    },
                ),
                (
                    "grace".to_owned(),
                    graft_mirror::batch::AuthorIdentity {
                        name: "Grace Hopper".to_owned(),
                        email: "grace@example.com".to_owned(),
                    },
                ),
            ]
            .into_iter()
            .collect();
        })
        .run()
        .await
        .unwrap()
        .outcome,
    );
    let commits = w.db.batch_commits(rec.number).unwrap();
    assert_eq!(commits.len(), 2);
    let who = |sha: &graft_git::GitSha| {
        git(
            &w.upstream,
            &["log", "-1", "--format=%an <%ae>", &sha.to_hex()],
        )
    };
    let body =
        |sha: &graft_git::GitSha| git(&w.upstream, &["log", "-1", "--format=%B", &sha.to_hex()]);
    assert_eq!(who(&commits[0].sha), "Ada Lovelace <ada@example.com>");
    assert_eq!(who(&commits[1].sha), "Grace Hopper <grace@example.com>");
    let first = body(&commits[0].sha);
    assert!(first.contains("Graft-Session: ses_1"), "{first}");
    assert!(first.contains("Graft-Work-Item: WI-1"), "{first}");
    assert!(first.contains("Graft-Model: gpt-test"), "{first}");
    assert!(
        first.contains("Co-authored-by: Lin <lin@example.com>"),
        "{first}"
    );
    let second = body(&commits[1].sha);
    assert!(second.contains("Graft-Session: ses_2"), "{second}");
    assert!(second.contains("Graft-Work-Item: WI-2"), "{second}");
    assert!(!second.contains("Graft-Model:"), "{second}");
    assert!(!second.contains("Co-authored-by:"), "{second}");
}

#[tokio::test]
async fn claimed_author_is_only_a_trailer_of_the_authenticated_principals() {
    let w = World::new().await;
    w.start().await;
    w.land_as_subject(
        "subject-a",
        "Claimed C",
        &[("a.txt", "a1\n")],
        "add a",
        1,
        "c@example.com",
    )
    .await;
    w.land_as_subject(
        "subject-b",
        "Claimed C",
        &[("b.txt", "b1\n")],
        "add b",
        2,
        "c@example.com",
    )
    .await;
    let rec = opened(
        w.batcher_with(&w.db, |c| {
            c.authors = [
                (
                    "subject-a".to_owned(),
                    graft_mirror::batch::AuthorIdentity {
                        name: "Author A".to_owned(),
                        email: "a@example.com".to_owned(),
                    },
                ),
                (
                    "subject-b".to_owned(),
                    graft_mirror::batch::AuthorIdentity {
                        name: "Author B".to_owned(),
                        email: "b@example.com".to_owned(),
                    },
                ),
            ]
            .into_iter()
            .collect();
        })
        .run()
        .await
        .unwrap()
        .outcome,
    );
    let commits = w.db.batch_commits(rec.number).unwrap();
    assert_eq!(commits.len(), 2);
    let who = |sha: &graft_git::GitSha| {
        git(
            &w.upstream,
            &["log", "-1", "--format=%an <%ae>", &sha.to_hex()],
        )
    };
    let body =
        |sha: &graft_git::GitSha| git(&w.upstream, &["log", "-1", "--format=%B", &sha.to_hex()]);
    assert_eq!(who(&commits[0].sha), "Author A <a@example.com>");
    assert_eq!(who(&commits[1].sha), "Author B <b@example.com>");
    for sha in [&commits[0].sha, &commits[1].sha] {
        let message = body(sha);
        assert!(
            message.contains("Claimed-Author: Claimed C <c@example.com>"),
            "{message}"
        );
        assert!(
            !message.contains("c@example.com>") || message.contains("Claimed-Author:"),
            "{message}"
        );
        assert!(!who(sha).contains("Claimed"), "{}", who(sha));
    }
}

#[tokio::test]
async fn an_unmapped_principal_exports_as_default_author() {
    let w = World::new().await;
    w.start().await;
    w.land_as_subject(
        "subject-z",
        "Zed",
        &[("a.txt", "a1\n")],
        "add a",
        1,
        "z@example.com",
    )
    .await;
    let rec = opened(
        w.batcher_with(&w.db, |c| {
            c.authors.insert(
                "graft-mirror".to_owned(),
                graft_mirror::batch::AuthorIdentity {
                    name: "Should Not Win".to_owned(),
                    email: "mirror@example.com".to_owned(),
                },
            );
            c.default_author = Some(graft_mirror::batch::AuthorIdentity {
                name: "Default Author".to_owned(),
                email: "default@example.com".to_owned(),
            });
        })
        .run()
        .await
        .unwrap()
        .outcome,
    );
    let commits = w.db.batch_commits(rec.number).unwrap();
    assert_eq!(commits.len(), 1);
    let who = git(
        &w.upstream,
        &["log", "-1", "--format=%an <%ae>", &commits[0].sha.to_hex()],
    );
    assert_eq!(who, "Default Author <default@example.com>");
    let message = git(
        &w.upstream,
        &["log", "-1", "--format=%B", &commits[0].sha.to_hex()],
    );
    assert!(
        message.contains("Claimed-Author: Zed <z@example.com>"),
        "{message}"
    );
    assert!(!message.contains("subject-z"), "{message}");
}

#[tokio::test]
async fn a_missing_principal_falls_back_to_default_author() {
    let w = World::new().await;
    w.start().await;
    // `land` moves the line as the mirror service, then we export with no
    // mapping for that subject and a default author.
    w.land_as_subject("nobody", "nobody", &[("a.txt", "a1\n")], "add a", 1, "")
        .await;
    let rec = opened(
        w.batcher_with(&w.db, |c| {
            c.default_author = Some(graft_mirror::batch::AuthorIdentity {
                name: "Old Default".to_owned(),
                email: "old@example.com".to_owned(),
            });
        })
        .run()
        .await
        .unwrap()
        .outcome,
    );
    let commits = w.db.batch_commits(rec.number).unwrap();
    let who = git(
        &w.upstream,
        &["log", "-1", "--format=%an <%ae>", &commits[0].sha.to_hex()],
    );
    // The actor is present (the test caller). An unmapped known principal
    // uses default_author, which is also the fallback when the actor is absent.
    assert_eq!(who, "Old Default <old@example.com>");
}

#[tokio::test]
async fn a_land_with_no_author_email_is_authored_by_the_mirror() {
    let w = World::new().await;
    w.start().await;
    w.land_as_subject("nobody", "nobody", &[("a.txt", "a1\n")], "add a", 1, "")
        .await;
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

/// WI-2577: every append to one open batch reaches the remote branch, not
/// only the first.
#[tokio::test]
async fn repeated_appends_each_advance_the_batch_branch() {
    let w = World::new().await;
    w.start().await;
    w.land(&[("a.txt", "a1\n")], "add a", 1).await;
    let first = opened(w.batcher().run().await.unwrap().outcome);
    let mut head = support::branch(&w.upstream, &first.branch).unwrap();
    for n in 0..3u64 {
        let file = format!("f{n}.txt");
        let land = w
            .land(&[(file.as_str(), "x\n")], "review fix", 2 + n as i64)
            .await;
        let run = w.batcher().run().await.unwrap();
        let BatchOutcome::Waiting(rec) = run.outcome else {
            panic!(
                "append {n}: expected the open batch to wait, got {:?}",
                run.outcome
            )
        };
        let now = support::branch(&w.upstream, &first.branch).unwrap();
        assert_ne!(now, head, "append {n} did not move the branch");
        assert_eq!(now, rec.squash_sha.to_hex(), "append {n}");
        assert_eq!(rec.lands, first.lands + n + 1, "append {n}");
        assert_eq!(rec.tip, land, "append {n}");
        assert_eq!(
            w.db.batch_commits(first.number).unwrap().len() as u64,
            rec.lands,
            "append {n}"
        );
        let parent = git(&w.upstream, &["rev-parse", &format!("{now}^")]);
        assert_eq!(parent, head, "append {n} is one commit on the old head");
        head = now;
    }
}

/// WI-2577: a run that takes no append says why. It never repeats the
/// detail of an earlier append, which read as a push that did not happen.
#[tokio::test]
async fn an_append_skipped_after_an_append_reports_this_run() {
    let w = World::new().await;
    w.start().await;
    w.land(&[("a.txt", "a1\n")], "add a", 1).await;
    let first = opened(w.batcher().run().await.unwrap().outcome);
    w.land(&[("b.txt", "b1\n")], "review fix", 2).await;
    let BatchOutcome::Waiting(rec) = w.batcher().run().await.unwrap().outcome else {
        panic!("expected the open batch to wait")
    };
    assert!(rec.detail.contains("after appending 1"), "{}", rec.detail);
    let head = support::branch(&w.upstream, &first.branch).unwrap();

    // Nothing new: an honest no-op.
    let BatchOutcome::Waiting(rec) = w.batcher().run().await.unwrap().outcome else {
        panic!("expected the open batch to wait")
    };
    assert_eq!(rec.detail, "waiting for review; no new lands to append");
    assert_eq!(rec.lands, first.lands + 1);
    assert_eq!(support::branch(&w.upstream, &first.branch).unwrap(), head);

    // Sealed (auto-merge armed): the land waits and the run says so.
    w.github
        .merging
        .lock()
        .unwrap()
        .push(first.pr_number.unwrap());
    w.land(&[("c.txt", "c1\n")], "ci fix", 3).await;
    let BatchOutcome::Waiting(rec) = w.batcher().run().await.unwrap().outcome else {
        panic!("expected the open batch to wait")
    };
    assert!(rec.detail.starts_with("sealed"), "{}", rec.detail);
    assert!(!rec.detail.contains("appending"), "{}", rec.detail);
    assert_eq!(rec.lands, first.lands + 1);
    assert_eq!(support::branch(&w.upstream, &first.branch).unwrap(), head);
    let stored = w.db.batch(first.number).unwrap().unwrap();
    assert_eq!(stored.detail, rec.detail);
}

/// WI-2577: an append whose lease fails is an error, never "appended".
#[tokio::test]
async fn an_append_whose_lease_fails_is_an_error() {
    let w = World::new().await;
    w.start().await;
    w.land(&[("a.txt", "a1\n")], "add a", 1).await;
    let first = opened(w.batcher().run().await.unwrap().outcome);
    w.land(&[("b.txt", "b1\n")], "review fix", 2).await;
    w.batcher().run().await.unwrap();
    let main = support::branch(&w.upstream, "main").unwrap();
    w.land(&[("c.txt", "c1\n")], "ci fix", 3).await;
    // Someone moves the branch after the run reads the PR (observe, then
    // append): the push's lease on the old head must fail.
    *w.github.race_push.lock().unwrap() = Some((1, first.branch.clone(), main.clone()));

    let err = w.batcher().run().await.unwrap_err();
    assert!(
        matches!(err, MirrorError::Append { ref branch, .. } if *branch == first.branch),
        "{err:?}"
    );
    assert_eq!(support::branch(&w.upstream, &first.branch).unwrap(), main);
}

/// A PR left on GitHub for `head` that this mirror DB never opened (a trial,
/// or a DB that was reset): merged into `main` as `merged`, its branch gone.
fn leftover_merged_pr(w: &World, head: &str, merged: &str) -> u64 {
    let mut prs = w.github.prs_mut();
    let number = prs.len() as u64 + 1;
    prs.push(FakePr {
        number,
        new: NewPull {
            title: "an earlier batch".to_owned(),
            body: String::new(),
            head: head.to_owned(),
            base: "main".to_owned(),
        },
        open: false,
        merged: Some(merged.to_owned()),
    });
    number
}

/// WI-2585: a fresh DB on a GitHub repo that already has `graft/batch-1`
/// merged (branch deleted) never adopts that PR; the lands go in a new PR.
#[tokio::test]
async fn a_leftover_merged_pr_is_never_adopted() {
    let w = World::new().await;
    w.start().await;
    let main = support::branch(&w.upstream, "main").unwrap();
    let old = leftover_merged_pr(&w, "graft/batch-1", &main);
    let a = w.land(&[("a.txt", "a1\n")], "add a", 1).await;
    let b = w.land(&[("b.txt", "b1\n")], "add b", 2).await;

    let run = w.batcher().run().await.unwrap();
    assert!(run.settled.is_none());
    let rec = opened(run.outcome);
    assert_eq!(rec.state, BatchState::Open, "{}", rec.detail);
    assert_eq!((rec.number, rec.lands), (2, 2));
    assert_eq!(rec.branch, "graft/batch-2");
    assert!(rec.merged.is_none() && rec.merge_sha.is_none());
    let new = rec.pr_number.unwrap();
    assert_ne!(new, old);

    let prs = w.github.prs();
    let pr = prs.iter().find(|p| p.number == new).unwrap();
    assert!(pr.open && pr.merged.is_none());
    assert_eq!(pr.new.head, "graft/batch-2");
    for id in [a, b] {
        assert!(
            pr.new.body.contains(&id.to_string()[..13]),
            "{}",
            pr.new.body
        );
    }
    // Nothing reached main; the old PR is as it was.
    assert_eq!(support::branch(&w.upstream, "main").unwrap(), main);
    assert!(support::branch(&w.upstream, "graft/batch-1").is_none());
    let old_pr = prs.iter().find(|p| p.number == old).unwrap();
    assert!(!old_pr.open && old_pr.new.head == "graft/batch-1");

    // The next run waits on the new PR; it does not settle anything.
    match w.batcher().run().await.unwrap().outcome {
        BatchOutcome::Waiting(r) => assert_eq!(r.pr_number, Some(new)),
        other => panic!("expected to wait, got {other:?}"),
    }
}

/// WI-2585: a leftover branch with other commits and a leftover merged PR
/// (the trial's batch-1 and batch-2) are both skipped; the lands go in a
/// fresh PR on a free number, and neither leftover is changed.
#[tokio::test]
async fn leftover_batch_branches_and_prs_are_skipped_for_a_fresh_number() {
    let w = World::new().await;
    w.start().await;
    let main = support::branch(&w.upstream, "main").unwrap();
    let tree = git(&w.upstream, &["rev-parse", &format!("{main}^{{tree}}")]);
    let stray = git(
        &w.upstream,
        &["commit-tree", &tree, "-p", &main, "-m", "trial leftover"],
    );
    git(
        &w.upstream,
        &["update-ref", "refs/heads/graft/batch-1", &stray],
    );
    let old = leftover_merged_pr(&w, "graft/batch-2", &main);
    w.land(&[("a.txt", "a1\n")], "add a", 1).await;
    w.land(&[("b.txt", "b1\n")], "add b", 2).await;

    let rec = opened(w.batcher().run().await.unwrap().outcome);
    assert_eq!(rec.state, BatchState::Open, "{}", rec.detail);
    assert_eq!((rec.number, rec.lands), (3, 2));
    assert_eq!(rec.branch, "graft/batch-3");
    assert!(rec.merged.is_none());
    assert_ne!(rec.pr_number, Some(old));
    let prs = w.github.prs();
    let pr = prs
        .iter()
        .find(|p| Some(p.number) == rec.pr_number)
        .unwrap();
    assert!(pr.open && pr.merged.is_none());
    assert_eq!(pr.new.head, "graft/batch-3");
    assert_eq!(
        w.changed(
            &main,
            &support::branch(&w.upstream, "graft/batch-3").unwrap()
        ),
        ["a.txt", "b.txt"]
    );
    assert_eq!(
        support::branch(&w.upstream, "graft/batch-1").unwrap(),
        stray
    );
    assert_eq!(support::branch(&w.upstream, "main").unwrap(), main);
}

/// WI-2585: a PR graft did not open that appears on the batch branch after
/// the number was chosen and the branch pushed is not adopted. The batch is
/// rejected unmerged, and the next run carries its lands in a fresh PR.
#[tokio::test]
async fn a_foreign_pr_on_the_pushed_branch_is_rejected_not_adopted() {
    let w = World::new().await;
    w.start().await;
    let main = support::branch(&w.upstream, "main").unwrap();
    let a = w.land(&[("a.txt", "a1\n")], "add a", 1).await;
    // The number check reads graft/batch-1 once; the PR appears for open().
    *w.github.foreign_pr.lock().unwrap() = Some(("graft/batch-1".to_owned(), 1));

    let run = w.batcher().run().await.unwrap();
    let rec = opened(run.outcome);
    assert_eq!(rec.number, 1);
    assert_eq!(rec.state, BatchState::Rejected);
    assert!(rec.detail.contains("did not open"), "{}", rec.detail);
    assert!(rec.merged.is_none() && rec.merge_sha.is_none());
    assert_eq!(rec.pr_number, None, "the foreign PR is not recorded");
    assert_eq!(w.github.prs().len(), 1, "no PR was opened over it");
    assert_eq!(support::branch(&w.upstream, "main").unwrap(), main);

    // The lands are still pending: the next run puts them in a new PR on a
    // free number, past the taken branch.
    let rec = opened(w.batcher().run().await.unwrap().outcome);
    assert_eq!(rec.state, BatchState::Open, "{}", rec.detail);
    assert_eq!((rec.number, rec.lands), (2, 1));
    let prs = w.github.prs();
    let pr = prs
        .iter()
        .find(|p| Some(p.number) == rec.pr_number)
        .unwrap();
    assert!(pr.open && pr.merged.is_none());
    assert_eq!(pr.new.head, "graft/batch-2");
    assert!(pr.new.body.contains(&a.to_string()[..13]));
}

// ---- Post-land checked lines (design §8.7 step 6, WI-2599) ----

/// A batcher for a checked line.
fn checked(c: &mut BatchConfig) {
    c.checked = true;
}

impl World {
    /// Lands `head` on the integration line through `Land`, so a checked
    /// line gives it a `pending` land-checks row.
    async fn land_head(&self, head: ChangesetId) -> ChangesetId {
        match self
            .api
            .land(
                &cx(),
                Land {
                    repo: REPO.to_owned(),
                    head,
                    line: LINE.to_owned(),
                    squash: false,
                    merge: MergeMode::File,
                    deadline_ms: None,
                    merged_from: None,
                },
            )
            .await
            .unwrap()
        {
            Landed::Landed { landed, .. } => landed,
            other => panic!("not landed: {other:?}"),
        }
    }

    /// One changeset writing `file` on the line tip, landed through `Land`.
    async fn land_checked(&self, file: &str, n: i64) -> ChangesetId {
        let tip = self.tip(LINE).await;
        let body = format!("{file} {n}\n");
        let id = self
            .api
            .create_changeset(
                &cx(),
                CreateChangeset {
                    repo: REPO.to_owned(),
                    parents: tip.into_iter().collect(),
                    changes: vec![Change::Put {
                        path: RepoPath::new(file).unwrap(),
                        kind: EntryKind::File,
                        id: graft_model::ObjectId::hash(ObjectKind::Blob, body.as_bytes()),
                    }],
                    author: Author {
                        name: "Tess".to_owned(),
                        email: "tess@example.com".to_owned(),
                        kind: AuthorKind::Agent,
                    },
                    authored_at: Timestamp {
                        ms: 1_700_000_000_000 + n * 1000,
                        offset_min: 0,
                    },
                    message: format!("add {file}"),
                    metadata: Metadata::new(),
                    inline_blobs: vec![Frame::encode(ObjectKind::Blob, body.as_bytes())],
                    workspace: None,
                },
            )
            .await
            .unwrap()
            .id;
        self.land_head(id).await
    }

    /// Lands graft's `Revert` of `land` (metadata `graft.reverts`).
    async fn land_revert(&self, land: ChangesetId) -> ChangesetId {
        let tip = self.tip(LINE).await.unwrap();
        let revert = self
            .api
            .revert(
                &cx(),
                Revert {
                    repo: REPO.to_owned(),
                    changeset: land,
                    onto: tip,
                    workspace: None,
                    check_url: None,
                },
            )
            .await
            .unwrap();
        self.land_head(revert).await
    }

    /// The post-land checker reports `ci/test` on `land`.
    async fn report(&self, land: ChangesetId, pass: bool) {
        self.api
            .attest_v2(
                &attest_cx(),
                AttestV2 {
                    repo: REPO.to_owned(),
                    changeset: land,
                    kind: AttestKind::Check,
                    check: "ci/test".to_owned(),
                    status: if pass {
                        AttestStatus::Pass
                    } else {
                        AttestStatus::Fail
                    },
                    by: Some("checker".to_owned()),
                    url: "https://ci.example/run".to_owned(),
                },
            )
            .await
            .unwrap();
    }

    /// The state of `land`'s row on the integration line.
    async fn check_state(&self, land: ChangesetId) -> LandCheckState {
        let rows = graft_client::land_checks(
            &self.api,
            &cx(),
            GetLandChecks {
                repo: REPO.to_owned(),
                line: LINE.to_owned(),
                from_pos: 0,
                page: Page {
                    limit: 0,
                    token: None,
                },
            },
            1000,
        )
        .await
        .unwrap();
        rows.iter().find(|r| r.changeset == land).unwrap().state
    }

    /// The files the batch branch changes on GitHub's `main`.
    fn batch_files(&self, n: u64) -> Vec<String> {
        let head = support::branch(&self.upstream, &format!("graft/batch-{n}")).unwrap();
        let main = support::branch(&self.upstream, "main").unwrap();
        self.changed(&main, &head)
    }
}

// Accept 2: good, good, pending, good is cut after the second land, and
// the rest waits until the pending land settles.
#[tokio::test]
async fn a_checked_line_cuts_before_a_pending_land() {
    let w = World::checked().await;
    w.start().await;
    let a = w.land_checked("a.txt", 1).await;
    let b = w.land_checked("b.txt", 2).await;
    let c = w.land_checked("c.txt", 3).await;
    let d = w.land_checked("d.txt", 4).await;
    for land in [a, b, d] {
        w.report(land, true).await;
    }
    let rec = opened(w.batcher_with(&w.db, checked).run().await.unwrap().outcome);
    assert_eq!((rec.lands, rec.tip), (2, b));
    assert_eq!(w.batch_files(1), ["a.txt", "b.txt"]);
    assert!(
        rec.body
            .contains("Cut at 2 lands with settled post-land checks: 2 newer lands wait"),
        "{}",
        rec.body
    );
    assert!(!rec.body.contains(&short(c)) && !rec.body.contains(&short(d)));

    // Still pending: nothing is appended to the open batch.
    let run = w.batcher_with(&w.db, checked).run().await.unwrap();
    let BatchOutcome::Waiting(open) = run.outcome else {
        panic!("expected the open batch, got {:?}", run.outcome);
    };
    assert_eq!((open.lands, open.tip), (2, b));
    assert!(open.detail.contains("not settled"), "{}", open.detail);

    // Settled: the open batch takes c and d.
    w.report(c, true).await;
    let run = w.batcher_with(&w.db, checked).run().await.unwrap();
    let BatchOutcome::Waiting(open) = run.outcome else {
        panic!("expected the open batch, got {:?}", run.outcome);
    };
    assert_eq!((open.lands, open.tip), (4, d));
    assert_eq!(w.batch_files(1), ["a.txt", "b.txt", "c.txt", "d.txt"]);
}

// Accept 3: good, bad, good revert of the bad land, good is one batch of
// all four, the bad land with its revert.
#[tokio::test]
async fn a_checked_line_takes_a_bad_land_with_its_revert() {
    let w = World::checked().await;
    w.start().await;
    let a = w.land_checked("a.txt", 1).await;
    let b = w.land_checked("b.txt", 2).await;
    w.report(a, true).await;
    w.report(b, false).await;
    let r = w.land_revert(b).await;
    assert_eq!(w.check_state(b).await, LandCheckState::Reverted);
    w.report(r, true).await;
    let d = w.land_checked("d.txt", 4).await;
    w.report(d, true).await;

    let rec = opened(w.batcher_with(&w.db, checked).run().await.unwrap().outcome);
    assert_eq!((rec.lands, rec.tip), (4, d));
    assert_eq!(w.batch_files(1), ["a.txt", "d.txt"]);
    for land in [a, b, r, d] {
        assert!(rec.body.contains(&short(land)), "{}", rec.body);
    }
    // One commit per land, the bad land and its revert included.
    let head = support::branch(&w.upstream, "graft/batch-1").unwrap();
    let main = support::branch(&w.upstream, "main").unwrap();
    assert_eq!(chain_of(&w, &main, &head).len(), 4);
}

// Accept 4: a bad land with no revert yet stops the cut before it; a line
// whose oldest land is bad makes no batch.
#[tokio::test]
async fn a_checked_line_cuts_before_a_bad_land_with_no_revert() {
    let w = World::checked().await;
    w.start().await;
    let a = w.land_checked("a.txt", 1).await;
    let b = w.land_checked("b.txt", 2).await;
    let c = w.land_checked("c.txt", 3).await;
    w.report(a, true).await;
    w.report(b, false).await;
    w.report(c, true).await;
    let rec = opened(w.batcher_with(&w.db, checked).run().await.unwrap().outcome);
    assert_eq!((rec.lands, rec.tip), (1, a));
    assert_eq!(w.batch_files(1), ["a.txt"]);
    assert!(!rec.body.contains(&short(b)));

    // After a merge the next batch starts at the bad land: no batch.
    w.github.squash_merge(1);
    let run = w.batcher_with(&w.db, checked).run().await.unwrap();
    assert_eq!(run.settled.unwrap().state, BatchState::Merged);
    match run.outcome {
        BatchOutcome::Unsettled { lands, waiting } => assert_eq!((lands, waiting), (2, b)),
        other => panic!("expected no batch, got {other:?}"),
    }
    assert_eq!(w.github.prs().len(), 1);
}

// Accept 1 (the checked path is off by default): the same lands, unchecked,
// batch to the tip whatever their rows say.
#[tokio::test]
async fn an_unchecked_batcher_ignores_land_checks() {
    let w = World::checked().await;
    w.start().await;
    let a = w.land_checked("a.txt", 1).await;
    w.report(a, false).await;
    let b = w.land_checked("b.txt", 2).await;
    let rec = opened(w.batcher().run().await.unwrap().outcome);
    assert_eq!((rec.lands, rec.tip), (2, b));
    assert!(!rec.body.contains("post-land"));
}

/// `GetLandChecks` reads through the batcher: each call's page limit and
/// the rows it returned.
struct Counting<'a> {
    api: &'a Api,
    reads: Mutex<Vec<(u32, usize)>>,
}

macro_rules! forward {
    ($($name:ident($req:ty) -> $out:ty;)*) => {
        $(
            async fn $name(&self, cx: &CallContext, req: $req) -> graft_proto::error::ApiResult<$out> {
                self.api.$name(cx, req).await
            }
        )*
    };
}

impl ObjectApi for Counting<'_> {
    forward! {
        get_objects(GetObjects) -> Objects;
        has_objects(HasObjects) -> Vec<graft_model::ObjectId>;
        put_objects(PutObjects) -> Vec<graft_model::ObjectId>;
    }
}

impl AdminApi for Counting<'_> {
    forward! {
        import_changesets(ImportChangesets) -> Vec<Created>;
        gc(Gc) -> GcReport;
        mint_token(MintToken) -> String;
    }
    async fn create_repo(
        &self,
        cx: &CallContext,
        name: String,
    ) -> graft_proto::error::ApiResult<()> {
        self.api.create_repo(cx, name).await
    }
}

impl RepoApi for Counting<'_> {
    forward! {
        create_changeset(CreateChangeset) -> Created;
        get_changesets(GetChangesets) -> Changesets;
        resolve_ref(ResolveRef) -> RefView;
        list_refs(ListRefs) -> Vec<RefView>;
        update_ref(UpdateRef) -> RefView;
        stat(PathAt) -> EntryView;
        list_tree(ListTree) -> Entries;
        read_file(PathAt) -> FileContent;
        log(Log) -> History;
        is_ancestor(Pair) -> bool;
        merge_base(Pair) -> Option<ChangesetId>;
        diff(Diff) -> DiffResult;
        blame(Blame) -> BlameResult;
        rebase(Rebase) -> ChangesetId;
        revert(Revert) -> ChangesetId;
        land(Land) -> Landed;
        attest(Attest) -> ();
        watch(Watch) -> JournalPage;
    }
    async fn get_land_checks(
        &self,
        cx: &CallContext,
        req: GetLandChecks,
    ) -> graft_proto::error::ApiResult<LandCheckPage> {
        let limit = req.page.limit;
        let page = self.api.get_land_checks(cx, req).await?;
        self.reads.lock().unwrap().push((limit, page.rows.len()));
        Ok(page)
    }
}

// Accept 5: a batch after many checked lands reads its own lands' rows,
// paged, plus O(log line length) one-row probes, never the line's rows.
#[tokio::test]
async fn a_checked_cut_reads_only_the_batch_rows() {
    const EARLIER: i64 = 40;
    let w = World::checked().await;
    w.start().await;
    for n in 0..EARLIER {
        let land = w.land_checked(&format!("f{n}.txt"), n).await;
        w.report(land, true).await;
    }
    let rec = opened(w.batcher_with(&w.db, checked).run().await.unwrap().outcome);
    assert_eq!(rec.lands, EARLIER as u64);
    w.github.squash_merge(1);
    assert_eq!(
        w.batcher_with(&w.db, checked)
            .run()
            .await
            .unwrap()
            .settled
            .unwrap()
            .state,
        BatchState::Merged
    );
    let x = w.land_checked("x.txt", 100).await;
    let y = w.land_checked("y.txt", 101).await;
    w.report(x, true).await;
    w.report(y, true).await;

    let counting = Counting {
        api: &w.api,
        reads: Mutex::new(Vec::new()),
    };
    let run = w
        .batcher_over(&counting, &w.db, checked)
        .run()
        .await
        .unwrap();
    let rec = opened(run.outcome);
    assert_eq!((rec.lands, rec.tip), (2, y));
    let reads = counting.reads.into_inner().unwrap();
    let (probes, pages): (Vec<_>, Vec<_>) = reads.iter().partition(|(limit, _)| *limit == 1);
    // The batch read asks for its two lands and gets exactly them.
    assert_eq!(pages.iter().map(|(_, rows)| rows).sum::<usize>(), 2);
    assert!(pages.iter().all(|(limit, _)| *limit <= 2), "{pages:?}");
    // Probes: an exponential search and a bisection over ~40 positions.
    assert!(probes.len() <= 16, "{} probes", probes.len());
    let rows: usize = reads.iter().map(|(_, rows)| rows).sum();
    assert!(rows < EARLIER as usize / 2, "{rows} rows read");
}

/// The server's export record, in memory: it refuses a `RecordExports` call
/// over the server's cap, as the server does, and keeps each call's size.
#[derive(Default)]
struct Exports {
    got: Mutex<HashSet<(ChangesetId, GitSha)>>,
    calls: Mutex<Vec<usize>>,
}

impl Exports {
    /// Whether `commit` is recorded as the export of `changeset`.
    fn is_export_of(&self, changeset: ChangesetId, commit: GitSha) -> bool {
        self.got.lock().unwrap().contains(&(changeset, commit))
    }

    fn calls(&self) -> Vec<usize> {
        self.calls.lock().unwrap().clone()
    }
}

impl ExportLog for Exports {
    fn record_exports<'s>(
        &'s self,
        _cx: &'s CallContext,
        repo: &'s str,
        exports: &'s [(ChangesetId, GitSha)],
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<(), ApiError>> + Send + 's>>
    {
        Box::pin(async move {
            assert_eq!(repo, REPO);
            if exports.len() > MAX_RECORD_EXPORTS {
                return Err(ApiError::LimitExceeded {
                    limit: "record_exports".to_owned(),
                    max: MAX_RECORD_EXPORTS as u64,
                });
            }
            self.calls.lock().unwrap().push(exports.len());
            self.got.lock().unwrap().extend(exports.iter().copied());
            Ok(())
        })
    }
}

/// Asserts each of batch `number`'s per-land commits is recorded as its
/// land's export, and returns them.
fn assert_recorded(w: &World, log: &Exports, number: u64) -> Vec<(ChangesetId, GitSha)> {
    let commits: Vec<_> =
        w.db.batch_commits(number)
            .unwrap()
            .into_iter()
            .map(|c| (c.land, c.sha))
            .collect();
    assert!(!commits.is_empty());
    for (land, sha) in &commits {
        assert!(
            log.is_export_of(*land, *sha),
            "{land} -> {sha} not recorded"
        );
    }
    commits
}

#[tokio::test]
async fn each_batch_commit_is_recorded_as_its_lands_export() {
    let w = World::new().await;
    w.start().await;
    let a = w.land(&[("a.txt", "a1\n")], "add a", 1).await;
    let b = w.land(&[("b.txt", "b1\n")], "add b", 2).await;
    let log = Exports::default();
    let rec = opened(
        w.batcher()
            .with_export_log(&log)
            .run()
            .await
            .unwrap()
            .outcome,
    );
    let commits = assert_recorded(&w, &log, rec.number);
    assert_eq!(
        commits.iter().map(|(land, _)| *land).collect::<Vec<_>>(),
        [a, b]
    );
    // One commit per land: a land's export is its own commit only.
    assert!(!log.is_export_of(a, commits[1].1));
    assert_eq!(commits[1].1, rec.squash_sha);

    // An append to the open batch records the appended land's commit.
    let c = w.land(&[("c.txt", "c1\n")], "review fix", 3).await;
    let run = w.batcher().with_export_log(&log).run().await.unwrap();
    let BatchOutcome::Waiting(rec) = run.outcome else {
        panic!("expected the open batch to wait")
    };
    let commits = assert_recorded(&w, &log, rec.number);
    assert_eq!(commits.len(), 3);
    assert_eq!(commits[2], (c, rec.squash_sha));
    assert_eq!(
        support::branch(&w.upstream, &rec.branch).unwrap(),
        rec.squash_sha.to_hex()
    );

    // A re-run with nothing new records nothing new and changes no commit.
    let before = log.got.lock().unwrap().len();
    let run = w.batcher().with_export_log(&log).run().await.unwrap();
    assert!(matches!(run.outcome, BatchOutcome::Waiting(_)));
    assert_eq!(log.got.lock().unwrap().len(), before);
    assert_eq!(assert_recorded(&w, &log, rec.number), commits);
}

#[tokio::test]
async fn batch_exports_are_recorded_before_the_push() {
    let w = World::new().await;
    w.start().await;
    w.land(&[("a.txt", "a1\n")], "add a", 1).await;
    w.land(&[("b.txt", "b1\n")], "add b", 2).await;
    // GitHub refuses every push to a batch branch.
    let hook = w.upstream.join("hooks/pre-receive");
    std::fs::write(
        &hook,
        "#!/bin/sh\nwhile read old new ref; do\n  case \"$ref\" in refs/heads/graft/batch-*) exit 1;; esac\ndone\nexit 0\n",
    )
    .unwrap();
    let mut perms = std::fs::metadata(&hook).unwrap().permissions();
    std::os::unix::fs::PermissionsExt::set_mode(&mut perms, 0o755);
    std::fs::set_permissions(&hook, perms).unwrap();

    let log = Exports::default();
    let _ = w.batcher().with_export_log(&log).run().await;
    let rec = w.db.last_batch(LINE).unwrap().expect("the batch was cut");
    assert_eq!(
        support::branch(&w.upstream, &rec.branch),
        None,
        "the push failed"
    );
    assert_ne!(rec.state, BatchState::Open);
    assert_eq!(assert_recorded(&w, &log, rec.number).len(), 2);

    // A log that cannot record stops the push.
    std::fs::remove_file(&hook).unwrap();
    struct Refuse;
    impl ExportLog for Refuse {
        fn record_exports<'s>(
            &'s self,
            _cx: &'s CallContext,
            _repo: &'s str,
            _exports: &'s [(ChangesetId, GitSha)],
        ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<(), ApiError>> + Send + 's>>
        {
            Box::pin(async { Err(ApiError::Unavailable { retry_after_ms: 1 }) })
        }
    }
    let w = World::new().await;
    w.start().await;
    w.land(&[("a.txt", "a1\n")], "add a", 1).await;
    assert!(w.batcher().with_export_log(&Refuse).run().await.is_err());
    let rec = w.db.last_batch(LINE).unwrap().expect("the batch was cut");
    assert_eq!(rec.state, BatchState::Planned);
    assert_eq!(support::branch(&w.upstream, &rec.branch), None);
    // The next run records, then pushes the same commit.
    let log = Exports::default();
    let rec = match w
        .batcher()
        .with_export_log(&log)
        .run()
        .await
        .unwrap()
        .outcome
    {
        BatchOutcome::Waiting(rec) | BatchOutcome::Opened(rec) => rec,
        other => panic!("expected the batch to open, got {other:?}"),
    };
    assert_recorded(&w, &log, rec.number);
    assert_eq!(
        support::branch(&w.upstream, &rec.branch).unwrap(),
        rec.squash_sha.to_hex()
    );
}

#[tokio::test]
async fn a_batch_over_the_record_cap_is_recorded_in_chunks() {
    let w = World::new().await;
    w.start().await;
    let lands = MAX_RECORD_EXPORTS + 3;
    for n in 0..lands {
        let path = format!("f{n}.txt");
        w.land(&[(path.as_str(), "x\n")], &format!("add {n}"), n as i64)
            .await;
    }
    let log = Exports::default();
    let rec = opened(
        w.batcher()
            .with_export_log(&log)
            .run()
            .await
            .unwrap()
            .outcome,
    );
    assert_eq!(rec.lands as usize, lands);
    assert_eq!(assert_recorded(&w, &log, rec.number).len(), lands);
    assert_eq!(log.calls(), [MAX_RECORD_EXPORTS, 3]);
}
