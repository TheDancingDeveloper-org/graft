//! G3.2's crash harness, process-kill half (`graft-testkit` `crash`, design
//! §6.6): a child process sweeps a repository on the SQLite metadata store
//! and the filesystem object store, and is killed with `SIGKILL` at a
//! random point. The parent reopens both stores, runs start-up recovery and
//! sweeps to the end, and checks that
//!
//! - no `deleting` mark survives recovery, and no doomed mark the end;
//! - every landed changeset's snapshot is whole, and the brute-force check
//!   of design §8.6 holds;
//! - every draft was collected and every frame only drafts held is gone.
//!
//! The deterministic half, with a crash at every call, is `crash.rs`.

#![cfg(unix)]

mod common;

use std::collections::BTreeSet;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use common::*;
use graft_engine::Engine;
use graft_gc::{Collector, NoPace};
use graft_model::{Change, ChangesetId, Clock, ObjectId, ObjectKind};
use graft_proto::types::*;
use graft_proto::{AdminApi, Frame, ObjectApi, RepoApi, WorkspaceApi};
use graft_store::{MetaStore, ObjectStore, RecordState, RepoId};
use graft_store_fs::{FsMode, FsObjectStore};
use graft_store_sqlite::{SqliteConfig, SqliteMetaStore};
use graft_testkit::clock::FakeClock;
use graft_testkit::crash::{self, Delays};

const TEST: &str = "a_sweep_killed_at_any_point_recovers";
const ITERATIONS: u32 = 10;
const SEED: u64 = 0x6c_3200;
const WORKSPACES: usize = 3;
const STACK: usize = 8;

fn open(dir: &Path) -> (Arc<SqliteMetaStore>, Arc<FsObjectStore>) {
    let meta = SqliteMetaStore::open(SqliteConfig::new(dir.join("meta.sqlite"))).expect("sqlite");
    let objects = FsObjectStore::open(dir.join("objects"), FsMode::Authoritative).expect("fs");
    (Arc::new(meta), Arc::new(objects))
}

fn now_file(dir: &Path) -> std::path::PathBuf {
    dir.join("now")
}

/// A collector over the stores in `dir`, and what drives it.
struct Sweeper {
    gc: Collector<Arc<SqliteMetaStore>, Arc<FsObjectStore>>,
    shape: RealShape<Arc<FsObjectStore>>,
    clock: Arc<FakeClock>,
    repo: RepoId,
}

fn collector(dir: &Path, now: i64) -> Sweeper {
    let (meta, objects) = open(dir);
    let repo = block_on(meta.repo("r")).expect("repo").expect("exists");
    let clock = Arc::new(FakeClock::at(now));
    let config = graft_gc::GcConfig {
        batch: 2,
        pause: Duration::ZERO,
        ..graft_gc::GcConfig::default()
    };
    let gc = Collector::new(meta, objects.clone(), config)
        .with_clock(clock.clone())
        .with_pace(Arc::new(NoPace));
    Sweeper {
        gc,
        shape: RealShape { objects },
        clock,
        repo,
    }
}

/// Writes the repository: a landed root, and stacks of drafts in closed
/// workspaces, each draft with its own blob. Returns the root and the
/// drafts' blobs.
fn build(dir: &Path) -> (ChangesetId, Vec<ObjectId>) {
    let (meta, objects) = open(dir);
    let clock = Arc::new(FakeClock::at(1_000_000));
    let engine = Engine::new(meta.clone(), objects, clock.clone(), engine_config());
    let cx = cx("r");
    block_on(engine.create_repo(&cx, "r".to_owned())).expect("repo");
    let repo = block_on(meta.repo("r")).expect("repo").expect("exists");
    let upload = |data: &[u8]| {
        block_on(engine.put_objects(
            &cx,
            PutObjects {
                repo: "r".to_owned(),
                blobs: vec![Frame::encode(ObjectKind::Blob, data)],
            },
        ))
        .expect("upload")
    };
    let create = |parents: Vec<ChangesetId>, changes: Vec<Change>, ws: Option<&str>, n: usize| {
        let mut req = CreateChangeset {
            repo: "r".to_owned(),
            parents,
            changes,
            author: graft_model::Author {
                name: "Tess".to_owned(),
                email: "tess@example.com".to_owned(),
                kind: graft_model::AuthorKind::Agent,
            },
            authored_at: graft_model::Timestamp {
                ms: 1_700_000_000_000,
                offset_min: 0,
            },
            message: format!("change {n}"),
            metadata: graft_model::Metadata::new(),
            inline_blobs: Vec::new(),
            workspace: None,
        };
        req.workspace = ws.map(str::to_owned);
        block_on(engine.create_changeset(&cx, req))
            .expect("create")
            .id
    };
    upload(b"root");
    let root = create(vec![], vec![put("root", b"root")], None, 0);
    block_on(engine.update_ref(
        &cx,
        UpdateRef {
            repo: "r".to_owned(),
            name: LINE.to_owned(),
            expected: None,
            new: root,
            kind: RefKind::Line,
        },
    ))
    .expect("line");
    block_on(graft_gc::set_deletion(&*meta, repo, true, "tester")).expect("switch on");

    let mut blobs = Vec::new();
    for ws in 0..WORKSPACES {
        let name = format!("w{ws}");
        block_on(engine.open_workspace(
            &cx,
            OpenWorkspace {
                repo: "r".to_owned(),
                name: Some(name.clone()),
                line: LINE.to_owned(),
                at: None,
                lease_ms: 0,
            },
        ))
        .expect("open");
        let mut head = root;
        for i in 0..STACK {
            let data = format!("{name} draft {i} {}", "x".repeat(512)).into_bytes();
            upload(&data);
            blobs.push(ObjectId::hash(ObjectKind::Blob, &data));
            head = create(
                vec![head],
                vec![put(&format!("{name}/{i}"), &data)],
                Some(&name),
                ws * STACK + i + 1,
            );
        }
        block_on(engine.close_workspace(
            &cx,
            WorkspaceRef {
                repo: "r".to_owned(),
                name,
            },
            false,
        ))
        .expect("close");
    }
    let start = clock.now().ms + RETENTION_PLUS;
    std::fs::write(now_file(dir), start.to_string()).expect("now");
    (root, blobs)
}

/// The child: sweep pass after pass until nothing is left, the clock past
/// the grace period between passes.
fn sweep_until_killed(dir: &Path) {
    let now: i64 = std::fs::read_to_string(now_file(dir))
        .expect("now")
        .parse()
        .expect("ms");
    let Sweeper {
        gc,
        shape,
        clock,
        repo,
    } = collector(dir, now);
    crash::ack("open");
    loop {
        let r = block_on(gc.sweep(repo, &shape, false)).expect("sweep");
        crash::ack(&format!(
            "pass collected={} deleted={}",
            r.collected, r.deleted
        ));
        if r.collected + r.doomed + r.deleted + r.expired == 0 && !r.more {
            break;
        }
        clock.advance(GRACE_PLUS);
    }
    crash::ack("done");
}

#[test]
fn a_sweep_killed_at_any_point_recovers() {
    if let Some(child) = crash::child() {
        return sweep_until_killed(&child.dir);
    }
    let dir = crash::TempDir::new("graft-gc-kill");
    let mut delays = Delays::new(SEED);
    let (mut killed, mut found_deleting) = (0, 0);
    for iteration in 1..=ITERATIONS {
        let sub = dir.path().join(format!("i{iteration}"));
        std::fs::create_dir_all(&sub).expect("dir");
        let (root, blobs) = build(&sub);
        let run = crash::run_child(
            TEST,
            &sub,
            iteration,
            delays.next(Duration::ZERO, Duration::from_millis(150)),
        );
        killed += u32::from(run.killed);

        // Reopen, as a restarted server would, well after the child's
        // clock.
        let Sweeper {
            gc,
            shape,
            clock,
            repo,
        } = collector(&sub, 1_000_000 + 2 * RETENTION_PLUS);
        let (meta, objects) = gc.stores();
        check(&**meta, &**objects, repo).expect("consistent after the kill");
        let marks = block_on(meta.old_doomed(repo, i64::MAX, None, 1000)).expect("marks");
        found_deleting += marks.iter().filter(|(_, m)| m.deleting).count();
        block_on(gc.recover(repo)).expect("recover");
        assert!(
            block_on(meta.old_doomed(repo, i64::MAX, None, 1000))
                .expect("marks")
                .iter()
                .all(|(_, m)| !m.deleting),
            "recovery finished every delete"
        );
        for _ in 0..32 {
            let r = block_on(gc.sweep(repo, &shape, false)).expect("sweep");
            let left = block_on(meta.old_doomed(repo, i64::MAX, None, 1)).expect("marks");
            if r.collected + r.doomed + r.deleted + r.expired == 0 && left.is_empty() {
                break;
            }
            clock.advance(GRACE_PLUS);
        }
        check(&**meta, &**objects, repo).expect("consistent at the end");
        assert!(
            block_on(meta.old_doomed(repo, i64::MAX, None, 1))
                .expect("marks")
                .is_empty(),
            "nothing left doomed"
        );
        for state in [RecordState::Draft, RecordState::Abandoned] {
            assert!(
                block_on(meta.list_records(repo, state, None, 10))
                    .expect("list")
                    .is_empty(),
                "every draft collected"
            );
        }
        snapshot(&**objects, repo, root_tree(&**meta, repo, root)).expect("the root is whole");
        let present: BTreeSet<ObjectId> = block_on(objects.has(repo, &blobs))
            .expect("has")
            .into_iter()
            .collect();
        assert!(present.is_empty(), "draft frames left: {present:?}");
    }
    eprintln!(
        "{killed} of {ITERATIONS} children killed mid-run; {found_deleting} deletes found half done"
    );
}

fn root_tree<M: MetaStore>(meta: &M, repo: RepoId, id: ChangesetId) -> ObjectId {
    let (content, _) = block_on(meta.changeset(repo, id))
        .expect("read")
        .expect("stored");
    graft_model::Changeset::decode(&content)
        .expect("decode")
        .root
        .0
}
