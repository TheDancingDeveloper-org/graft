//! `Gc` through the engine: the dry run reports what expiry makes
//! collectable, and the sweep is refused (`docs/plan.md` G2.9, G3.2).

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

#[test]
fn the_sweep_is_refused() {
    let f = memory();
    let repo = Repo::new(&f, "gc");
    match block_on(repo.e().gc(
        &admin("gc"),
        Gc {
            repo: "gc".to_owned(),
            dry_run: false,
        },
    )) {
        Err(ApiError::Rejected { policy, .. }) => assert_eq!(policy, "gc"),
        other => panic!("expected Rejected, got {other:?}"),
    }
}
