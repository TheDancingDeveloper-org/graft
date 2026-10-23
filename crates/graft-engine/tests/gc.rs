//! `Gc` through the engine: the dry run reports what expiry makes
//! collectable, and the sweep is refused until the operator turns deletion
//! on, then does what the dry run said (`docs/plan.md` G2.9, G3.2).

mod common;

use common::*;
use graft_proto::types::{Gc, GcReport};
use graft_proto::{AdminApi, ApiError};

const RETENTION_MS: i64 = 7 * 24 * 3600 * 1000;

fn dry_run(repo: &Repo<'_, graft_store::memory::MemoryStore>) -> GcReport {
    block_on(repo.e().gc(
        &admin("gc"),
        Gc {
            repo: "gc".to_owned(),
            dry_run: true,
        },
    ))
    .expect("dry run")
}

#[test]
fn an_abandoned_changeset_is_reported_once_its_retention_has_passed() {
    let f = memory();
    let repo = Repo::new(&f, "gc");
    // Without a workspace, the commit is created abandoned, and it holds its
    // blob and its root tree.
    repo.commit(vec![], &[("a.txt", b"hello".as_slice())], &[]);

    assert_eq!(dry_run(&repo), GcReport::default());

    f.clock.advance(RETENTION_MS + 1);
    let report = dry_run(&repo);
    assert_eq!(report.changesets, 1);
    assert_eq!(report.objects, 2);
    assert!(report.bytes > 0);
}

fn sweep(repo: &Repo<'_, graft_store::memory::MemoryStore>) -> Result<GcReport, ApiError> {
    block_on(repo.e().gc(
        &admin("gc"),
        Gc {
            repo: "gc".to_owned(),
            dry_run: false,
        },
    ))
}

#[test]
fn the_sweep_is_refused_until_deletion_is_on_then_does_what_the_dry_run_said() {
    let f = memory();
    let repo = Repo::new(&f, "gc");
    repo.commit(vec![], &[("a.txt", b"hello".as_slice())], &[]);
    f.clock.advance(RETENTION_MS + 1);
    let planned = dry_run(&repo);
    assert_eq!((planned.changesets, planned.objects), (1, 2));

    match sweep(&repo) {
        Err(ApiError::Rejected { policy, detail }) => {
            assert_eq!(policy, "gc");
            assert!(detail.contains("deletion is off"), "{detail}");
        }
        other => panic!("expected Rejected, got {other:?}"),
    }
    let meta = repo.e().stores().0;
    let id = block_on(graft_store::MetaStore::repo(meta, "gc"))
        .expect("read")
        .expect("repo");
    block_on(graft_gc::set_deletion(meta, id, true, "ops")).expect("on");

    // The first pass collects and dooms; the grace period holds the frames.
    let first = sweep(&repo).expect("sweep");
    assert_eq!(
        first,
        GcReport {
            changesets: 1,
            objects: 0,
            bytes: 0
        }
    );
    f.clock.advance(15 * 60 * 1000 + 1);
    let second = sweep(&repo).expect("sweep");
    assert_eq!(
        second,
        GcReport {
            changesets: 0,
            objects: 2,
            bytes: planned.bytes,
        },
        "the dry run's bytes are what the sweep reclaims"
    );
    assert_eq!(dry_run(&repo), GcReport::default(), "nothing left");
}
