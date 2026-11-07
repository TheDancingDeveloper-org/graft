//! The Forgejo forge against a fake Forgejo API (wiremock, WI-3492): batch
//! PRs open, read and close over `/api/v1`, and `land_once` rebase-merges a
//! batch only when every required status context is `success`. The GitHub
//! client keeps its own headers.

use graft_mirror::MirrorError;
use graft_mirror::credentials::{SecretToken, StaticToken};
use graft_mirror::forgejo::{ForgejoForge, LandOutcome, land_once};
use graft_mirror::github::{Forge, ForgeError, HttpForge, NewPull, RetryPolicy};
use serde_json::{Value, json};
use wiremock::matchers::{body_json, header, method, path, query_param};
use wiremock::{Mock, MockServer, ResponseTemplate};

const TOKEN: &str = "fake-token-for-tests-only";

fn token() -> StaticToken {
    StaticToken::new(SecretToken::new(TOKEN.into()).unwrap())
}

fn once() -> RetryPolicy {
    RetryPolicy {
        attempts: 1,
        ..RetryPolicy::default()
    }
}

fn forgejo(server: &MockServer) -> ForgejoForge<StaticToken> {
    ForgejoForge::new(&server.uri(), "o", "r", token(), once()).unwrap()
}

fn head() -> String {
    "a".repeat(40)
}

fn pull(number: u64, state: &str, merged: bool, branch: &str, repo: &str) -> Value {
    json!({
        "number": number,
        "html_url": format!("https://forgejo.example/o/r/pulls/{number}"),
        "state": state,
        "merged": merged,
        "merge_commit_sha": if merged { Value::String("b".repeat(40)) } else { Value::Null },
        "merged_at": if merged { Value::String("2023-11-15T09:13:20+11:00".into()) } else { Value::Null },
        "closed_at": null,
        "body": "batch body",
        "head": { "sha": head(), "ref": branch, "repo": { "full_name": repo } },
        "base": { "ref": "main" },
    })
}

async fn serve_pull(server: &MockServer, number: u64, body: Value) {
    Mock::given(method("GET"))
        .and(path(format!("/repos/o/r/pulls/{number}")))
        .respond_with(ResponseTemplate::new(200).set_body_json(body))
        .mount(server)
        .await;
}

async fn serve_statuses(server: &MockServer, statuses: Value) {
    Mock::given(method("GET"))
        .and(path(format!("/repos/o/r/commits/{}/status", head())))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(json!({ "state": "x", "statuses": statuses })),
        )
        .mount(server)
        .await;
}

/// A merge endpoint that must be called `times` times, with a rebase at the
/// PR's current head.
async fn expect_merge(server: &MockServer, number: u64, times: u64) {
    Mock::given(method("POST"))
        .and(path(format!("/repos/o/r/pulls/{number}/merge")))
        .and(body_json(json!({
            "Do": "rebase",
            "head_commit_id": head(),
            "delete_branch_after_merge": false,
        })))
        .respond_with(ResponseTemplate::new(200))
        .expect(times)
        .mount(server)
        .await;
}

fn required() -> Vec<String> {
    vec![
        "ci/woodpecker/pr/test".into(),
        "ci / build (pull_request)".into(),
    ]
}

#[tokio::test]
async fn a_batch_pr_opens_with_forgejo_token_auth() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/repos/o/r/pulls"))
        .and(header("authorization", format!("token {TOKEN}").as_str()))
        .and(header("accept", "application/json"))
        .and(body_json(json!({
            "title": "batch 4", "body": "b", "head": "graft/batch-4", "base": "main"
        })))
        .respond_with(ResponseTemplate::new(201).set_body_json(pull(
            4,
            "open",
            false,
            "graft/batch-4",
            "o/r",
        )))
        .expect(1)
        .mount(&server)
        .await;
    let new = NewPull {
        title: "batch 4".into(),
        body: "b".into(),
        head: "graft/batch-4".into(),
        base: "main".into(),
    };
    let p = forgejo(&server).create_pull(&new).await.unwrap();
    assert_eq!(p.number, 4);
    assert_eq!(p.node_id, "4");
    assert!(p.open && !p.merged);
    assert_eq!(p.head_sha, head());
    assert_eq!(p.head_ref, "graft/batch-4");
}

#[tokio::test]
async fn the_github_client_keeps_its_own_headers() {
    let server = MockServer::start().await;
    let mut gh_pull = pull(9, "open", false, "graft/batch-9", "o/r");
    gh_pull["node_id"] = json!("PR_9");
    Mock::given(method("GET"))
        .and(path("/repos/o/r/pulls/9"))
        .and(header("authorization", format!("Bearer {TOKEN}").as_str()))
        .and(header("accept", "application/vnd.github+json"))
        .and(header("x-github-api-version", "2022-11-28"))
        .respond_with(ResponseTemplate::new(200).set_body_json(gh_pull))
        .expect(1)
        .mount(&server)
        .await;
    let gh = HttpForge::new(&server.uri(), "o", "r", token(), once()).unwrap();
    let p = gh.get_pull(9).await.unwrap();
    assert_eq!(p.node_id, "PR_9");
}

#[tokio::test]
async fn find_pull_takes_the_newest_pr_on_the_branch_in_this_repository() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/repos/o/r/pulls"))
        .and(query_param("state", "all"))
        .and(query_param("sort", "recentupdate"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!([
            pull(12, "open", false, "graft/batch-7", "fork/r"),
            pull(11, "open", false, "other", "o/r"),
            pull(10, "closed", false, "graft/batch-7", "o/r"),
            pull(8, "closed", false, "graft/batch-7", "o/r"),
        ])))
        .mount(&server)
        .await;
    let f = forgejo(&server);
    let found = f.find_pull("graft/batch-7").await.unwrap().unwrap();
    assert_eq!(found.number, 10);
    assert!(!found.open);
    assert!(f.find_pull("graft/batch-99").await.unwrap().is_none());
}

#[tokio::test]
async fn a_merged_pull_reads_its_merge_commit_and_utc_time() {
    let server = MockServer::start().await;
    serve_pull(&server, 5, pull(5, "closed", true, "graft/batch-5", "o/r")).await;
    let p = forgejo(&server).get_pull(5).await.unwrap();
    assert!(p.merged && !p.open);
    assert_eq!(p.merge_commit, Some("b".repeat(40)));
    assert_eq!(p.settled_ms, Some(1_700_000_000_000));
}

#[tokio::test]
async fn green_required_statuses_rebase_merge_the_batch() {
    let server = MockServer::start().await;
    serve_pull(&server, 3, pull(3, "open", false, "graft/batch-3", "o/r")).await;
    serve_statuses(
        &server,
        json!([
            { "context": "ci/woodpecker/pr/test", "status": "success", "target_url": "u" },
            { "context": "ci / build (pull_request)", "status": "success" },
            { "context": "not/required", "status": "failure" },
        ]),
    )
    .await;
    expect_merge(&server, 3, 1).await;
    let out = land_once(&forgejo(&server), 3, &required()).await.unwrap();
    assert_eq!(out, LandOutcome::Merged);
}

#[tokio::test]
async fn pending_or_missing_required_statuses_do_not_merge() {
    let server = MockServer::start().await;
    serve_pull(&server, 3, pull(3, "open", false, "graft/batch-3", "o/r")).await;
    serve_statuses(
        &server,
        json!([{ "context": "ci/woodpecker/pr/test", "status": "pending" }]),
    )
    .await;
    expect_merge(&server, 3, 0).await;
    let out = land_once(&forgejo(&server), 3, &required()).await.unwrap();
    assert_eq!(
        out,
        LandOutcome::Pending(vec![
            "ci/woodpecker/pr/test".into(),
            "ci / build (pull_request)".into()
        ])
    );
}

#[tokio::test]
async fn a_failed_required_status_does_not_merge() {
    let server = MockServer::start().await;
    serve_pull(&server, 3, pull(3, "open", false, "graft/batch-3", "o/r")).await;
    serve_statuses(
        &server,
        json!([
            { "context": "ci/woodpecker/pr/test", "status": "failure" },
            { "context": "ci / build (pull_request)", "status": "success" },
        ]),
    )
    .await;
    expect_merge(&server, 3, 0).await;
    let out = land_once(&forgejo(&server), 3, &required()).await.unwrap();
    assert_eq!(
        out,
        LandOutcome::Failed(vec![("ci/woodpecker/pr/test".into(), "failure".into())])
    );
}

#[tokio::test]
async fn a_closed_or_merged_pr_is_reported_and_not_merged() {
    let server = MockServer::start().await;
    serve_pull(&server, 6, pull(6, "closed", false, "graft/batch-6", "o/r")).await;
    serve_pull(&server, 7, pull(7, "closed", true, "graft/batch-7", "o/r")).await;
    expect_merge(&server, 6, 0).await;
    expect_merge(&server, 7, 0).await;
    let f = forgejo(&server);
    assert_eq!(
        land_once(&f, 6, &required()).await.unwrap(),
        LandOutcome::Closed
    );
    assert_eq!(
        land_once(&f, 7, &required()).await.unwrap(),
        LandOutcome::AlreadyMerged
    );
}

#[tokio::test]
async fn no_required_contexts_refuses_to_land() {
    let server = MockServer::start().await;
    expect_merge(&server, 3, 0).await;
    let out = land_once(&forgejo(&server), 3, &[]).await;
    assert!(matches!(out, Err(MirrorError::Config(_))));
}

#[tokio::test]
async fn a_refused_merge_is_an_error_without_the_token() {
    let server = MockServer::start().await;
    serve_pull(&server, 3, pull(3, "open", false, "graft/batch-3", "o/r")).await;
    serve_statuses(
        &server,
        json!([
            { "context": "ci/woodpecker/pr/test", "status": "success" },
            { "context": "ci / build (pull_request)", "status": "success" },
        ]),
    )
    .await;
    Mock::given(method("POST"))
        .and(path("/repos/o/r/pulls/3/merge"))
        .respond_with(
            ResponseTemplate::new(405).set_body_json(json!({ "message": "not mergeable" })),
        )
        .mount(&server)
        .await;
    let err = land_once(&forgejo(&server), 3, &required())
        .await
        .unwrap_err();
    let text = format!("{err} {err:?}");
    assert!(text.contains("405"), "{text}");
    assert!(!text.contains(TOKEN));
}

#[tokio::test]
async fn closing_comments_then_closes() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/repos/o/r/issues/4/comments"))
        .and(body_json(json!({ "body": "superseded" })))
        .respond_with(ResponseTemplate::new(201).set_body_json(json!({ "id": 1 })))
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("PATCH"))
        .and(path("/repos/o/r/pulls/4"))
        .and(body_json(json!({ "state": "closed" })))
        .respond_with(ResponseTemplate::new(201).set_body_json(pull(
            4,
            "closed",
            false,
            "graft/batch-4",
            "o/r",
        )))
        .expect(1)
        .mount(&server)
        .await;
    forgejo(&server).close_pr(4, "superseded").await.unwrap();
}

#[tokio::test]
async fn a_server_error_is_retried_then_transient_and_merging_is_never_true() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/repos/o/r/pulls/3"))
        .respond_with(ResponseTemplate::new(502))
        .expect(2)
        .mount(&server)
        .await;
    let f = ForgejoForge::new(
        &server.uri(),
        "o",
        "r",
        token(),
        RetryPolicy {
            attempts: 2,
            base: std::time::Duration::from_millis(1),
            max_wait: std::time::Duration::from_millis(1),
        },
    )
    .unwrap();
    assert!(matches!(f.get_pull(3).await, Err(ForgeError::Transient(_))));
    assert!(!f.merging("3").await.unwrap());
    assert!(matches!(f.enqueue("3").await, Err(ForgeError::Refused(_))));
}

/// Runs `graft-mirror land-batch` with `args`, returning its exit code and
/// its output.
fn land_batch_cli(args: &[&str]) -> (Option<i32>, String) {
    let out = std::process::Command::new(env!("CARGO_BIN_EXE_graft-mirror"))
        .arg("land-batch")
        .args(args)
        .env_remove("GRAFT_TOKEN")
        .output()
        .unwrap();
    let text = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    (out.status.code(), text)
}

/// A serve config whose one `[[forgejo]]` host is `git.example`, its API
/// the mock server.
fn serve_config(dir: &std::path::Path, server: &MockServer, token_file: bool) -> String {
    let token = dir.join("forgejo-token");
    std::fs::write(&token, format!("{TOKEN}\n")).unwrap();
    let token_line = if token_file {
        format!("token_file = \"{}\"\n", token.display())
    } else {
        String::new()
    };
    let file = dir.join("serve.toml");
    std::fs::write(
        &file,
        format!(
            "server = \"http://graft:7878\"\ndata_dir = \"{}\"\n\
             [[forgejo]]\nhost = \"git.example\"\napi = \"{}\"\n\
             required_contexts = [\"ci/woodpecker/pr/test\", \"ci / build (pull_request)\"]\n\
             {token_line}",
            dir.join("data").display(),
            server.uri(),
        ),
    )
    .unwrap();
    file.display().to_string()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn land_batch_lands_a_serve_mirrored_repo_from_its_forgejo_host() {
    let server = MockServer::start().await;
    let tmp = tempfile::tempdir().unwrap();
    let config = serve_config(tmp.path(), &server, true);
    serve_pull(&server, 9, pull(9, "open", false, "graft/batch-9", "o/r")).await;
    serve_statuses(
        &server,
        json!([
            { "context": "ci/woodpecker/pr/test", "status": "success", "target_url": "" },
            { "context": "ci / build (pull_request)", "status": "success", "target_url": "" },
        ]),
    )
    .await;
    Mock::given(method("POST"))
        .and(path("/repos/o/r/pulls/9/merge"))
        .and(header("authorization", format!("token {TOKEN}").as_str()))
        .and(body_json(json!({
            "Do": "rebase",
            "head_commit_id": head(),
            "delete_branch_after_merge": false,
        })))
        .respond_with(ResponseTemplate::new(200))
        .expect(1)
        .mount(&server)
        .await;
    let (code, out) = tokio::task::spawn_blocking(move || {
        land_batch_cli(&[
            "9",
            "--serve-config",
            &config,
            "--upstream",
            "https://git.example/o/r.git",
        ])
    })
    .await
    .unwrap();
    assert_eq!(code, Some(0), "{out}");
    assert!(out.contains("merged (rebase)"), "{out}");
    assert!(!out.contains(TOKEN), "the token printed");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn land_batch_refuses_an_upstream_off_its_hosts_or_a_host_with_no_token() {
    let server = MockServer::start().await;
    let tmp = tempfile::tempdir().unwrap();
    let with_token = serve_config(tmp.path(), &server, true);
    let (code, out) = land_batch_cli(&[
        "9",
        "--serve-config",
        &with_token,
        "--upstream",
        "https://github.com/o/r.git",
    ]);
    assert_ne!(code, Some(0), "{out}");
    assert!(out.contains("not on a [[forgejo]] host"), "{out}");

    let no_token = serve_config(tmp.path(), &server, false);
    let (code, out) = land_batch_cli(&[
        "9",
        "--serve-config",
        &no_token,
        "--upstream",
        "https://git.example/o/r.git",
    ]);
    assert_ne!(code, Some(0), "{out}");
    assert!(out.contains("no token_file"), "{out}");

    // --serve-config without --upstream is a usage error.
    let (code, out) = land_batch_cli(&["9", "--serve-config", &no_token]);
    assert_eq!(code, Some(64), "{out}");
    assert!(server.received_requests().await.unwrap().is_empty());
}
