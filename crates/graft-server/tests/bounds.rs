//! Bounds on the object API through the binary: an oversized `PutObjects`,
//! a `PutObjects` refused before it is buffered, a huge page request, and a
//! slow `GetObjects` client held back by backpressure, and the shared
//! `PutObjects` budget: calls that wait for it, and an idle writer that
//! gives it back.

mod common;

use std::collections::BTreeMap;
use std::time::{Duration, Instant};

use common::{Site, api, noise};
use graft_model::{ObjectId, ObjectKind};
use graft_proto::types::{Objects, PutObjects};
use graft_proto::{ApiError, Frame, pb};
use tokio::sync::mpsc;
use tonic::codegen::tokio_stream::wrappers::ReceiverStream;

fn put_message(repo: &str, blobs: &[Vec<u8>]) -> pb::PutObjectsRequest {
    PutObjects {
        repo: repo.into(),
        blobs: blobs
            .iter()
            .map(|b| Frame::encode(ObjectKind::Blob, b))
            .collect(),
    }
    .into()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_oversized_put_is_refused_and_its_budget_returned() {
    let site = Site::new(
        "server-put-cap",
        "max_put_call_bytes = 1048576\nput_buffer_bytes = 1048576",
    );
    let token = site.mint(&["repo:r:admin"]);
    let server = site.start();
    let mut c = server.clients(Some(&token)).await;
    c.create_repo("r").await.expect("create");

    // Two messages of 600 KiB: the second passes the per-call cap.
    let messages = vec![
        put_message("r", &[noise(1, 600 << 10)]),
        put_message("r", &[noise(2, 600 << 10)]),
    ];
    let e = c
        .objects
        .put_objects(tonic::codegen::tokio_stream::iter(messages))
        .await
        .expect_err("over the cap");
    assert_eq!(
        api(e),
        ApiError::LimitExceeded {
            limit: "put-bytes".into(),
            max: 1 << 20
        }
    );

    // Too many blobs in one call.
    let tiny: Vec<Vec<u8>> = (0..1025u32).map(|i| i.to_le_bytes().to_vec()).collect();
    let e = c
        .objects
        .put_objects(tonic::codegen::tokio_stream::iter(vec![put_message(
            "r", &tiny,
        )]))
        .await
        .expect_err("too many ids");
    assert!(matches!(api(e), ApiError::LimitExceeded { limit, .. } if limit == "ids"));

    // The budget came back: a call that fits still goes through, twice
    // (the whole server budget is one call's cap here).
    for seed in [3, 4] {
        c.put_blobs("r", &[noise(seed, 900 << 10)])
            .await
            .expect("a put within the cap");
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_put_is_refused_on_its_first_message_before_buffering() {
    let site = Site::new("server-put-early", "");
    let admin = site.mint(&["repo:r:admin"]);
    let reader = site.mint(&["repo:r:read"]);
    let prefixed = site.mint(&["repo:r:write:src"]);
    let elsewhere = site.mint(&["repo:nope:admin"]);
    let server = site.start();
    server
        .clients(Some(&admin))
        .await
        .create_repo("r")
        .await
        .expect("create");

    for (token, repo, want) in [
        (&reader, "r", "permission_denied"),
        (&prefixed, "r", "permission_denied"),
        (&admin, "nope", "permission_denied"),
        (&elsewhere, "nope", "not_found"),
    ] {
        let mut c = server.clients(Some(token)).await;
        // One message, then a stream that stays open: the answer must not
        // wait for the client to finish sending.
        let (tx, rx) = mpsc::channel(1);
        tx.send(put_message(repo, &[b"x".to_vec()]))
            .await
            .expect("queue");
        let started = Instant::now();
        let e = tokio::time::timeout(
            Duration::from_secs(5),
            c.objects.put_objects(ReceiverStream::new(rx)),
        )
        .await
        .expect("refused without waiting for the stream to end")
        .expect_err("refused");
        let got = match api(e) {
            ApiError::PermissionDenied { .. } => "permission_denied",
            ApiError::NotFound { .. } => "not_found",
            other => panic!("unexpected {other:?}"),
        };
        assert_eq!(got, want, "{repo}");
        assert!(started.elapsed() < Duration::from_secs(5));
        drop(tx);
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn huge_page_requests_are_clamped() {
    let site = Site::new("server-pages", "");
    let token = site.mint(&["repo:r:admin"]);
    let server = site.start();
    let mut c = server.clients(Some(&token)).await;
    c.create_repo("r").await.expect("create");

    // GetObjects: 20 stored, 4,000 unknown, some repeated. At most 1,024
    // distinct ids are looked at; the rest come back deferred.
    let stored: Vec<Vec<u8>> = (0..20).map(|i| noise(i, 100)).collect();
    c.put_blobs("r", &stored).await.expect("put");
    let mut ids: Vec<ObjectId> = stored
        .iter()
        .map(|b| ObjectId::hash(ObjectKind::Blob, b))
        .collect();
    ids.extend((0..4000u32).map(|i| {
        let mut b = [0u8; 32];
        b[..4].copy_from_slice(&i.to_le_bytes());
        ObjectId(b)
    }));
    let repeated = ids[..100].to_vec();
    ids.extend(repeated);
    let mut stream = c
        .objects
        .get_objects(pb::GetObjectsRequest {
            repo: "r".into(),
            ids: ids.iter().map(|i| i.0.to_vec()).collect(),
        })
        .await
        .expect("get")
        .into_inner();
    let (mut found, mut missing, mut deferred) = (0, 0, 0);
    while let Some(page) = stream.message().await.expect("page") {
        let page = Objects::try_from(page).expect("valid");
        found += page.found.len();
        missing += page.missing.len();
        deferred += page.deferred.len();
    }
    assert_eq!(found, 20);
    assert_eq!(found + missing, 1024);
    assert_eq!(deferred, 4020 - 1024);

    // ListTree and ListRefs with an enormous limit get one bounded page.
    let files: BTreeMap<String, Vec<u8>> = (0..1100)
        .map(|i| (format!("f{i:04}"), format!("{i}").into_bytes()))
        .collect();
    let created = c
        .commit("r", None, &BTreeMap::new(), &files, 0)
        .await
        .expect("commit");
    let entries = c
        .repo
        .list_tree(pb::ListTreeRequest {
            repo: "r".into(),
            at: created.id.0.0.to_vec(),
            path: None,
            recursive: true,
            page: Some(pb::Page {
                limit: u32::MAX,
                token: None,
            }),
        })
        .await
        .expect("list")
        .into_inner();
    assert_eq!(entries.entries.len(), graft_store::MAX_PAGE as usize);
    assert!(entries.next.is_some());
    let r = server.get(
        &format!("/v1/repos/r/tree/{}?limit=4294967295", created.id),
        Some(&token),
    );
    assert_eq!(r.status, 200);
    assert_eq!(
        r.json()["entries"].as_array().expect("entries").len(),
        graft_store::MAX_PAGE as usize
    );
}

#[cfg(target_os = "linux")]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_slow_client_holds_back_get_objects() {
    const BLOBS: u64 = 64;
    const SIZE: usize = 1 << 20;
    let site = Site::new("server-slow", "");
    let token = site.mint(&["repo:r:admin"]);
    let blobs: Vec<Vec<u8>> = (0..BLOBS).map(|i| noise(i + 100, SIZE)).collect();
    let ids: Vec<ObjectId> = blobs
        .iter()
        .map(|b| ObjectId::hash(ObjectKind::Blob, b))
        .collect();
    {
        let server = site.start();
        let mut c = server.clients(Some(&token)).await;
        c.create_repo("r").await.expect("create");
        for chunk in blobs.chunks(16) {
            c.put_blobs("r", chunk).await.expect("put");
        }
        server.kill9();
    }

    // A fresh process, so the uploads' memory does not hide the reads'.
    let server = site.start();
    let mut c = server.clients(Some(&token)).await;
    let before = server.rss().expect("rss");
    let mut stream = c
        .objects
        .get_objects(pb::GetObjectsRequest {
            repo: "r".into(),
            ids: ids.iter().map(|i| i.0.to_vec()).collect(),
        })
        .await
        .expect("get")
        .into_inner();
    let first = stream.message().await.expect("page").expect("a page");
    let mut got = first.found.len();

    // Stall. Without backpressure the server reads and queues all 64 MiB;
    // with it, it holds a page or two and one small batch (17 MiB measured,
    // allocator slack included).
    tokio::time::sleep(Duration::from_millis(1500)).await;
    let during = server.rss().expect("rss");
    let growth = during.saturating_sub(before);
    assert!(
        growth < 32 << 20,
        "server memory grew {} MiB while the client stalled",
        growth >> 20
    );
    // Others are still served meanwhile.
    assert_eq!(server.get("/healthz", None).status, 200);

    // The slow client still gets everything.
    while let Some(page) = stream.message().await.expect("page") {
        let page = Objects::try_from(page).expect("valid");
        assert!(page.missing.is_empty() && page.deferred.is_empty());
        got += page.found.len();
    }
    assert_eq!(got as u64, BLOBS);
}

/// Starts a `PutObjects` that sends one message of `size` bytes and then
/// keeps its stream open until the returned sender is dropped.
async fn held_put(
    server: &common::Server,
    token: &str,
    seed: u64,
    size: usize,
) -> (
    mpsc::Sender<pb::PutObjectsRequest>,
    tokio::task::JoinHandle<Result<tonic::Response<pb::ObjectIds>, tonic::Status>>,
) {
    let mut c = server.clients(Some(token)).await;
    let (tx, rx) = mpsc::channel(1);
    tx.send(put_message("r", &[noise(seed, size)]))
        .await
        .expect("queue");
    let call = tokio::spawn(async move { c.objects.put_objects(ReceiverStream::new(rx)).await });
    // Let the server read the message and take its share of the budget.
    tokio::time::sleep(Duration::from_millis(500)).await;
    (tx, call)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn concurrent_puts_share_one_budget() {
    // 64 KiB for the whole server, and a one-second wait for it.
    let site = Site::new(
        "server-put-budget",
        "max_put_call_bytes = 65536\nput_buffer_bytes = 65536\nput_wait_ms = 1000",
    );
    let token = site.mint(&["repo:r:admin"]);
    let server = site.start();
    let mut c = server.clients(Some(&token)).await;
    c.create_repo("r").await.expect("create");

    // A holds 48 KiB of the budget with its stream open.
    let (held, first) = held_put(&server, &token, 1, 48 << 10).await;

    // B needs 32 KiB, which is not there: it stalls, then is refused.
    let started = Instant::now();
    let e = c
        .put_blobs("r", &[noise(2, 32 << 10)])
        .await
        .expect_err("the budget is spent");
    assert_eq!(
        e,
        ApiError::Unavailable {
            retry_after_ms: 1000
        }
    );
    assert!(
        started.elapsed() >= Duration::from_millis(900),
        "refused after waiting, not at once: {:?}",
        started.elapsed()
    );

    // A finishes, and gives its share back.
    drop(held);
    first.await.expect("join").expect("the holder's put");
    c.put_blobs("r", &[noise(3, 60 << 10)])
        .await
        .expect("the whole budget is free again");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_idle_put_gives_its_budget_back() {
    let site = Site::new(
        "server-put-idle",
        "max_put_call_bytes = 65536\nput_buffer_bytes = 65536\nput_wait_ms = 5000\nput_idle_ms = 500",
    );
    let token = site.mint(&["repo:r:admin"]);
    let server = site.start();
    let mut c = server.clients(Some(&token)).await;
    c.create_repo("r").await.expect("create");

    // A sends one message and then nothing; B waits for A's share, which
    // comes back when A has been idle for 500 ms, well within B's 5 s.
    let (held, first) = held_put(&server, &token, 1, 48 << 10).await;
    let started = Instant::now();
    c.put_blobs("r", &[noise(2, 32 << 10)])
        .await
        .expect("the idle writer's share came back");
    assert!(started.elapsed() < Duration::from_secs(4));

    let e = first.await.expect("join").expect_err("idle too long");
    assert_eq!(
        api(e),
        ApiError::Unavailable {
            retry_after_ms: 1000
        }
    );
    drop(held);
}
