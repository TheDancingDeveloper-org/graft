//! The mirror uploads objects over the streamed `PutObjects` (WI-3591): a
//! `mirror`-kind token holding only `repo:*:mirror` is let through the
//! server's pre-buffer check by the engine's lease-fenced mirror check, not
//! by a write scope. It needs the live lease, an enabled follow row and the
//! followed-line marker; a read token stays refused.

mod common;

use common::{Auth, Site};
use graft_proto::ApiError;
use graft_proto::pb;
use graft_proto::pb::mirror_service_client::MirrorServiceClient;
use graft_proto::types::{RefKind, UpdateRef, followed_ref};
use std::collections::BTreeMap;

fn refused(result: Result<(), ApiError>, why: &str) {
    assert!(
        matches!(result, Err(ApiError::PermissionDenied { .. })),
        "{why}: {result:?}"
    );
}

#[tokio::test]
async fn the_mirror_uploads_objects_only_under_its_lease_on_a_followed_repository() {
    let site = Site::new("mirror-put", "");
    let server = site.start();
    let admin = site.mint(&["repos:admin", "repo:*:admin"]);
    let mut owner = server.clients(Some(&admin)).await;
    let snapshot = BTreeMap::from([("a".to_owned(), b"1".to_vec())]);
    // `r` gets everything; `nomark` lacks the marker, `norow` the row,
    // `nolease` the lease.
    let mut tips = BTreeMap::new();
    for repo in ["r", "nomark", "norow", "nolease"] {
        owner.create_repo(repo).await.expect("repo");
        let created = owner.import(repo, std::slice::from_ref(&snapshot)).await;
        owner
            .set_line(repo, "main", created[0].id)
            .await
            .expect("line");
        tips.insert(repo, created[0].id);
    }
    let operator = site.mint_kind("op", "human", &["mirror:bind"], 3600);
    let mut op =
        MirrorServiceClient::with_interceptor(server.channel().await, Auth::new(Some(&operator)));
    for repo in ["r", "nomark", "nolease"] {
        op.set_mirror(pb::SetMirrorRequest {
            repo: repo.into(),
            upstream: format!("https://github.com/o/{repo}.git"),
            lines: vec![pb::MirrorLine {
                branch: "main".into(),
                line: "main".into(),
                direction: "follow".into(),
            }],
            interval_s: 0,
            enabled: true,
            export: None,
            expected_version: None,
        })
        .await
        .expect("bind");
    }
    for repo in ["r", "norow", "nolease"] {
        owner
            .repo
            .update_ref(pb::UpdateRefRequest::from(UpdateRef {
                repo: repo.into(),
                name: followed_ref("main"),
                expected: None,
                new: tips[repo],
                kind: RefKind::Tool,
                lease_epoch: None,
            }))
            .await
            .expect("marker");
    }

    let token = site.mint_kind("mirror-svc", "mirror", &["repo:*:mirror"], 3600);
    let mut lease =
        MirrorServiceClient::with_interceptor(server.channel().await, Auth::new(Some(&token)));
    for repo in ["r", "nomark", "norow"] {
        lease
            .acquire_lease(pb::AcquireLeaseRequest {
                repo: repo.into(),
                instance: "i1".into(),
            })
            .await
            .expect("lease");
    }
    let mut mirror = server.clients(Some(&token)).await;
    let blob = vec![b"imported by the mirror".to_vec()];

    // (a) Lease, row and marker: the upload is accepted.
    mirror
        .put_blobs("r", &blob)
        .await
        .expect("the lease-holding mirror uploads to a followed repository");

    // (b) Each missing piece refuses it.
    refused(mirror.put_blobs("nomark", &blob).await, "no marker");
    refused(mirror.put_blobs("norow", &blob).await, "no follow row");
    refused(mirror.put_blobs("nolease", &blob).await, "no lease");

    // (c) A read token is refused on the fully set-up repository.
    let reader = site.mint(&["repo:*:read"]);
    let mut reader = server.clients(Some(&reader)).await;
    refused(reader.put_blobs("r", &blob).await, "read token");
    server.stop();
}
