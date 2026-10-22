//! The GitHub App credential provider against a fake API (wiremock): it mints
//! an installation token from a signed JWT, reuses it until shortly before it
//! expires, and maps failures. The RSA key is generated into a temp directory
//! for each run; no private key is kept in the repository.
//!
//! The provider takes a plain `fn` clock, so the scenarios share one static
//! clock and run one after another in a single test.

use std::process::Command;
use std::sync::Arc;
use std::sync::atomic::{AtomicI64, AtomicUsize, Ordering};

use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use graft_mirror::MirrorError;
use graft_mirror::credentials::{CredentialProvider, GithubApp, SecretToken, StaticToken};
use graft_mirror::github::ForgeError;
use serde_json::{Value, json};
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, Request, ResponseTemplate};

const INSTALL_PATH: &str = "/app/installations/42/access_tokens";

/// The provider's clock, in ms. The scenarios move it by hand.
static NOW_MS: AtomicI64 = AtomicI64::new(1_000_000_000);

fn now() -> i64 {
    NOW_MS.load(Ordering::SeqCst)
}

fn advance_min(minutes: i64) {
    NOW_MS.fetch_add(minutes * 60_000, Ordering::SeqCst);
}

/// A throwaway 2048-bit RSA key, PEM, in `dir`.
fn key_file(dir: &std::path::Path) -> std::path::PathBuf {
    let path = dir.join("app.pem");
    let out = Command::new("openssl")
        .args(["genrsa", "-out"])
        .arg(&path)
        .arg("2048")
        .output()
        .expect("openssl runs");
    assert!(out.status.success(), "openssl genrsa failed");
    path
}

/// Answers each mint with `ghs_<n>`, counting from one.
fn minting(counter: Arc<AtomicUsize>) -> impl Fn(&Request) -> ResponseTemplate {
    move |_: &Request| {
        let n = counter.fetch_add(1, Ordering::SeqCst) + 1;
        ResponseTemplate::new(201).set_body_json(json!({
            "token": format!("ghs_{n}"),
            "expires_at": "2026-10-22T13:00:00Z",
        }))
    }
}

async fn mint_calls(server: &MockServer) -> Vec<Request> {
    server
        .received_requests()
        .await
        .unwrap()
        .into_iter()
        .filter(|r| r.url.path() == INSTALL_PATH)
        .collect()
}

#[tokio::test(flavor = "current_thread")]
async fn github_app_provider_scenarios() {
    let dir = tempfile::tempdir().unwrap();
    let key = key_file(dir.path());

    // 1. Mints once, reuses the cache, and refreshes under ten minutes left.
    NOW_MS.store(1_000_000_000, Ordering::SeqCst);
    let server = MockServer::start().await;
    let mints = Arc::new(AtomicUsize::new(0));
    Mock::given(method("POST"))
        .and(path(INSTALL_PATH))
        .respond_with(minting(mints.clone()))
        .mount(&server)
        .await;
    let app = GithubApp::load(&server.uri(), 123456, 42, &key, now).unwrap();
    assert_eq!(app.token().await.unwrap().expose(), "ghs_1");
    assert_eq!(app.token().await.unwrap().expose(), "ghs_1");
    assert_eq!(mint_calls(&server).await.len(), 1, "cached, not re-minted");
    advance_min(40);
    assert_eq!(
        app.token().await.unwrap().expose(),
        "ghs_1",
        "twenty minutes left: fresh"
    );
    assert_eq!(mint_calls(&server).await.len(), 1);
    advance_min(11);
    assert_eq!(
        app.token().await.unwrap().expose(),
        "ghs_2",
        "under ten minutes: refreshed"
    );
    assert_eq!(mint_calls(&server).await.len(), 2);

    // 2. The JWT names the App and lives at most ten minutes.
    NOW_MS.store(1_000_000_000, Ordering::SeqCst);
    let server = MockServer::start().await;
    let mints = Arc::new(AtomicUsize::new(0));
    Mock::given(method("POST"))
        .and(path(INSTALL_PATH))
        .respond_with(minting(mints.clone()))
        .mount(&server)
        .await;
    let app = GithubApp::load(&server.uri(), 123456, 42, &key, now).unwrap();
    app.token().await.unwrap();
    let calls = mint_calls(&server).await;
    let auth = calls[0]
        .headers
        .get("authorization")
        .unwrap()
        .to_str()
        .unwrap();
    let jwt = auth.strip_prefix("Bearer ").expect("a bearer JWT");
    let parts: Vec<&str> = jwt.split('.').collect();
    assert_eq!(parts.len(), 3, "header.payload.signature");
    let header: Value = serde_json::from_slice(&URL_SAFE_NO_PAD.decode(parts[0]).unwrap()).unwrap();
    assert_eq!(header["alg"], "RS256");
    let claims: Value = serde_json::from_slice(&URL_SAFE_NO_PAD.decode(parts[1]).unwrap()).unwrap();
    assert_eq!(claims["iss"], "123456", "issued by the App id");
    let iat = claims["iat"].as_i64().unwrap();
    let exp = claims["exp"].as_i64().unwrap();
    let now_s = now() / 1000;
    assert!(iat <= now_s, "not issued in the future");
    assert!(exp - now_s <= 600, "GitHub allows at most ten minutes");
    assert!(exp > now_s, "still valid when sent");

    // 3. A failing mint is transient; the next use mints again.
    NOW_MS.store(1_000_000_000, Ordering::SeqCst);
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path(INSTALL_PATH))
        .respond_with(ResponseTemplate::new(500))
        .up_to_n_times(1)
        .with_priority(1)
        .mount(&server)
        .await;
    let mints = Arc::new(AtomicUsize::new(0));
    Mock::given(method("POST"))
        .and(path(INSTALL_PATH))
        .respond_with(minting(mints.clone()))
        .mount(&server)
        .await;
    let app = GithubApp::load(&server.uri(), 123456, 42, &key, now).unwrap();
    let err = app.token().await.unwrap_err();
    assert!(matches!(err, ForgeError::Transient(_)), "{err}");
    assert_eq!(
        app.token().await.unwrap().expose(),
        "ghs_1",
        "the next use mints"
    );

    // 4. A refused mint is not retried by the provider itself.
    NOW_MS.store(1_000_000_000, Ordering::SeqCst);
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path(INSTALL_PATH))
        .respond_with(ResponseTemplate::new(404).set_body_json(json!({"message": "Not Found"})))
        .mount(&server)
        .await;
    let app = GithubApp::load(&server.uri(), 123456, 42, &key, now).unwrap();
    let err = app.token().await.unwrap_err();
    assert!(matches!(err, ForgeError::Refused(_)), "{err}");
    assert_eq!(mint_calls(&server).await.len(), 1);
}

#[test]
fn a_bad_key_is_a_config_error_at_load() {
    let dir = tempfile::tempdir().unwrap();
    let not_a_key = dir.path().join("k.pem");
    std::fs::write(&not_a_key, "not a key").unwrap();
    let err = GithubApp::load("https://api.github.com", 1, 2, &not_a_key, now).unwrap_err();
    assert!(matches!(err, MirrorError::Config(_)), "{err}");
    let missing = GithubApp::load(
        "https://api.github.com",
        1,
        2,
        &dir.path().join("none"),
        now,
    )
    .unwrap_err();
    assert!(matches!(missing, MirrorError::Config(_)), "{missing}");
}

#[tokio::test(flavor = "current_thread")]
async fn a_static_token_is_used_as_is_and_never_prints() {
    let token = SecretToken::new("fake-static".to_owned()).unwrap();
    let p = StaticToken::new(token);
    let got = p.token().await.unwrap();
    assert_eq!(got.expose(), "fake-static");
    assert!(!format!("{got:?}").contains("fake-static"));
}
