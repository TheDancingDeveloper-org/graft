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

    // Vogt's own token: write under `src/` only.
    let vogt = site.mint_as("vogt", &["repo:r:write:src"], 3600);
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

    let short = site.mint_as("short", &["repo:r:admin"], 2);
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
