//! The mirror tools on `/mcp` (D41 amendment, WI-4387): the server's own
//! mount runs the mirror service in process, an oversight-kind token binds a
//! mirror (with a start point) and sets its credential, a worker is refused,
//! and no result carries the credential.

mod common;
#[path = "common/mcp_agent.rs"]
mod mcp_agent;

use common::Site;
use graft_proto::ApiError;
use graft_proto::context::PrincipalKind;
use graft_proto::pb;
use mcp_agent::Agent;
use serde_json::json;

const UPSTREAM: &str = "https://github.com/o/r.git";
const START: &str = "0123456789abcdef0123456789abcdef01234567";

fn add_args(start_at: Option<&str>) -> serde_json::Value {
    let mut args = json!({
        "repo": "r",
        "upstream": UPSTREAM,
        "lines": [{"branch": "main", "line": "main", "direction": "follow"}],
    });
    if let Some(start) = start_at {
        args["start_at"] = json!(start);
    }
    args
}

#[tokio::test]
async fn an_oversight_token_binds_a_mirror_and_sets_its_credential_over_mcp() {
    let site = Site::new("mcp-mirror-bind", "");
    let kek = site.dir.path().join("kek-k1");
    std::fs::write(&kek, [0x11; 32]).expect("kek");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        std::fs::set_permissions(&kek, std::fs::Permissions::from_mode(0o600)).expect("chmod");
    }
    let text = std::fs::read_to_string(&site.config).expect("config");
    std::fs::write(
        &site.config,
        format!(
            "{text}[mirror]\ncredential_kek = {{ id = \"k1\", file = \"{}\" }}\n",
            kek.display()
        ),
    )
    .expect("config");
    let server = site.start();
    let owner = site.mint(&["repos:admin", "repo:*:admin"]);
    server
        .clients(Some(&owner))
        .await
        .create_repo("r")
        .await
        .expect("repo");

    // The kind Vogt mints for oversight sessions, with the scope D41 gives it.
    let oversight = site.mint_kind("ov", "oversight", &["mirror:bind", "repo:r:read"], 3600);
    let agent = Agent::open(server.addr, &oversight);

    // The tools exist on the server's own mount, and `start_at` crosses it.
    let added = agent.ok("graft_mirror_add", add_args(Some(START)));
    assert_eq!(added["mirror"]["start_at"], START);
    assert_eq!(added["mirror"]["credential_set"], false);

    // A credential is taken as a file path; its value never comes back.
    let secret = "ghp_thisMustNotAppearInAnyResult1234567890";
    let path = site.dir.path().join("upstream-credential");
    std::fs::write(&path, secret).expect("credential file");
    let out = agent.ok(
        "graft_mirror_credential",
        json!({
            "repo": "r",
            "upstream": UPSTREAM,
            "credential_file": path.display().to_string(),
        }),
    );
    assert_eq!(out["mirror"]["credential_set"], true);
    assert!(
        !out.to_string().contains(secret),
        "the credential leaked into a tool result"
    );

    // Closing the file surfaces no contents either.
    std::fs::remove_file(&path).expect("remove credential file");
    let err = agent.err(
        "graft_mirror_credential",
        json!({
            "repo": "r",
            "upstream": UPSTREAM,
            "credential_file": path.display().to_string(),
        }),
    );
    assert_eq!(err["kind"], "invalid_argument");
    assert!(!err.to_string().contains(secret));
}

#[tokio::test]
async fn a_worker_token_is_refused_mirror_bind_over_mcp() {
    let site = Site::new("mcp-mirror-worker", "");
    let server = site.start();
    let owner = site.mint(&["repos:admin", "repo:*:admin"]);
    server
        .clients(Some(&owner))
        .await
        .create_repo("r")
        .await
        .expect("repo");

    let worker = site.mint_as("wk", &["repo:r:read", "repo:r:write"], 3600);
    let agent = Agent::open(server.addr, &worker);
    let err = agent.err("graft_mirror_add", add_args(None));
    assert_eq!(err["kind"], "permission_denied");
    assert_eq!(err["scope"], "mirror:bind");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn only_an_oversight_token_mints_an_oversight_token() {
    let site = Site::new("mint-oversight", "");
    let server = site.start();
    let admin = site.mint(&["repos:admin"]);
    server
        .clients(Some(&admin))
        .await
        .create_repo("r")
        .await
        .expect("repo");

    // A worker cannot ask for the oversight kind, even for itself.
    let worker = site.mint_as("wk", &["repo:r:read"], 3600);
    let mut c = server.clients(Some(&worker)).await;
    let e = c
        .admin
        .mint_token(pb::MintTokenRequest {
            subject: String::new(),
            scopes: vec!["repo:r:read".into()],
            ttl_s: 60,
            session: None,
            work_item: None,
            kind: Some("oversight".into()),
        })
        .await
        .expect_err("a worker cannot mint oversight");
    assert!(matches!(common::api(e), ApiError::PermissionDenied { .. }));

    // An oversight token mints one; its kind survives the delegation.
    let oversight = site.mint_kind("ov", "oversight", &["mirror:bind"], 3600);
    let mut c = server.clients(Some(&oversight)).await;
    let minted = c
        .admin
        .mint_token(pb::MintTokenRequest {
            subject: String::new(),
            scopes: vec!["mirror:bind".into()],
            ttl_s: 60,
            session: None,
            work_item: None,
            kind: Some("oversight".into()),
        })
        .await
        .expect("oversight mints oversight")
        .into_inner()
        .token;
    let claims = graft_auth::Claims::peek_unverified(&graft_auth::Token::from_encoded(minted))
        .expect("claims");
    assert_eq!(claims.principal.kind, PrincipalKind::Oversight);

    // Delegating a worker token that carries mirror:bind is refused: the kind
    // rule in `mint` still refuses an agent the operator-only scope.
    let e = c
        .admin
        .mint_token(pb::MintTokenRequest {
            subject: "worker".into(),
            scopes: vec!["mirror:bind".into()],
            ttl_s: 60,
            session: Some("ses_worker".into()),
            work_item: None,
            kind: None,
        })
        .await
        .expect_err("an agent may not carry mirror:bind");
    assert!(
        matches!(
            common::api(e),
            ApiError::InvalidChange { ref rule, .. } if rule == "scope-kind"
        ),
        "a delegated agent never holds mirror:bind"
    );
}
