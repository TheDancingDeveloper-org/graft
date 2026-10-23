//! Graft-primary export against a local bare repository standing in for
//! GitHub, and an in-process engine over the memory store.
//!
//! Covers ordered, deterministic export of 1,000 random lands; bootstrap
//! from an in-sync, a behind, an absent and a foreign upstream; an
//! out-of-band push (stop, alert, never force); a crash between push and
//! record; `reconcile` by landing and by discarding; and the full export.

mod support;

use std::path::PathBuf;
use std::sync::Arc;

use graft_engine::{Engine, EngineConfig};
use graft_git::GitSha;
use graft_git::map::MirrorMap;
use graft_mirror::credentials::NoCredentials;
use graft_mirror::db::ExportState;
use graft_mirror::export::{Export, ExportConfig, ExportOutcome, GRAFT_PRIMARY, export_all};
use graft_mirror::follow::FollowLine;
use graft_mirror::reconcile::{Reconciled, Resolution, reconcile, stray_ref};
use graft_mirror::{Mirror, MirrorDb, MirrorError, current_ref};
use graft_model::ChangesetId;
use graft_proto::api::AdminApi;
use graft_store::memory::MemoryStore;
use graft_telemetry::Metrics;
use graft_testkit::clock::FakeClock;
use support::*;

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

/// A bare "GitHub", the mirror's directory and DB, a work tree for
/// out-of-band pushes, and an engine with a `main` line.
struct World {
    tmp: tempfile::TempDir,
    github: PathBuf,
    mirror_dir: PathBuf,
    db: MirrorDb,
    api: Api,
    lands: u64,
}

impl World {
    async fn new(deny_rewrites: bool) -> World {
        let tmp = tempfile::tempdir().unwrap();
        let github = tmp.path().join("github.git");
        support::github(&github, deny_rewrites);
        let api = engine();
        api.create_repo(&cx(), REPO.to_owned()).await.unwrap();
        let db = MirrorDb::open(&tmp.path().join("mirror.sqlite")).unwrap();
        let mut w = World {
            mirror_dir: tmp.path().join("mirror.git"),
            tmp,
            github,
            db,
            api,
            lands: 0,
        };
        w.land_files(&[("README", "hello\n")]).await;
        w
    }

    fn mirror(&self) -> Mirror {
        Mirror::new(&self.mirror_dir, self.github.to_str().unwrap())
    }

    fn export(&self) -> Export<'_, Api, NoCredentials> {
        Export::new(
            &self.api,
            cx(),
            &self.db,
            self.mirror(),
            &NoCredentials,
            ExportConfig::new(REPO, vec![main_line()]),
        )
        .unwrap()
        .with_clock(clock)
    }

    async fn poll(&self) -> ExportOutcome {
        advance(1000);
        let mut out = self.export().poll_once().await.unwrap();
        out.pop().unwrap().1
    }

    async fn land_files(&mut self, files: &[(&str, &str)]) -> ChangesetId {
        self.lands += 1;
        let files: Vec<(String, Vec<u8>)> = files
            .iter()
            .map(|(p, b)| ((*p).to_owned(), b.as_bytes().to_vec()))
            .collect();
        *land(&self.api, &[files], self.lands).await.last().unwrap()
    }

    async fn land_one(&mut self) -> ChangesetId {
        let n = self.lands + 1;
        self.land_files(&[(&format!("f{n}.txt"), &format!("file {n}\n"))])
            .await
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

    fn sha_of(&self, id: ChangesetId) -> String {
        self.db.sha_of(id).unwrap().unwrap().to_hex()
    }

    fn state(&self) -> ExportState {
        self.db.export_state("main").unwrap().unwrap()
    }

    /// Pushes a commit of its own on top of GitHub's main, out of band.
    fn push_out_of_band(&self, file: &str) -> String {
        let work = self.tmp.path().join(format!("oob-{file}"));
        git(
            self.tmp.path(),
            &[
                "clone",
                "-q",
                self.github.to_str().unwrap(),
                work.to_str().unwrap(),
            ],
        );
        std::fs::write(work.join(file), "out of band\n").unwrap();
        git(&work, &["add", "-A"]);
        git(&work, &["commit", "-q", "-m", "an out-of-band commit"]);
        git(&work, &["push", "-q", "origin", "HEAD:main"]);
        git(&work, &["rev-parse", "HEAD"])
    }

    async fn reconcile(&self, how: Resolution) -> Result<Reconciled, MirrorError> {
        reconcile(
            &self.api,
            &cx(),
            &self.db,
            &self.mirror(),
            &FakeLand(&self.api),
            &NoCredentials,
            REPO,
            &main_line(),
            how,
        )
        .await
    }

    /// GitHub's first-parent history is graft's line: one commit per
    /// changeset, in order, each the recorded export of its changeset.
    async fn assert_mirrored(&self) {
        let chain = line_chain(&self.api).await;
        let shas = git(
            &self.github,
            &["rev-list", "--first-parent", "--reverse", "main"],
        );
        let mapped: Vec<ChangesetId> = shas
            .lines()
            .map(|s| {
                let sha = GitSha::from_hex(s).unwrap();
                self.db
                    .changeset_of(sha)
                    .unwrap()
                    .expect("every commit is mapped")
            })
            .collect();
        assert_eq!(mapped, chain, "one commit per changeset, in order");
        assert_eq!(self.github_main(), Some(self.sha_of(self.tip().await)));
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn each_land_is_exported_on_a_lease() {
    let mut w = World::new(true).await;
    assert!(matches!(
        w.poll().await,
        ExportOutcome::Pushed { from: None, .. }
    ));
    w.assert_mirrored().await;
    let a = w.land_one().await;
    let before = w.github_main().unwrap();
    match w.poll().await {
        ExportOutcome::Pushed {
            from,
            to,
            changeset,
            moves,
        } => {
            assert_eq!(from.map(|s| s.to_hex()), Some(before));
            assert_eq!(to.to_hex(), w.sha_of(a));
            assert_eq!(changeset, a);
            assert_eq!(moves, 1);
        }
        other => panic!("{other:?}"),
    }
    assert_eq!(w.poll().await, ExportOutcome::UpToDate);
    // Two lands between polls: both changesets are exported, in order, in
    // one push.
    w.land_one().await;
    w.land_one().await;
    assert!(matches!(
        w.poll().await,
        ExportOutcome::Pushed { moves: 2, .. }
    ));
    w.assert_mirrored().await;
    let text = git(&w.github, &["log", "-1", "--format=%cn <%ce>%n%B", "main"]);
    assert!(text.starts_with("graft <graft@graft.invalid>"), "{text}");
    assert!(text.contains(&format!("Graft-Changeset: {}", w.tip().await)));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_thousand_random_lands_export_in_order_with_deterministic_shas() {
    async fn run(seed: u64) -> (Vec<String>, Vec<String>) {
        set_clock(1_700_000_000_000);
        let w = World::new(true).await;
        let mut rng = Rng(seed);
        let mut pushed = Vec::new();
        let mut contents = std::collections::BTreeMap::new();
        for n in 0..1000u64 {
            let depth = 1 + rng.below(3);
            let mut stack = Vec::new();
            for _ in 0..depth {
                let mut files = std::collections::BTreeMap::new();
                for _ in 0..1 + rng.below(3) {
                    let path = format!("d{}/f{:02}.txt", rng.below(4), rng.below(12));
                    let body = format!("{n} {}\n", rng.next()).into_bytes();
                    contents.insert(path.clone(), body.clone());
                    files.insert(path, body);
                }
                stack.push(files.into_iter().collect());
            }
            land(&w.api, &stack, n + 2).await;
            if rng.below(4) == 0
                && let ExportOutcome::Pushed { to, .. } = w.poll().await
            {
                pushed.push(to.to_hex());
            }
        }
        if let ExportOutcome::Pushed { to, .. } = w.poll().await {
            pushed.push(to.to_hex());
        }
        assert_eq!(w.poll().await, ExportOutcome::UpToDate);
        w.assert_mirrored().await;
        // Every push was a fast-forward of the one before it.
        for pair in pushed.windows(2) {
            git(
                &w.github,
                &["merge-base", "--is-ancestor", &pair[0], &pair[1]],
            );
        }
        // The tree at the tip holds what graft's line holds.
        for (path, body) in contents.iter().take(10) {
            let got = git(&w.github, &["show", &format!("main:{path}")]);
            assert_eq!(got, String::from_utf8_lossy(body).trim());
        }
        let shas = git(
            &w.github,
            &["rev-list", "--first-parent", "--reverse", "main"],
        );
        (pushed, shas.lines().map(str::to_owned).collect())
    }
    let (pushed_a, shas_a) = run(0x9e37_79b9_7f4a_7c15).await;
    let (pushed_b, shas_b) = run(0x9e37_79b9_7f4a_7c15).await;
    assert!(shas_a.len() > 1000, "every changeset is a commit");
    assert_eq!(shas_a, shas_b, "the same lands export to the same shas");
    assert_eq!(pushed_a, pushed_b);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_out_of_band_push_stops_export_alerts_and_never_forces() {
    let mut w = World::new(true).await;
    w.poll().await;
    let lease = w.github_main().unwrap();
    let stray = w.push_out_of_band("stray.txt");
    w.land_one().await;
    let metrics = Metrics::register(&prometheus::Registry::new()).unwrap();
    let mut export = w.export().with_metrics(metrics.clone());
    advance(1000);
    let out = export.poll_once().await.unwrap().pop().unwrap().1;
    assert_eq!(
        out,
        ExportOutcome::Diverged {
            expected: GitSha::from_hex(&lease),
            upstream: GitSha::from_hex(&stray),
        }
    );
    assert_eq!(
        metrics.mirror_divergence.with_label_values(&["main"]).get(),
        1
    );
    assert_eq!(
        w.github_main().as_deref(),
        Some(stray.as_str()),
        "not forced"
    );
    // Stopped: later lands are not pushed, and the stray commit stays.
    w.land_one().await;
    assert_eq!(
        w.poll().await,
        ExportOutcome::Held {
            upstream: GitSha::from_hex(&stray)
        }
    );
    assert_eq!(w.github_main().as_deref(), Some(stray.as_str()));
    assert!(w.state().diverged);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_crash_between_push_and_record_resumes_without_a_stop() {
    let mut w = World::new(true).await;
    w.poll().await;
    let recorded = w.state();
    w.land_one().await;
    assert!(matches!(w.poll().await, ExportOutcome::Pushed { .. }));
    // The push reached GitHub but the record did not reach the DB. The
    // page is read again and the same sha pushed: refused, read back, and
    // taken as done.
    let pushed = w.github_main();
    w.db.set_export_state("main", &recorded).unwrap();
    assert!(matches!(w.poll().await, ExportOutcome::Pushed { .. }));
    assert_eq!(w.github_main(), pushed, "the same commit, not a second one");
    assert!(!w.state().diverged);
    w.assert_mirrored().await;
    // The same, with another land queued behind the lost record.
    let recorded = w.state();
    w.land_one().await;
    w.poll().await;
    w.db.set_export_state("main", &recorded).unwrap();
    w.land_one().await;
    assert!(matches!(w.poll().await, ExportOutcome::Pushed { .. }));
    assert!(!w.state().diverged);
    w.assert_mirrored().await;
}

/// A mirror DB with the world's map but no export record, in place of the
/// world's: a mirror that has the commits (as follow left them before the
/// authority switch) and starts exporting now.
async fn fresh_db(w: &mut World, name: &str) {
    let fresh = MirrorDb::open(&w.tmp.path().join(name)).unwrap();
    for id in line_chain(&w.api).await {
        if let Some(commit) = w.db.imported(id).unwrap() {
            fresh.put_imported(id, commit).unwrap();
        }
    }
    w.db = fresh;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn bootstrap_starts_from_what_upstream_holds() {
    // In sync with the tip: nothing to push, and the next land is pushed on
    // that lease.
    let mut w = World::new(true).await;
    w.land_one().await;
    w.poll().await;
    let tip = w.tip().await;
    fresh_db(&mut w, "in-sync.sqlite").await;
    assert_eq!(w.poll().await, ExportOutcome::UpToDate);
    assert_eq!(w.state().changeset, Some(tip));
    assert!(w.state().lsn > 1, "the cursor starts at the journal's end");
    w.land_one().await;
    assert!(matches!(
        w.poll().await,
        ExportOutcome::Pushed { from: Some(f), moves: 1, .. } if f.to_hex() == w.sha_of(tip)
    ));
    w.assert_mirrored().await;

    // Behind the tip, on an older commit of the line: caught up at once.
    let mut w = World::new(true).await;
    w.poll().await;
    let behind = w.github_main().unwrap();
    for _ in 0..40 {
        w.land_one().await;
    }
    fresh_db(&mut w, "behind.sqlite").await;
    assert!(
        w.db.sha_of(w.tip().await).unwrap().is_none(),
        "never exported"
    );
    assert!(matches!(
        w.poll().await,
        ExportOutcome::Pushed { from: Some(f), .. } if f.to_hex() == behind
    ));
    w.assert_mirrored().await;
    assert_eq!(w.state().changeset, Some(w.tip().await));
    assert_eq!(w.poll().await, ExportOutcome::UpToDate);

    // On a commit graft does not know: diverged from the start.
    let w = World::new(true).await;
    w.push_unrelated();
    assert!(matches!(
        w.poll().await,
        ExportOutcome::Diverged {
            expected: None,
            upstream: Some(_)
        }
    ));
    assert!(w.state().diverged);
}

impl World {
    /// Gives GitHub a main of its own, unrelated to graft.
    fn push_unrelated(&self) {
        let work = self.tmp.path().join("unrelated");
        std::fs::create_dir_all(&work).unwrap();
        git(&work, &["init", "-q", "-b", "main"]);
        std::fs::write(work.join("x"), "x\n").unwrap();
        git(&work, &["add", "-A"]);
        git(&work, &["commit", "-q", "-m", "unrelated"]);
        git(
            &work,
            &["push", "-q", self.github.to_str().unwrap(), "main:main"],
        );
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn reconcile_lands_the_stray_commits_and_export_resumes() {
    let mut w = World::new(true).await;
    w.poll().await;
    let stray = w.push_out_of_band("stray.txt");
    w.land_one().await;
    assert!(matches!(w.poll().await, ExportOutcome::Diverged { .. }));
    // The operator lands the stray commit through graft. The line moved
    // since, so `Land` rebases it, and the next push replaces the stray
    // commit with its rebased copy, on the lease the operator approved.
    // GitHub lets the mirror's App rewrite; the test repository must too.
    git(
        &w.github,
        &["config", "receive.denyNonFastForwards", "false"],
    );
    let out = w.reconcile(Resolution::Land).await.unwrap();
    assert_eq!(out.stray.map(|s| s.to_hex()), Some(stray.clone()));
    let kept = stray_ref("main", GitSha::from_hex(&stray).unwrap());
    assert_eq!(out.kept_as.as_deref(), Some(kept.as_str()));
    assert_eq!(w.tip().await, out.landed.unwrap());
    assert!(!w.state().diverged);
    assert!(matches!(
        w.poll().await,
        ExportOutcome::Pushed { from: Some(f), .. } if f.to_hex() == stray
    ));
    w.assert_mirrored().await;
    let files = git(&w.github, &["ls-tree", "--name-only", "main"]);
    assert!(
        files.lines().any(|f| f == "stray.txt"),
        "the stray change landed"
    );
    assert_eq!(git(&w.mirror_dir, &["rev-parse", &kept]), stray, "kept");
    w.land_one().await;
    assert!(matches!(w.poll().await, ExportOutcome::Pushed { .. }));
    w.assert_mirrored().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn reconcile_lands_a_stray_fast_forward_as_itself() {
    let mut w = World::new(true).await;
    w.poll().await;
    let stray = w.push_out_of_band("stray.txt");
    // Stopped before graft's line moved again (as an operator would find a
    // line stopped by an earlier refused push it has since reverted).
    let mut state = w.state();
    state.diverged = true;
    state.stray = GitSha::from_hex(&stray);
    w.db.set_export_state("main", &state).unwrap();
    let out = w.reconcile(Resolution::Land).await.unwrap();
    let landed = out.landed.unwrap();
    // No rebase was needed: the stray commit landed as itself, and its
    // export is exactly the commit GitHub has. Nothing is rewritten.
    assert_eq!(w.sha_of(landed), stray);
    assert_eq!(w.poll().await, ExportOutcome::UpToDate);
    assert_eq!(w.github_main().as_deref(), Some(stray.as_str()));
    w.land_one().await;
    assert!(matches!(
        w.poll().await,
        ExportOutcome::Pushed { from: Some(f), .. } if f.to_hex() == stray
    ));
    w.assert_mirrored().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn reconcile_discard_restores_graft_history_on_a_lease() {
    let mut w = World::new(false).await;
    w.poll().await;
    let exported = w.github_main().unwrap();
    let stray = w.push_out_of_band("stray.txt");
    w.land_one().await;
    assert!(matches!(w.poll().await, ExportOutcome::Diverged { .. }));
    let out = w.reconcile(Resolution::Discard).await.unwrap();
    assert!(!out.already_resolved);
    assert_eq!(w.github_main(), Some(exported), "graft's history is back");
    // Kept, not deleted.
    let kept = stray_ref("main", GitSha::from_hex(&stray).unwrap());
    assert_eq!(git(&w.mirror_dir, &["rev-parse", &kept]), stray);
    // Export resumes from its cursor: the land made while stopped is pushed.
    assert!(matches!(w.poll().await, ExportOutcome::Pushed { .. }));
    w.assert_mirrored().await;
    // A second reconcile has nothing to do.
    assert!(w.reconcile(Resolution::Discard).await.is_err());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn reconcile_refuses_a_rewritten_upstream_for_land_and_a_line_not_stopped() {
    let mut w = World::new(false).await;
    w.poll().await;
    assert!(w.reconcile(Resolution::Land).await.is_err(), "not diverged");
    w.push_unrelated_force();
    w.land_one().await;
    assert!(matches!(w.poll().await, ExportOutcome::Diverged { .. }));
    assert!(w.reconcile(Resolution::Land).await.is_err());
    assert!(w.state().diverged, "refusal changes nothing");
    w.reconcile(Resolution::Discard).await.unwrap();
    w.poll().await;
    w.assert_mirrored().await;
}

impl World {
    fn push_unrelated_force(&self) {
        let work = self.tmp.path().join("rewrite");
        std::fs::create_dir_all(&work).unwrap();
        git(&work, &["init", "-q", "-b", "main"]);
        std::fs::write(work.join("x"), "x\n").unwrap();
        git(&work, &["add", "-A"]);
        git(&work, &["commit", "-q", "-m", "rewritten"]);
        git(
            &work,
            &[
                "push",
                "-q",
                "--force",
                self.github.to_str().unwrap(),
                "main:main",
            ],
        );
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn export_claims_the_line_and_refuses_one_another_authority_holds() {
    let w = World::new(true).await;
    w.db.claim_authority("main", graft_mirror::bridge::GIT_PRIMARY)
        .unwrap();
    let refused = Export::new(
        &w.api,
        cx(),
        &w.db,
        w.mirror(),
        &NoCredentials,
        ExportConfig::new(REPO, vec![main_line()]),
    );
    assert!(refused.is_err());
    let w = World::new(true).await;
    w.export();
    assert_eq!(
        w.db.claim_authority("main", "anything").unwrap(),
        GRAFT_PRIMARY
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_full_export_writes_what_export_pushed() {
    let mut w = World::new(true).await;
    for _ in 0..5 {
        w.land_one().await;
        w.poll().await;
    }
    let dest = w.tmp.path().join("exit.git");
    let report = export_all(&w.api, &cx(), &w.db, REPO, &dest).await.unwrap();
    assert_eq!(branch(&dest, "main"), w.github_main());
    assert_eq!(report.refs.len(), 1);
}
