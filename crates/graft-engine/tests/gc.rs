//! `Gc` is refused, for a dry run and for a sweep, until the planner matches
//! the design (WI-2203, `docs/plan.md` G2.9). No path from the admin API
//! reaches a delete.

mod common;

use common::*;
use graft_proto::types::Gc;
use graft_proto::{AdminApi, ApiError};

#[test]
fn gc_is_refused_for_dry_run_and_for_sweep() {
    let f = memory();
    let repo = Repo::new(&f, "gc");
    for dry_run in [true, false] {
        let req = Gc {
            repo: "gc".to_owned(),
            dry_run,
        };
        match block_on(repo.e().gc(&admin("gc"), req)) {
            Err(ApiError::Rejected { policy, .. }) => assert_eq!(policy, "gc"),
            other => panic!("expected Rejected for dry_run={dry_run}, got {other:?}"),
        }
    }
}
