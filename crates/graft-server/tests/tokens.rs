//! Token escalation and revocation through the real binary (`docs/design.md`
//! §12.1, §12.2): a minted token is never broader than its caller, never
//! outlives it, and a revoked `jti` is refused on writes, across restarts.

mod common;

use std::time::Duration;

use common::{Site, api};
use graft_proto::ApiError;
use graft_proto::pb;

fn mint_req(scopes: &[&str], ttl_s: u64) -> pb::MintTokenRequest {
    pb::MintTokenRequest {
        subject: "ses_child".into(),
        scopes: scopes.iter().map(|s| s.to_string()).collect(),
        ttl_s,
        session: Some("ses_child".into()),
        work_item: Some("WI-2198".into()),
    }
}

fn main_ref() -> pb::ResolveRefRequest {
    pb::ResolveRefRequest {
        repo: "r".into(),
        name: "main".into(),
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_minted_token_is_no_broader_than_its_caller() {
    let site = Site::new("server-mint-widen", "");
    let admin = site.mint(&["repo:r:admin"]);
    let server = site.start();
    server
        .clients(Some(&admin))
        .await
        .create_repo("r")
        .await
        .expect("create");

    // The service principal's own token: a service, writing under `src/` only.
    let vogt = site.mint_kind("vogt", "service", &["repo:r:write:src"], 3600);
    let mut c = server.clients(Some(&vogt)).await;

    // Narrower than the caller: minted, and usable.
    let narrow = c
        .admin
        .mint_token(mint_req(&["repo:r:write:src/lib"], 60))
        .await
        .expect("a narrower token mints")
        .into_inner()
        .token;
    let mut n = server.clients(Some(&narrow)).await;
    let e = n
        .repo
        .resolve_ref(main_ref())
        .await
        .expect_err("no ref yet");
    assert!(matches!(api(e), ApiError::NotFound { .. }));

    // Broader in any direction: refused.
    for scopes in [
        &["repo:r:write"][..],
        &["repo:r:write:docs"][..],
        &["repo:r:admin"][..],
        &["repo:r:land:main"][..],
        &["repo:other:read"][..],
    ] {
        let e = c
            .admin
            .mint_token(mint_req(scopes, 60))
            .await
            .expect_err("broader than the caller");
        assert!(
            matches!(api(e), ApiError::PermissionDenied { .. }),
            "scopes {scopes:?}"
        );
    }

    // A longer TTL than the caller's own: refused, naming the limit.
    let e = c
        .admin
        .mint_token(mint_req(&["repo:r:read"], 7200))
        .await
        .expect_err("longer than the server maximum");
    assert!(matches!(api(e), ApiError::LimitExceeded { .. }));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_minted_token_never_outlives_its_caller() {
    let site = Site::new("server-mint-life", "");
    let admin = site.mint(&["repo:r:admin"]);
    let server = site.start();
    server
        .clients(Some(&admin))
        .await
        .create_repo("r")
        .await
        .expect("create");

    let short = site.mint_kind("short", "service", &["repo:r:admin"], 2);
    let mut c = server.clients(Some(&short)).await;
    let e = c
        .admin
        .mint_token(mint_req(&["repo:r:read"], 3600))
        .await
        .expect_err("outlives the caller");
    assert!(matches!(api(e), ApiError::LimitExceeded { .. }));

    // The default TTL is capped at what the caller has left.
    let child = c
        .admin
        .mint_token(mint_req(&["repo:r:read"], 0))
        .await
        .expect("capped default")
        .into_inner()
        .token;
    let mut k = server.clients(Some(&child)).await;
    let e = k
        .repo
        .resolve_ref(main_ref())
        .await
        .expect_err("no ref yet");
    assert!(matches!(api(e), ApiError::NotFound { .. }));

    // Both die together.
    tokio::time::sleep(Duration::from_millis(2_600)).await;
    let e = k.repo.resolve_ref(main_ref()).await.expect_err("expired");
    assert_eq!(api(e), ApiError::Unauthenticated);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_revoked_token_is_refused_on_writes_and_stays_revoked_across_restarts() {
    let site = Site::new("server-revoke", "");
    let admin = site.mint(&["repo:r:admin"]);
    let target = site.mint_as("ses_target", &["repo:r:write"], 3600);
    let other = site.mint_as("ses_other", &["repo:r:write"], 3600);
    let server = site.start();
    server
        .clients(Some(&admin))
        .await
        .create_repo("r")
        .await
        .expect("create");

    let mut t = server.clients(Some(&target)).await;
    t.put_blobs("r", &[b"before".to_vec()])
        .await
        .expect("writes before revocation");

    let out = site.admin_stdin(&["token", "revoke"], &target);
    assert!(
        out.status.success(),
        "revoke: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(String::from_utf8_lossy(&out.stdout).starts_with("revoked "));

    // Refused on the next write: the denylist is read per write.
    let e = t
        .put_blobs("r", &[b"after".to_vec()])
        .await
        .expect_err("revoked");
    assert_eq!(e, ApiError::Unauthenticated);
    // It mints nothing either, even within its own scopes.
    let e = t
        .admin
        .mint_token(mint_req(&["repo:r:write"], 60))
        .await
        .expect_err("revoked");
    assert_eq!(api(e), ApiError::Unauthenticated);
    // Reads are not on the denylist's path.
    let e = t
        .repo
        .resolve_ref(main_ref())
        .await
        .expect_err("no ref yet");
    assert!(matches!(api(e), ApiError::NotFound { .. }));

    // Another token is unaffected.
    server
        .clients(Some(&other))
        .await
        .put_blobs("r", &[b"other".to_vec()])
        .await
        .expect("other token still writes");

    // Still revoked after a restart: the denylist is durable.
    server.stop();
    let server = site.start();
    let mut t = server.clients(Some(&target)).await;
    let e = t
        .put_blobs("r", &[b"later".to_vec()])
        .await
        .expect_err("revoked after restart");
    assert_eq!(e, ApiError::Unauthenticated);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn revoking_takes_a_live_token_of_this_server_only() {
    let site = Site::new("server-revoke-bad", "");
    let admin = site.mint(&["repo:r:admin"]);
    let other = site.mint_as("ses_other", &["repo:r:write"], 3600);
    let server = site.start();
    server
        .clients(Some(&admin))
        .await
        .create_repo("r")
        .await
        .expect("create");

    let garbage = site.admin_stdin(&["token", "revoke"], "v4.public.garbage\n");
    assert_eq!(garbage.status.code(), Some(1));
    let foreign = Site::new("server-revoke-foreign", "").mint(&["repo:r:admin"]);
    let signed_elsewhere = site.admin_stdin(&["token", "revoke"], &foreign);
    assert_eq!(signed_elsewhere.status.code(), Some(1));
    let stderr = String::from_utf8_lossy(&signed_elsewhere.stderr);
    assert!(!stderr.contains(&foreign), "errors never echo a token");

    server
        .clients(Some(&other))
        .await
        .put_blobs("r", &[b"still good".to_vec()])
        .await
        .expect("refused revocations change nothing");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn every_scope_form_is_held_exactly_or_refused() {
    let site = Site::new("server-mint-forms", "");
    let admin = site.mint(&["repo:r:admin"]);
    let server = site.start();
    server
        .clients(Some(&admin))
        .await
        .create_repo("r")
        .await
        .expect("create");
    let vogt = site.mint_kind("vogt", "service", &["repo:r:write:src"], 3600);
    let mut c = server.clients(Some(&vogt)).await;

    // No wildcards, no case folding, no trimming, no path tricks: each of
    // these is either not a scope or not one the caller holds.
    for scope in [
        "repo:*:read",
        "repo:*:write",
        "repo:r:*",
        "repo:r:write:*",
        "*",
        "repo:R:read",
        "REPO:r:read",
        "repo:r:READ",
        "repo:r:write:SRC",
        " repo:r:read",
        "repo:r:read ",
        "repo: r:read",
        "repo:r:write: src",
        "repo:r:write:src ",
        "repo:r:read\n",
        "repo:r:write:src/../docs",
        "repo:r:write:./src",
        "repo:r:write:src/",
        "repo:r:write:/src",
        "repo:r:write:srcx",
        "repo:r:write:sr",
        "repo:r:write:",
        "repo:r:read:extra",
        "repo:r:land:*",
        "objects:read",
        "objects:*",
        "",
    ] {
        let e = c
            .admin
            .mint_token(mint_req(&[scope], 60))
            .await
            .expect_err("not held");
        assert!(
            matches!(
                api(e),
                ApiError::PermissionDenied { .. } | ApiError::InvalidChange { .. }
            ),
            "scope {scope:?}"
        );
    }
    // One bad scope among good ones spoils the request.
    let e = c
        .admin
        .mint_token(mint_req(&["repo:r:read", "repo:r:write"], 60))
        .await
        .expect_err("one broader scope");
    assert!(matches!(api(e), ApiError::PermissionDenied { .. }));
    // A token with no scopes is refused rather than minted empty.
    let e = c
        .admin
        .mint_token(mint_req(&[], 60))
        .await
        .expect_err("no scopes");
    assert!(matches!(api(e), ApiError::InvalidChange { .. }));
    // The exact forms the caller holds do mint.
    for scope in ["repo:r:read", "repo:r:write:src", "repo:r:write:src/a/b"] {
        c.admin
            .mint_token(mint_req(&[scope], 60))
            .await
            .unwrap_or_else(|e| panic!("{scope}: {e:?}"));
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_token_minted_from_a_minted_token_is_no_broader_and_no_longer() {
    let site = Site::new("server-mint-chain", "");
    let admin = site.mint(&["repo:r:admin"]);
    let server = site.start();
    server
        .clients(Some(&admin))
        .await
        .create_repo("r")
        .await
        .expect("create");
    let vogt = site.mint_kind("vogt", "service", &["repo:r:write"], 3600);
    let mut v = server.clients(Some(&vogt)).await;

    // The orchestrator delegates a short-lived session token.
    let session = v
        .admin
        .mint_token(mint_req(&["repo:r:write:src"], 3))
        .await
        .expect("delegated")
        .into_inner()
        .token;
    let mut s = server.clients(Some(&session)).await;

    // The session token cannot widen, outlive itself, or name anyone else.
    for scopes in [
        &["repo:r:write"][..],
        &["repo:r:write:docs"],
        &["repo:r:admin"],
    ] {
        let e = s
            .admin
            .mint_token(self_req(scopes, 0))
            .await
            .expect_err("broader");
        assert!(
            matches!(api(e), ApiError::PermissionDenied { .. }),
            "{scopes:?}"
        );
    }
    let e = s
        .admin
        .mint_token(self_req(&["repo:r:read"], 60))
        .await
        .expect_err("outlives its caller");
    assert!(matches!(api(e), ApiError::LimitExceeded { .. }));
    for req in [
        pb::MintTokenRequest {
            subject: "ses_other".into(),
            ..self_req(&["repo:r:read"], 0)
        },
        pb::MintTokenRequest {
            session: Some("ses_other".into()),
            ..self_req(&["repo:r:read"], 0)
        },
        pb::MintTokenRequest {
            work_item: Some("WI-other".into()),
            ..self_req(&["repo:r:read"], 0)
        },
    ] {
        let e = s
            .admin
            .mint_token(req.clone())
            .await
            .expect_err("another identity");
        assert!(
            matches!(api(e), ApiError::PermissionDenied { .. }),
            "{req:?}"
        );
    }

    // Narrower, for itself: minted, with the default life capped at what is
    // left, and so it dies with its caller.
    let grandchild = s
        .admin
        .mint_token(self_req(&["repo:r:read"], 0))
        .await
        .expect("narrower, for itself")
        .into_inner()
        .token;
    let mut g = server.clients(Some(&grandchild)).await;
    let e = g
        .repo
        .resolve_ref(main_ref())
        .await
        .expect_err("no ref yet");
    assert!(matches!(api(e), ApiError::NotFound { .. }));
    tokio::time::sleep(Duration::from_millis(3_600)).await;
    let e = g.repo.resolve_ref(main_ref()).await.expect_err("expired");
    assert_eq!(api(e), ApiError::Unauthenticated);
}

/// A request that names no identity: the token is the caller's own.
fn self_req(scopes: &[&str], ttl_s: u64) -> pb::MintTokenRequest {
    pb::MintTokenRequest {
        subject: String::new(),
        session: None,
        work_item: None,
        ..mint_req(scopes, ttl_s)
    }
}
