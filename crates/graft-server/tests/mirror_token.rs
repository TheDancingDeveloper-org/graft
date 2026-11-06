//! The mirror token reads what it follows (D41, WI-3411): a `mirror`-kind
//! token holding only `repo:*:mirror` polls and imports through the real
//! server and engine access checks, and still writes nothing it does not
//! follow.

mod common;

use std::collections::BTreeMap;

use common::Site;
use graft_proto::ApiError;
use graft_proto::pb;

#[tokio::test]
async fn the_mirror_token_reads_the_repositories_it_follows() {
    let site = Site::new("mirror-read", "");
    let server = site.start();

    let admin = site.mint(&["repos:admin", "repo:r:admin"]);
    let mut owner = server.clients(Some(&admin)).await;
    owner.create_repo("r").await.expect("repo");
    let snapshot = BTreeMap::from([("a".to_owned(), b"1".to_vec())]);
    let created = owner.import("r", &[snapshot]).await;
    let tip = created[0].id;
    owner.set_line("r", "main", tip).await.expect("line");

    let token = site.mint_kind("mirror-svc", "mirror", &["repo:*:mirror"], 3600);
    let mut mirror = server.clients(Some(&token)).await;
    let refs = mirror
        .repo
        .list_refs(pb::ListRefsRequest {
            repo: "r".into(),
            prefix: None,
            page: None,
        })
        .await
        .expect("the mirror lists the refs it polls")
        .into_inner();
    assert!(
        refs.refs
            .iter()
            .any(|r| r.name == "main" && r.target == tip.0.0.to_vec()),
        "{:?}",
        refs.refs
    );
    let history = mirror
        .repo
        .log(pb::LogRequest {
            repo: "r".into(),
            from: tip.0.0.to_vec(),
            until: None,
            paths: Vec::new(),
            page: None,
        })
        .await
        .expect("the mirror reads history")
        .into_inner();
    assert_eq!(history.changesets.len(), 1);
    let frames = mirror.get_objects("r", &[created[0].root.0]).await;
    assert_eq!(frames.len(), 1, "the mirror reads objects");

    // Reading grants no write: an unfollowed line stays closed to it.
    assert!(matches!(
        mirror.set_line("r", "other", tip).await,
        Err(ApiError::PermissionDenied { .. })
    ));
    server.stop();
}
