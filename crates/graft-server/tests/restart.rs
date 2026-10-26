//! Restart to serving with a full object store.
//!
//! By default the store holds 128 MiB in 16 KiB objects, which keeps CI
//! fast. Set `GRAFT_RESTART_OBJECTS` to scale it: 65536 gives the 1 GiB of
//! the design's size example.
//!
//! This test asserts outcomes only: the server answers `/readyz` after a
//! restart, and a repo created before the restart is still listed. Startup
//! time is printed, never asserted (AGENTS.md rule 10, WI-2480). The
//! `restart-timing` CI job records the same measurement without a pass or
//! fail on time.

mod common;

use common::{Site, fill_objects};

#[tokio::test]
async fn restart_serves_and_keeps_an_acknowledged_write() {
    let count: usize = std::env::var("GRAFT_RESTART_OBJECTS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(8192);
    let site = Site::new("server-restart", "");
    // A first start creates the stores; then fill the object tree directly.
    site.start().stop();
    let bytes = fill_objects(&site.objects_dir(), count, 16 << 10);

    let token = site.mint(&["repo:kept:admin"]);
    let server = site.start();
    eprintln!(
        "{count} objects, {bytes} bytes: restart to serving {:?}",
        server.startup
    );
    assert_eq!(server.get("/readyz", None).status, 200);
    server
        .clients(Some(&token))
        .await
        .create_repo("kept")
        .await
        .expect("create repo");
    server.kill9();

    let again = site.start();
    eprintln!("second restart to serving {:?}", again.startup);
    assert_eq!(again.get("/readyz", None).status, 200);
    let refs = again
        .clients(Some(&token))
        .await
        .repo
        .list_refs(graft_proto::pb::ListRefsRequest {
            repo: "kept".into(),
            prefix: None,
            page: None,
        })
        .await
        .expect("list refs of the acknowledged repo")
        .into_inner();
    assert!(refs.refs.is_empty(), "a new repo has no refs");
    again.stop();
}
