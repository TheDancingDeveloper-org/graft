//! `AdminService.RevokeToken` through the real binary (`docs/design.md`
//! §12.1): the service that minted a token revokes it, an admin of the
//! token's repositories revokes it, everyone else is refused; revocation
//! takes the very next write, outlives a restart, and every revoke and
//! refusal is audited by principal and token id, never by token value.

mod common;

use std::fs;
use std::process::Stdio;

use common::{Clients, Site, api};
use graft_proto::{ApiError, pb};

fn mint_child(scopes: &[&str]) -> pb::MintTokenRequest {
    pb::MintTokenRequest {
        subject: "ses_child".into(),
        scopes: scopes.iter().map(|s| s.to_string()).collect(),
        ttl_s: 600,
        session: Some("ses_child".into()),
        work_item: Some("WI-1".into()),
    }
}

async fn revoke(c: &mut Clients, token: &str) -> Result<(), ApiError> {
    c.admin
        .revoke_token(pb::RevokeTokenRequest {
            token: token.into(),
        })
        .await
        .map(|_| ())
        .map_err(api)
}

/// The `jti` inside a `v4.public` token: its payload is base64url JSON.
fn jti_of(token: &str) -> String {
    let body = token.strip_prefix("v4.public.").expect("v4.public");
    let mut bits = 0u32;
    let mut have = 0;
    let mut bytes = Vec::new();
    for c in body.bytes() {
        let v = match c {
            b'A'..=b'Z' => c - b'A',
            b'a'..=b'z' => c - b'a' + 26,
            b'0'..=b'9' => c - b'0' + 52,
            b'-' => 62,
            b'_' => 63,
            _ => panic!("not base64url"),
        };
        bits = (bits << 6) | u32::from(v);
        have += 6;
        if have >= 8 {
            have -= 8;
            bytes.push((bits >> have) as u8);
            bits &= (1 << have) - 1;
        }
    }
    let text = String::from_utf8_lossy(&bytes).into_owned();
    let rest = text.split("\"jti\":\"").nth(1).expect("jti claim");
    rest.split('"').next().expect("jti value").to_owned()
}

async fn can_write(server: &common::Server, token: &str) -> Result<(), ApiError> {
    server
        .clients(Some(token))
        .await
        .put_blobs("r", &[b"x".to_vec()])
        .await
        .map(|_| ())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_minting_service_revokes_and_nobody_else_does() {
    let site = Site::new("server-revoke-rpc-minter", "");
    let admin = site.mint(&["repo:r:admin"]);
    let vogt = site.mint_kind("vogt", "service", &["repo:r:write"], 3600);
    let other_svc = site.mint_kind("other-svc", "service", &["repo:r:write"], 3600);
    let agent = site.mint_as("ses_agent", &["repo:r:write"], 3600);
    let log = site.dir.path().join("server.log");
    let server = site.start_with_stderr(Stdio::from(fs::File::create(&log).expect("log")));
    server
        .clients(Some(&admin))
        .await
        .create_repo("r")
        .await
        .expect("create");

    let mut v = server.clients(Some(&vogt)).await;
    let child = v
        .admin
        .mint_token(mint_child(&["repo:r:write"]))
        .await
        .expect("mint")
        .into_inner()
        .token;
    let sibling = v
        .admin
        .mint_token(mint_child(&["repo:r:write"]))
        .await
        .expect("mint")
        .into_inner()
        .token;
    can_write(&server, &child).await.expect("live before");

    // Another service, and the child itself, are refused; nothing changes.
    for token in [&other_svc, &agent, &child] {
        let mut c = server.clients(Some(token)).await;
        let e = revoke(&mut c, &child).await.expect_err("not the minter");
        assert!(matches!(e, ApiError::PermissionDenied { .. }), "{e:?}");
    }
    can_write(&server, &child).await.expect("still live");

    // The minter revokes: the child's very next write is refused, its
    // sibling is untouched, and revoking again is fine.
    revoke(&mut v, &child).await.expect("minter revokes");
    assert_eq!(
        can_write(&server, &child).await.expect_err("revoked"),
        ApiError::Unauthenticated
    );
    can_write(&server, &sibling).await.expect("sibling is live");
    revoke(&mut v, &child).await.expect("twice is fine");

    // A revoked token revokes nothing.
    let mut c = server.clients(Some(&child)).await;
    assert_eq!(
        revoke(&mut c, &sibling).await.expect_err("revoked caller"),
        ApiError::Unauthenticated
    );

    // Still revoked after a restart.
    server.stop();
    let server = site.start();
    assert_eq!(
        can_write(&server, &child).await.expect_err("revoked"),
        ApiError::Unauthenticated
    );
    can_write(&server, &sibling).await.expect("sibling is live");
    let mut v = server.clients(Some(&vogt)).await;
    revoke(&mut v, &sibling)
        .await
        .expect("the record of who minted it is durable");
    assert_eq!(
        can_write(&server, &sibling).await.expect_err("revoked"),
        ApiError::Unauthenticated
    );
    drop(server);

    // The audit: who, which token and what happened, never a token.
    let text = fs::read_to_string(&log).expect("log");
    let audit: Vec<&str> = text
        .lines()
        .filter(|l| l.contains("token revocation"))
        .collect();
    let (child_id, sibling_id) = (jti_of(&child), jti_of(&sibling));
    let line = |who: &str, jti: &str, outcome: &str| {
        audit
            .iter()
            .filter(|l| {
                l.contains(&format!("principal={who}"))
                    && l.contains(&format!("token_jti={jti}"))
                    && l.contains(outcome)
            })
            .count()
    };
    assert_eq!(line("vogt", &child_id, "revoked"), 2, "{audit:#?}");
    assert!(line("vogt", &child_id, "basis=minter") >= 1, "{audit:#?}");
    for who in ["other-svc", "ses_agent", "ses_child"] {
        assert_eq!(
            line(who, &child_id, "permission_denied"),
            1,
            "{who}: {audit:#?}"
        );
    }
    assert_eq!(line("ses_child", &sibling_id, "unauthenticated"), 0);
    assert!(
        audit
            .iter()
            .any(|l| l.contains("principal=ses_child") && l.contains("unauthenticated")),
        "{audit:#?}"
    );
    for token in [&admin, &vogt, &other_svc, &agent, &child, &sibling] {
        assert!(!text.contains(token.as_str()), "a token reached the log");
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_admin_revokes_any_token_on_the_repositories_it_administers() {
    let site = Site::new("server-revoke-rpc-admin", "");
    let boot = site.mint(&["repo:r:admin", "repo:s:admin"]);
    let r_admin = site.mint_as("r-admin", &["repo:r:admin"], 3600);
    // Minted on the host: no service minted it.
    let host_minted = site.mint_as("ses_host", &["repo:r:write"], 3600);
    let two_repos = site.mint_as("ses_two", &["repo:r:write", "repo:s:write"], 3600);
    let objects = site.mint_as("node", &["objects:read"], 3600);
    let server = site.start();
    let mut b = server.clients(Some(&boot)).await;
    b.create_repo("r").await.expect("create r");
    b.create_repo("s").await.expect("create s");

    // An admin of `r` revokes a token on `r` that no service minted ...
    let mut a = server.clients(Some(&r_admin)).await;
    revoke(&mut a, &host_minted).await.expect("admin revokes");
    assert_eq!(
        can_write(&server, &host_minted).await.expect_err("revoked"),
        ApiError::Unauthenticated
    );
    // ... but not one that also names a repository it does not administer,
    // nor one that names no repository at all.
    for token in [&two_repos, &objects] {
        let e = revoke(&mut a, token).await.expect_err("not its admin");
        assert!(matches!(e, ApiError::PermissionDenied { .. }), "{e:?}");
    }
    can_write(&server, &two_repos).await.expect("still live");
    // An admin of both does.
    revoke(&mut b, &two_repos).await.expect("admin of both");
    assert_eq!(
        can_write(&server, &two_repos).await.expect_err("revoked"),
        ApiError::Unauthenticated
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn only_a_live_token_of_this_server_can_be_revoked() {
    let site = Site::new("server-revoke-rpc-bad", "");
    let admin = site.mint(&["repo:r:admin"]);
    let short = site.mint_as("ses_short", &["repo:r:write"], 1);
    let foreign = Site::new("server-revoke-rpc-foreign", "").mint(&["repo:r:write"]);
    let server = site.start();
    let mut a = server.clients(Some(&admin)).await;
    a.create_repo("r").await.expect("create");

    for token in ["", "v4.public.garbage", &foreign] {
        let e = revoke(&mut a, token).await.expect_err("not revocable");
        assert!(matches!(e, ApiError::InvalidChange { .. }), "{e:?}");
        assert!(!format!("{e:?}").contains(token) || token.is_empty());
    }
    // An unauthenticated or unauthorised caller learns nothing about it.
    let mut anon = server.clients(None).await;
    assert_eq!(
        revoke(&mut anon, &short).await.expect_err("no token"),
        ApiError::Unauthenticated
    );
}
