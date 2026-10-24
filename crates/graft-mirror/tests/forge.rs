//! `HttpForge::merging` against a fake GitHub GraphQL API (wiremock): a PR
//! queued or set to auto-merge is merging, and a PR the token cannot see is
//! an error, never "not merging".

use graft_mirror::credentials::{SecretToken, StaticToken};
use graft_mirror::github::{Forge, ForgeError, HttpForge, RetryPolicy};
use serde_json::{Value, json};
use wiremock::matchers::{body_partial_json, method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

fn forge(server: &MockServer) -> HttpForge<StaticToken> {
    HttpForge::new(
        &server.uri(),
        "o",
        "r",
        StaticToken::new(SecretToken::new("fake-token-for-tests-only".into()).unwrap()),
        RetryPolicy {
            attempts: 1,
            ..RetryPolicy::default()
        },
    )
    .unwrap()
}

async fn answer(server: &MockServer, node: &str, body: Value) {
    Mock::given(method("POST"))
        .and(path("/graphql"))
        .and(body_partial_json(json!({ "variables": { "id": node } })))
        .respond_with(ResponseTemplate::new(200).set_body_json(body))
        .mount(server)
        .await;
}

#[tokio::test]
async fn merging_reads_the_queue_and_auto_merge_and_refuses_an_unseen_pr() {
    let server = MockServer::start().await;
    answer(
        &server,
        "PR_idle",
        json!({ "data": { "node": { "isInMergeQueue": false, "autoMergeRequest": null } } }),
    )
    .await;
    answer(
        &server,
        "PR_queued",
        json!({ "data": { "node": { "isInMergeQueue": true, "autoMergeRequest": null } } }),
    )
    .await;
    answer(
        &server,
        "PR_auto",
        json!({ "data": { "node": {
            "isInMergeQueue": false,
            "autoMergeRequest": { "enabledAt": "2026-10-24T00:00:00Z" }
        } } }),
    )
    .await;
    answer(&server, "PR_hidden", json!({ "data": { "node": null } })).await;
    answer(
        &server,
        "PR_error",
        json!({ "data": null, "errors": [{ "message": "Could not resolve to a node" }] }),
    )
    .await;

    let f = forge(&server);
    assert!(!f.merging("PR_idle").await.unwrap());
    assert!(f.merging("PR_queued").await.unwrap());
    assert!(f.merging("PR_auto").await.unwrap());
    assert!(matches!(
        f.merging("PR_hidden").await,
        Err(ForgeError::Refused(_))
    ));
    assert!(matches!(
        f.merging("PR_error").await,
        Err(ForgeError::Refused(_))
    ));
}
