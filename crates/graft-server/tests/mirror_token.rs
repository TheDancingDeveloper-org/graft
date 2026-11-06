//! The mirror token reads what it follows (D41, WI-3411): a `mirror`-kind
//! token holding `repo:*:mirror`, the only set mint issues it, polls and
//! imports a repository bound to a mirror through the real server and engine
//! access checks. An unbound repository stays closed to it, and it still
//! writes nothing it does not follow. Mint still refuses any other scope on
//! that kind.

mod common;

use common::{Auth, Site};
use graft_proto::ApiError;
use graft_proto::pb;
use graft_proto::pb::mirror_service_client::MirrorServiceClient;
use std::collections::BTreeMap;

#[tokio::test]
async fn the_mirror_token_reads_only_bound_repositories() {
    let site = Site::new("mirror-read", "");
    let server = site.start();

    let admin = site.mint(&["repos:admin", "repo:*:admin"]);
    let mut owner = server.clients(Some(&admin)).await;
    let snapshot = BTreeMap::from([("a".to_owned(), b"1".to_vec())]);
    let mut tips = Vec::new();
    for repo in ["r", "u"] {
        owner.create_repo(repo).await.expect("repo");
        let created = owner.import(repo, std::slice::from_ref(&snapshot)).await;
        owner
            .set_line(repo, "main", created[0].id)
            .await
            .expect("line");
        tips.extend(created);
    }
    // Only `r` is bound to a mirror.
    let operator = site.mint_kind("op", "human", &["mirror:bind"], 3600);
    MirrorServiceClient::with_interceptor(server.channel().await, Auth::new(Some(&operator)))
        .set_mirror(pb::SetMirrorRequest {
            repo: "r".into(),
            upstream: "https://github.com/o/r.git".into(),
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
        .expect("bind r");

    // Mint gives the mirror kind `repo:*:mirror` and nothing else: no read
    // of its own, and no per-repository form of the set.
    for scopes in [
        &["repo:*:mirror", "repo:u:read"][..],
        &["repo:*:mirror", "repo:*:read"],
        &["repo:*:mirror", "objects:read"],
        &["repo:r:mirror"],
    ] {
        let mut args = vec!["token", "mint", "--subject", "m", "--kind", "mirror"];
        for s in scopes {
            args.extend(["--scope", s]);
        }
        assert!(!site.admin(&args).status.success(), "{scopes:?}");
    }
    let token = site.mint_kind("mirror-svc", "mirror", &["repo:*:mirror"], 3600);
    let mut mirror = server.clients(Some(&token)).await;
    let list = |repo: &str| pb::ListRefsRequest {
        repo: repo.into(),
        prefix: None,
        page: None,
    };
    let log = |repo: &str, tip: graft_model::ChangesetId| pb::LogRequest {
        repo: repo.into(),
        from: tip.0.0.to_vec(),
        until: None,
        paths: Vec::new(),
        page: None,
    };

    // The bound repository: refs, history and objects.
    let refs = mirror
        .repo
        .list_refs(list("r"))
        .await
        .expect("the mirror lists the refs it polls")
        .into_inner();
    assert!(
        refs.refs
            .iter()
            .any(|r| r.name == "main" && r.target == tips[0].id.0.0.to_vec()),
        "{:?}",
        refs.refs
    );
    let history = mirror
        .repo
        .log(log("r", tips[0].id))
        .await
        .expect("the mirror reads history")
        .into_inner();
    assert_eq!(history.changesets.len(), 1);
    let frames = mirror.get_objects("r", &[tips[0].root.0]).await;
    assert_eq!(frames.len(), 1, "the mirror reads objects");

    // The unbound repository: refused, refs, history and objects alike.
    let denied = |status: tonic::Status| {
        assert!(
            matches!(common::api(status), ApiError::PermissionDenied { .. }),
            "unbound repository readable"
        );
    };
    denied(mirror.repo.list_refs(list("u")).await.expect_err("refs"));
    denied(
        mirror
            .repo
            .log(log("u", tips[1].id))
            .await
            .expect_err("log"),
    );
    let mut objects = mirror
        .objects
        .get_objects(pb::GetObjectsRequest {
            repo: "u".into(),
            ids: vec![tips[1].root.0.0.to_vec()],
        })
        .await
        .map(|s| s.into_inner());
    let refused = match &mut objects {
        Err(status) => status.clone(),
        Ok(stream) => stream.message().await.expect_err("objects"),
    };
    denied(refused);

    // Reading grants no write: an unfollowed line stays closed to it.
    assert!(matches!(
        mirror.set_line("r", "other", tips[0].id).await,
        Err(ApiError::PermissionDenied { .. })
    ));
    server.stop();
}
