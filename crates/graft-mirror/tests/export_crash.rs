//! The export crash harness (work item G3.3): `kill -9` at any point
//! resumes with no duplicate and no gap.
//!
//! Each child process opens a graft engine over the SQLite metadata store
//! and the filesystem object store, the mirror DB, the local mirror and a
//! bare "GitHub" in one shared directory. It then lands and exports until it
//! is killed at a seeded random moment: inside a land, an export, a push, or
//! a DB write. After every kill the parent checks that GitHub's first-parent
//! history is exactly a prefix of graft's line, one commit per changeset in
//! line order, that every push the child acknowledged is still there, and
//! that the line was never stopped as diverged. A last clean run must then
//! catch GitHub up to the line exactly.

#![cfg(unix)]

mod support;

use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use graft_engine::{Engine, EngineConfig};
use graft_git::GitSha;
use graft_git::map::MirrorMap;
use graft_mirror::credentials::NoCredentials;
use graft_mirror::export::{Export, ExportConfig, ExportOutcome};
use graft_mirror::follow::FollowLine;
use graft_mirror::{Mirror, MirrorDb, current_ref};
use graft_proto::api::{AdminApi, RepoApi};
use graft_proto::types::ResolveRef;
use graft_store_fs::{FsMode, FsObjectStore};
use graft_store_sqlite::{SqliteConfig, SqliteMetaStore};
use graft_testkit::clock::FakeClock;
use graft_testkit::crash;
use support::*;

const TEST: &str = "kill_9_at_any_point_resumes_with_no_duplicate_and_no_gap";
const SEED: u64 = 0x5eed_0fc4_a54e_5500;
const ITERATIONS: u32 = 16;
/// Lands a child makes at most, so a child that is never killed ends.
const MAX_LANDS: u64 = 2_000;

type Api = Engine<SqliteMetaStore, FsObjectStore>;

fn open_engine(dir: &Path) -> Api {
    let meta = SqliteMetaStore::open(SqliteConfig::new(dir.join("meta.sqlite"))).unwrap();
    let objects = FsObjectStore::open(dir.join("objects"), FsMode::Authoritative).unwrap();
    Engine::new(
        Arc::new(meta),
        Arc::new(objects),
        Arc::new(FakeClock::at(1_000_000)),
        EngineConfig::default(),
    )
}

fn runtime() -> tokio::runtime::Runtime {
    tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .unwrap()
}

fn main_line() -> FollowLine {
    FollowLine {
        branch: "main".to_owned(),
        line: "main".to_owned(),
    }
}

fn github_dir(dir: &Path) -> std::path::PathBuf {
    dir.join("github.git")
}

fn mirror(dir: &Path) -> Mirror {
    Mirror::new(dir.join("mirror.git"), github_dir(dir).to_str().unwrap())
}

/// The land with sequence number `k` on top of the line: one or two files
/// of its own, fixed by `k`, so a land retried after a kill is the same
/// changeset.
fn land_files(k: u64) -> Vec<(String, Vec<u8>)> {
    let mut files = vec![(
        format!("d{}/k{k:05}.txt", k % 7),
        format!("{k}\n").into_bytes(),
    )];
    if k.is_multiple_of(3) {
        files.push(("shared.txt".to_owned(), format!("rev {k}\n").into_bytes()));
    }
    files.sort();
    files
}

/// The child: land, export, acknowledge, until killed.
async fn child(dir: &Path) {
    let api = open_engine(dir);
    let db = MirrorDb::open(&dir.join("mirror.sqlite")).unwrap();
    let mut export = Export::new(
        &api,
        cx(),
        &db,
        mirror(dir),
        &NoCredentials,
        ExportConfig::new(REPO, vec![main_line()]),
    )
    .unwrap();
    loop {
        // The line's version numbers the lands, so a land cut short by a
        // kill is retried as the same changeset.
        let view = api
            .resolve_ref(
                &cx(),
                ResolveRef {
                    repo: REPO.to_owned(),
                    name: "main".to_owned(),
                },
            )
            .await
            .unwrap();
        let k = view.version;
        if k >= MAX_LANDS {
            return;
        }
        let id = changeset(
            &api,
            Some(view.target),
            &land_files(k),
            &format!("land {k}"),
            k,
        )
        .await;
        move_line(&api, "main", Some(view.target), id).await;
        crash::ack(&format!("L {k} {id}"));
        // Every other land is exported at once; the rest in pairs.
        if k.is_multiple_of(2) {
            continue;
        }
        let (_, outcome) = export.poll_once().await.unwrap().pop().unwrap();
        match outcome {
            ExportOutcome::Pushed { to, .. } => crash::ack(&format!("P {to}")),
            ExportOutcome::UpToDate => {}
            other => panic!("export must never stop on its own pushes: {other:?}"),
        }
    }
}

fn setup(dir: &Path) {
    github(&github_dir(dir), true);
    runtime().block_on(async {
        let api = open_engine(dir);
        api.create_repo(&cx(), REPO.to_owned()).await.unwrap();
        let root = changeset(
            &api,
            None,
            &[("README".to_owned(), b"hi\n".to_vec())],
            "root",
            0,
        )
        .await;
        move_line(&api, "main", None, root).await;
    });
}

/// Checks GitHub against graft's line: an exact prefix (all of it when
/// `complete`), every acknowledged push still on it, and no stop.
fn verify(dir: &Path, acked_pushes: &[String], complete: bool) {
    runtime().block_on(async {
        let api = open_engine(dir);
        let db = MirrorDb::open(&dir.join("mirror.sqlite")).unwrap();
        let chain = line_chain(&api).await;
        let github = github_dir(dir);
        let mapped: Vec<_> = match branch(&github, "main") {
            None => Vec::new(),
            Some(_) => git(
                &github,
                &["rev-list", "--first-parent", "--reverse", "main"],
            )
            .lines()
            .map(|s| {
                let sha = GitSha::from_hex(s).unwrap();
                db.changeset_of(sha)
                    .unwrap()
                    .unwrap_or_else(|| panic!("GitHub commit {s} is not a recorded export"))
            })
            .collect(),
        };
        assert!(
            mapped.len() <= chain.len() && mapped[..] == chain[..mapped.len()],
            "GitHub must be a prefix of graft's line: no gap, no duplicate \
             ({} commits on GitHub, {} changesets on the line)",
            mapped.len(),
            chain.len()
        );
        if complete {
            assert_eq!(mapped, chain, "the clean run caught GitHub up");
        }
        for sha in acked_pushes {
            git(&github, &["merge-base", "--is-ancestor", sha, "main"]);
        }
        if let Some(state) = db.export_state("main").unwrap() {
            assert!(!state.diverged, "a kill must never look like divergence");
        }
        // The line is intact after the kills.
        assert!(
            current_ref(&api, &cx(), REPO, "main")
                .await
                .unwrap()
                .is_some()
        );
    });
}

#[test]
fn kill_9_at_any_point_resumes_with_no_duplicate_and_no_gap() {
    if let Some(c) = crash::child() {
        runtime().block_on(child(&c.dir));
        return;
    }
    let dir = crash::TempDir::new("mirror-export-crash");
    setup(dir.path());
    let mut delays = crash::Delays::new(SEED);
    let mut acked = Vec::new();
    let mut killed = 0;
    for iteration in 1..=ITERATIONS {
        let delay = delays.next(Duration::ZERO, Duration::from_millis(400));
        let run = crash::run_child(TEST, dir.path(), iteration, delay);
        killed += u32::from(run.killed);
        acked.extend(
            run.acks
                .iter()
                .filter_map(|a| a.strip_prefix("P "))
                .map(str::to_owned),
        );
        verify(dir.path(), &acked, false);
    }
    assert!(killed > ITERATIONS / 2, "most children were killed mid-run");
    assert!(!acked.is_empty(), "the children pushed");

    // A clean run catches GitHub up exactly.
    runtime().block_on(async {
        let api = open_engine(dir.path());
        let db = MirrorDb::open(&dir.path().join("mirror.sqlite")).unwrap();
        let mut export = Export::new(
            &api,
            cx(),
            &db,
            mirror(dir.path()),
            &NoCredentials,
            ExportConfig::new(REPO, vec![main_line()]),
        )
        .unwrap();
        loop {
            match export.poll_once().await.unwrap().pop().unwrap().1 {
                ExportOutcome::UpToDate => break,
                ExportOutcome::Pushed { .. } => {}
                other => panic!("{other:?}"),
            }
        }
    });
    verify(dir.path(), &acked, true);
}
