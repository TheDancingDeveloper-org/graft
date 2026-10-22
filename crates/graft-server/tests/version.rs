//! Every gRPC answer carries the server's protocol version, so a client can
//! bind it (`docs/design.md` §7.1): a unary answer, a streamed answer, and
//! an error status all have it in their headers.

mod common;

use common::{Site, noise};
use graft_proto::{PROTOCOL_VERSION, VERSION_HEADER, pb};

fn version() -> String {
    PROTOCOL_VERSION.to_string()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_unary_answer_carries_the_version() {
    let site = Site::new("server-version-unary", "");
    let token = site.mint(&["repo:r:admin"]);
    let server = site.start();
    let mut c = server.clients(Some(&token)).await;

    let resp = c
        .admin
        .create_repo(pb::CreateRepoRequest { name: "r".into() })
        .await
        .expect("create");
    assert_eq!(
        resp.metadata()
            .get(VERSION_HEADER)
            .and_then(|v| v.to_str().ok()),
        Some(version().as_str())
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_streamed_answer_carries_the_version() {
    let site = Site::new("server-version-stream", "");
    let token = site.mint(&["repo:r:admin"]);
    let server = site.start();
    let mut c = server.clients(Some(&token)).await;
    c.create_repo("r").await.expect("create");
    c.put_blobs("r", &[noise(1, 1 << 10)]).await.expect("put");

    // A server stream: the headers come before any message.
    let resp = c
        .objects
        .get_objects(pb::GetObjectsRequest {
            repo: "r".into(),
            ids: Vec::new(),
        })
        .await
        .expect("stream");
    assert_eq!(
        resp.metadata()
            .get(VERSION_HEADER)
            .and_then(|v| v.to_str().ok()),
        Some(version().as_str())
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_error_status_carries_the_version() {
    let site = Site::new("server-version-error", "");
    // A repository-scoped token may not create a repository: PermissionDenied.
    let token = site.mint(&["repo:r:read"]);
    let server = site.start();
    let mut c = server.clients(Some(&token)).await;

    let status = c
        .admin
        .create_repo(pb::CreateRepoRequest { name: "r".into() })
        .await
        .expect_err("not an admin");
    assert_eq!(status.code(), tonic::Code::PermissionDenied);
    assert_eq!(
        status
            .metadata()
            .get(VERSION_HEADER)
            .and_then(|v| v.to_str().ok()),
        Some(version().as_str())
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_unauthenticated_error_carries_the_version() {
    let site = Site::new("server-version-unauth", "");
    let server = site.start();
    let mut c = server.clients(None).await;

    let status = c
        .admin
        .create_repo(pb::CreateRepoRequest { name: "r".into() })
        .await
        .expect_err("no token");
    assert_eq!(status.code(), tonic::Code::Unauthenticated);
    assert_eq!(
        status
            .metadata()
            .get(VERSION_HEADER)
            .and_then(|v| v.to_str().ok()),
        Some(version().as_str())
    );
}
