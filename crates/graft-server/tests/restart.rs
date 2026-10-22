//! Restart to serving with a full object store (design §15: under 2 s with
//! 1 GB of objects).
//!
//! By default the store holds 128 MiB in 16 KiB objects, which keeps CI
//! fast. Set `GRAFT_RESTART_OBJECTS` to scale it: 65536 gives the 1 GiB of
//! the acceptance criterion.

mod common;

use std::time::Duration;

use common::{Site, fill_objects};

#[test]
fn restart_to_serving_is_under_two_seconds() {
    let count: usize = std::env::var("GRAFT_RESTART_OBJECTS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(8192);
    let site = Site::new("server-restart", "");
    // A first start creates the stores; then fill the object tree directly.
    site.start().stop();
    let bytes = fill_objects(&site.objects_dir(), count, 16 << 10);

    // Warm and cold page cache differ; take the worst of three starts.
    let mut worst = Duration::ZERO;
    for _ in 0..3 {
        let server = site.start();
        worst = worst.max(server.startup);
        assert_eq!(server.get("/readyz", None).status, 200);
        server.kill9();
    }
    eprintln!("{count} objects, {bytes} bytes: restart to serving {worst:?}");
    assert!(
        worst < Duration::from_secs(2),
        "restart to serving took {worst:?} with {bytes} bytes"
    );
}
