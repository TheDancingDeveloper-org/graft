//! The service's GitHub App access (WI-3312) against a fake App API: a
//! repository outside the installation's list is refused, one token is shared
//! across tasks, and no key, JWT or token shows in `Debug` or error output.
//! The RSA key is generated per run into a temp directory.

use std::process::Command;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use graft_mirror::credentials::{GithubApp, InstallationAccess};
use graft_mirror::github::ForgeError;
use serde_json::json;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

fn now() -> i64 {
    1_000_000_000
}

fn key_file(dir: &std::path::Path) -> std::path::PathBuf {
    let path = dir.join("app.pem");
    let out = Command::new("openssl")
        .args(["genrsa", "-out"])
        .arg(&path)
        .arg("2048")
        .output()
        .expect("openssl runs");
    assert!(out.status.success());
    path
}

async fn fake_api(listed: &[&str], mints: Arc<AtomicUsize>) -> MockServer {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/app/installations/42/access_tokens"))
        .respond_with(move |_: &wiremock::Request| {
            mints.fetch_add(1, Ordering::SeqCst);
            ResponseTemplate::new(201).set_body_json(json!({"token": "ghs_SHARED_SECRET"}))
        })
        .mount(&server)
        .await;
    let repos: Vec<_> = listed.iter().map(|n| json!({"full_name": n})).collect();
    Mock::given(method("GET"))
        .and(path("/installation/repositories"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "total_count": repos.len(), "repositories": repos
        })))
        .mount(&server)
        .await;
    server
}

#[tokio::test]
async fn outside_the_list_is_refused_and_the_token_is_shared() {
    let dir = tempfile::tempdir().unwrap();
    let key = key_file(dir.path());
    let mints = Arc::new(AtomicUsize::new(0));
    let server = fake_api(&["org/in-one", "Org/In-Two"], mints.clone()).await;
    let app = GithubApp::load(&server.uri(), 7, 42, &key, now).unwrap();
    let access = Arc::new(InstallationAccess::new(app));

    // Several tasks, one minted token.
    let mut tasks = Vec::new();
    for repo in ["in-one", "in-two", "in-one"] {
        let a = access.clone();
        tasks.push(tokio::spawn(async move { a.token_for("org", repo).await }));
    }
    for t in tasks {
        assert_eq!(t.await.unwrap().unwrap().expose(), "ghs_SHARED_SECRET");
    }
    assert_eq!(mints.load(Ordering::SeqCst), 1, "one token for all tasks");

    // Outside the list: refused, with no token in the error.
    let err = access.token_for("org", "elsewhere").await.unwrap_err();
    assert!(matches!(err, ForgeError::Refused(_)), "{err:?}");
    let shown = format!("{err} {err:?}");
    assert!(!shown.contains("ghs_SHARED_SECRET"), "{shown}");
    assert!(!access.token_for("other", "in-one").await.is_ok());
}

#[tokio::test]
async fn the_key_never_shows_in_debug_or_errors() {
    let dir = tempfile::tempdir().unwrap();
    let key = key_file(dir.path());
    let pem = std::fs::read_to_string(&key).unwrap();
    let body: String = pem.lines().filter(|l| !l.starts_with("-----")).collect();
    let mints = Arc::new(AtomicUsize::new(0));
    let server = fake_api(&[], mints).await;
    let app = GithubApp::load(&server.uri(), 7, 42, &key, now).unwrap();
    let access = InstallationAccess::new(app);
    let mut seen = format!("{access:?}");
    let err = access.token_for("org", "x").await.unwrap_err();
    seen.push_str(&format!("{err} {err:?}"));
    assert!(!seen.contains(&body[..40]), "key material leaked");
    assert!(!seen.contains("PRIVATE KEY"));
    assert!(!seen.contains("ghs_SHARED_SECRET"));
    assert!(seen.contains("<redacted>"));

    // A bad key file's error carries no file contents either.
    let bad = dir.path().join("bad.pem");
    std::fs::write(&bad, "SECRETBYTES").unwrap();
    let e = GithubApp::load(&server.uri(), 7, 42, &bad, now).unwrap_err();
    assert!(!format!("{e} {e:?}").contains("SECRETBYTES"));
}
