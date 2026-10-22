//! `Watch` streams are bounded: a capped wait, a lifetime of at most the
//! token's expiry and a configured maximum, and concurrency limits.

mod common;

use std::time::{Duration, Instant};

use common::{Site, api};
use graft_proto::ApiError;
use graft_proto::pb;

fn watch(wait_ms: u32) -> pb::WatchRequest {
    pb::WatchRequest {
        repo: "r".into(),
        from_lsn: 1,
        kinds: Vec::new(),
        limit: 0,
        wait_ms,
    }
}

/// Reads a stream to its end; returns how long it ran and how it ended.
async fn drain(mut s: tonic::Streaming<pb::JournalPage>) -> (Duration, Result<(), tonic::Status>) {
    let started = Instant::now();
    loop {
        match s.message().await {
            Ok(Some(_)) => {}
            Ok(None) => return (started.elapsed(), Ok(())),
            Err(e) => return (started.elapsed(), Err(e)),
        }
    }
}

async fn with_repo(site: &Site, server: &common::Server) {
    let admin = site.mint(&["repo:r:admin"]);
    server
        .clients(Some(&admin))
        .await
        .create_repo("r")
        .await
        .expect("create");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_wait_is_capped_whatever_the_client_asks() {
    let site = Site::new("server-watch-wait", "max_watch_wait_ms = 300");
    let server = site.start();
    with_repo(&site, &server).await;
    let token = site.mint(&["repo:r:read"]);
    let mut c = server.clients(Some(&token)).await;
    let s = c
        .repo
        .watch(watch(u32::MAX))
        .await
        .expect("watch")
        .into_inner();
    let (ran, end) = drain(s).await;
    end.expect("ends cleanly");
    assert!(ran < Duration::from_secs(3), "ran {ran:?}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_stream_ends_at_its_maximum_lifetime() {
    let site = Site::new("server-watch-life", "max_watch_lifetime_s = 1");
    let server = site.start();
    with_repo(&site, &server).await;
    let token = site.mint(&["repo:r:read"]);
    let mut c = server.clients(Some(&token)).await;
    let s = c
        .repo
        .watch(watch(60_000))
        .await
        .expect("watch")
        .into_inner();
    let (ran, end) = drain(s).await;
    end.expect("ends cleanly; the client resumes from next_lsn");
    assert!(ran >= Duration::from_millis(800), "ran {ran:?}");
    assert!(ran < Duration::from_secs(5), "ran {ran:?}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_stream_ends_when_its_token_expires() {
    let site = Site::new("server-watch-exp", "");
    let server = site.start();
    with_repo(&site, &server).await;
    let token = site.mint_as("brief", &["repo:r:read"], 2);
    let mut c = server.clients(Some(&token)).await;
    let s = c
        .repo
        .watch(watch(60_000))
        .await
        .expect("watch")
        .into_inner();
    let (ran, end) = drain(s).await;
    let e = end.expect_err("the token expired mid-stream");
    assert_eq!(api(e), ApiError::Unauthenticated);
    assert!(ran < Duration::from_secs(5), "ran {ran:?}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn concurrent_watches_are_limited_per_principal_and_in_total() {
    let site = Site::new(
        "server-watch-count",
        "max_watches_per_principal = 2\nmax_watches = 3",
    );
    let server = site.start();
    with_repo(&site, &server).await;
    let a = site.mint_as("a", &["repo:r:read"], 3600);
    let b = site.mint_as("b", &["repo:r:read"], 3600);
    let mut ca = server.clients(Some(&a)).await;
    let mut cb = server.clients(Some(&b)).await;

    let a1 = ca.repo.watch(watch(60_000)).await.expect("a1").into_inner();
    let _a2 = ca.repo.watch(watch(60_000)).await.expect("a2").into_inner();
    let e = ca
        .repo
        .watch(watch(60_000))
        .await
        .expect_err("a is at its limit");
    assert!(matches!(api(e), ApiError::Unavailable { retry_after_ms } if retry_after_ms > 0));

    let _b1 = cb.repo.watch(watch(60_000)).await.expect("b1").into_inner();
    let e = cb
        .repo
        .watch(watch(60_000))
        .await
        .expect_err("the total is reached");
    assert!(matches!(api(e), ApiError::Unavailable { .. }));

    // Closing a stream frees its slot.
    drop(a1);
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        match ca.repo.watch(watch(60_000)).await {
            Ok(_) => break,
            Err(e) => {
                assert!(matches!(api(e), ApiError::Unavailable { .. }));
                assert!(Instant::now() < deadline, "the slot was never freed");
                tokio::time::sleep(Duration::from_millis(100)).await;
            }
        }
    }
}
