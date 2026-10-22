//! The socket: permissions, stale and live sockets, the connection bound,
//! and the protocol version on every answer.

mod common;

use std::os::unix::fs::PermissionsExt as _;
use std::time::Duration;

use common::*;
use graft_node::socket;
use graft_proto::types::GetObjects;
use graft_proto::{ApiError, ObjectApi as _};
use tokio::io::AsyncReadExt as _;

fn mode(path: &std::path::Path) -> u32 {
    std::fs::metadata(path).unwrap().permissions().mode() & 0o777
}

#[tokio::test]
async fn the_socket_gets_its_mode_and_never_more() {
    let dir = tempfile::tempdir().unwrap();
    for want in [0o600, 0o660] {
        let path = dir.path().join(format!("s{want:o}.sock"));
        let _l = socket::bind(&path, want).unwrap();
        assert_eq!(mode(&path), want);
    }
    // No staging directory is left behind.
    let names: Vec<_> = std::fs::read_dir(dir.path())
        .unwrap()
        .map(|e| e.unwrap().file_name().into_string().unwrap())
        .collect();
    assert!(names.iter().all(|n| n.ends_with(".sock")), "{names:?}");
}

#[tokio::test]
async fn a_stale_socket_is_replaced_but_a_live_one_or_a_file_is_refused() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("node.sock");
    drop(std::os::unix::net::UnixListener::bind(&path).unwrap());
    assert!(path.exists());
    let live = socket::bind(&path, 0o600).expect("a stale socket is replaced");

    let refused = socket::bind(&path, 0o600).unwrap_err();
    assert_eq!(refused.kind(), std::io::ErrorKind::AddrInUse);
    drop(live);

    let file = dir.path().join("file");
    std::fs::write(&file, b"keep me").unwrap();
    assert!(socket::bind(&file, 0o600).is_err());
    assert_eq!(std::fs::read(&file).unwrap(), b"keep me");
}

#[tokio::test]
async fn connections_past_the_bound_are_closed_at_once() {
    let up = Upstream::start().await;
    let dir = tempfile::tempdir().unwrap();
    let mut cfg = config(dir.path(), &up.url());
    cfg.max_connections = 2;
    let node = NodeRun::start(cfg).await;
    let mut held = Vec::new();
    for _ in 0..2 {
        held.push(tokio::net::UnixStream::connect(&node.socket).await.unwrap());
    }
    let mut extra = tokio::net::UnixStream::connect(&node.socket).await.unwrap();
    let mut buf = [0u8; 64];
    let n = tokio::time::timeout(Duration::from_secs(2), extra.read(&mut buf))
        .await
        .expect("closed promptly")
        .unwrap_or(0);
    assert_eq!(n, 0, "the extra connection is closed");
    assert_eq!(node.node.metrics().connections_refused.get(), 1);
    assert_eq!(node.node.metrics().connections.get(), 2);

    // Freeing one lets a client in.
    drop(held.pop());
    tokio::time::sleep(Duration::from_millis(100)).await;
    let (id, frame) = blob(1);
    up.put("r", id, frame);
    let client = node.client(&token(&["repo:r:read"])).await;
    let objects = client
        .get_objects(
            &cx(),
            GetObjects {
                repo: "r".into(),
                ids: vec![id],
            },
        )
        .await
        .unwrap();
    assert_eq!(objects.found.len(), 1);
}

#[tokio::test]
async fn an_incompatible_client_is_refused_with_the_node_version() {
    let up = Upstream::start().await;
    let dir = tempfile::tempdir().unwrap();
    let node = NodeRun::start(config(dir.path(), &up.url())).await;
    let client = node
        .client(&token(&["repo:r:read"]))
        .await
        .with_protocol_version(graft_proto::ProtocolVersion { major: 9, minor: 0 });
    let got = client
        .get_objects(
            &cx(),
            GetObjects {
                repo: "r".into(),
                ids: vec![],
            },
        )
        .await;
    assert!(
        matches!(got, Err(ApiError::IncompatibleProtocol { .. })),
        "{got:?}"
    );
}
