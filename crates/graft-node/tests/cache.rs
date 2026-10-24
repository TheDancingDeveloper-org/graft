//! The object cache: single-flight, upstream outages, corruption from disk
//! and from upstream, repository isolation, eviction, and crash leftovers.

mod common;

use std::sync::Arc;
use std::time::{Duration, Instant};

use common::*;
use graft_model::{ObjectId, ObjectKind};
use graft_proto::types::GetObjects;
use graft_proto::{ApiError, Frame, ObjectApi as _};

fn get(repo: &str, ids: &[ObjectId]) -> GetObjects {
    GetObjects {
        repo: repo.to_owned(),
        ids: ids.to_vec(),
    }
}

/// Checks every found frame against its id and returns the found ids.
fn verified(objects: &graft_proto::types::Objects) -> Vec<ObjectId> {
    for (id, frame) in &objects.found {
        frame.decode_verified(*id).expect("frame matches its id");
    }
    objects.found.iter().map(|(id, _)| *id).collect()
}

fn seed(up: &Upstream, repo: &str, n: usize) -> Vec<ObjectId> {
    (0..n)
        .map(|i| {
            let (id, frame) = blob(i);
            up.put(repo, id, frame);
            id
        })
        .collect()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn thirty_concurrent_clones_fetch_each_object_once() {
    let up = Upstream::start().await;
    let ids = seed(&up, "r", 150);
    up.set_delay(Duration::from_millis(200));
    let dir = tempfile::tempdir().unwrap();
    let node = NodeRun::start(config(dir.path(), &up.url())).await;
    let tok = token(&["repo:r:read"]);

    let mut clients = Vec::new();
    for _ in 0..30 {
        clients.push(node.client(&tok).await);
    }
    let mut tasks = tokio::task::JoinSet::new();
    for client in clients {
        let ids = ids.clone();
        tasks.spawn(async move { client.get_objects(&cx(), get("r", &ids)).await });
    }
    while let Some(done) = tasks.join_next().await {
        let objects = done.unwrap().expect("clone succeeds");
        assert_eq!(verified(&objects).len(), ids.len());
        assert!(objects.missing.is_empty() && objects.deferred.is_empty());
    }
    for id in &ids {
        assert_eq!(up.asked("r", *id), 1, "object fetched more than once");
    }
    let m = node.node.metrics();
    assert!(m.base.singleflight_shared_total.get() > 0);
    assert_eq!(m.upstream_objects.get(), ids.len() as u64);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn with_upstream_down_cached_reads_work_and_misses_fail_fast() {
    let mut up = Upstream::start().await;
    let ids = seed(&up, "r", 20);
    let dir = tempfile::tempdir().unwrap();
    let mut node = NodeRun::start(config(dir.path(), &up.url())).await;
    let tok = token(&["repo:r:read"]);
    let client = node.client(&tok).await;
    let warm = client
        .get_objects(&cx(), get("r", &ids[..10]))
        .await
        .unwrap();
    assert_eq!(verified(&warm).len(), 10);
    node.settle().await;
    up.stop().await;

    let cached = client
        .get_objects(&cx(), get("r", &ids[..10]))
        .await
        .unwrap();
    assert_eq!(verified(&cached).len(), 10);

    let started = Instant::now();
    let miss = client.get_objects(&cx(), get("r", &ids[10..12])).await;
    assert!(
        matches!(miss, Err(ApiError::Unavailable { .. })),
        "{miss:?}"
    );
    assert!(started.elapsed() < Duration::from_secs(2));

    // The cache survives a restart, still with no upstream.
    drop(client);
    node.stop().await;
    drop(node);
    let node = NodeRun::start(config(dir.path(), &up.url())).await;
    let client = node.client(&tok).await;
    let again = client
        .get_objects(&cx(), get("r", &ids[..10]))
        .await
        .unwrap();
    assert_eq!(verified(&again).len(), 10);
}

#[tokio::test]
async fn a_corrupt_cache_entry_is_discarded_and_refetched() {
    let up = Upstream::start().await;
    let ids = seed(&up, "r", 1);
    let dir = tempfile::tempdir().unwrap();
    let node = NodeRun::start(config(dir.path(), &up.url())).await;
    let client = node.client(&token(&["repo:r:read"])).await;
    client.get_objects(&cx(), get("r", &ids)).await.unwrap();
    node.settle().await;
    let files = cached_files(&node.dir);
    assert_eq!(files.len(), 1);
    // Flip the last byte of the stored frame.
    let mut bytes = std::fs::read(&files[0]).unwrap();
    *bytes.last_mut().unwrap() ^= 0xff;
    std::fs::write(&files[0], &bytes).unwrap();

    let objects = client.get_objects(&cx(), get("r", &ids)).await.unwrap();
    assert_eq!(verified(&objects), ids);
    assert_eq!(up.asked("r", ids[0]), 2);
    node.settle().await;
    assert_eq!(
        node.node
            .metrics()
            .corrupt
            .with_label_values(&["cache"])
            .get(),
        1
    );
    // The refetched copy replaced it and is served from cache.
    let objects = client.get_objects(&cx(), get("r", &ids)).await.unwrap();
    assert_eq!(verified(&objects), ids);
    assert_eq!(up.asked("r", ids[0]), 2);
}

#[tokio::test]
async fn a_corrupt_upstream_frame_is_never_cached_or_served() {
    let up = Upstream::start().await;
    let (id, _) = blob(1);
    let (_, other) = blob(2);
    up.put("r", id, other);
    let (good, good_frame) = blob(3);
    up.put("r", good, good_frame);
    // A frame claiming the right id whose bytes are cut short.
    let (short, short_frame) = blob(4);
    up.put("r", short, short_frame[..short_frame.len() - 1].to_vec());
    let dir = tempfile::tempdir().unwrap();
    let node = NodeRun::start(config(dir.path(), &up.url())).await;
    let client = node.client(&token(&["repo:r:read"])).await;

    for bad in [id, short] {
        let got = client.get_objects(&cx(), get("r", &[bad])).await;
        assert_eq!(got, Err(ApiError::CorruptObject { id: bad }));
    }
    node.settle().await;
    assert!(cached_files(&node.dir).is_empty());
    let got = client.get_objects(&cx(), get("r", &[id])).await;
    assert_eq!(got, Err(ApiError::CorruptObject { id }));
    assert_eq!(up.asked("r", id), 2, "a corrupt frame must not be cached");

    let objects = client.get_objects(&cx(), get("r", &[good])).await.unwrap();
    assert_eq!(verified(&objects), vec![good]);
    assert_eq!(
        node.node
            .metrics()
            .corrupt
            .with_label_values(&["upstream"])
            .get(),
        3
    );
}

#[tokio::test]
async fn unrequested_frames_from_upstream_are_dropped() {
    let up = Upstream::start().await;
    let ids = seed(&up, "r", 1);
    let (stray, stray_frame) = blob(99);
    up.state
        .lock()
        .unwrap()
        .extra
        .push((stray.0.to_vec(), stray_frame));
    let dir = tempfile::tempdir().unwrap();
    let node = NodeRun::start(config(dir.path(), &up.url())).await;
    let client = node.client(&token(&["repo:r:read"])).await;
    let objects = client.get_objects(&cx(), get("r", &ids)).await.unwrap();
    assert_eq!(verified(&objects), ids);
    node.settle().await;
    assert_eq!(
        cached_files(&node.dir).len(),
        1,
        "only the asked-for object is cached"
    );
    assert!(node.node.metrics().upstream_protocol_errors.get() >= 1);
}

#[tokio::test]
async fn repositories_never_share_cached_objects() {
    let up = Upstream::start().await;
    let ids = seed(&up, "a", 1);
    let dir = tempfile::tempdir().unwrap();
    let node = NodeRun::start(config(dir.path(), &up.url())).await;
    let a = node.client(&token(&["repo:a:read"])).await;
    let b = node.client(&token(&["repo:b:read"])).await;
    a.get_objects(&cx(), get("a", &ids)).await.unwrap();
    node.settle().await;

    // Reading b's objects never answers from a's cache: b upstream lacks it.
    let objects = b.get_objects(&cx(), get("b", &ids)).await.unwrap();
    assert!(objects.found.is_empty());
    assert_eq!(objects.missing, ids);
    assert_eq!(up.asked("b", ids[0]), 1);
    // And a caller of b can't read a's.
    let denied = b.get_objects(&cx(), get("a", &ids)).await;
    assert!(
        matches!(denied, Err(ApiError::PermissionDenied { .. })),
        "{denied:?}"
    );
    // `objects:read` reads any repository.
    let svc = node.client(&token(&["objects:read"])).await;
    let objects = svc.get_objects(&cx(), get("a", &ids)).await.unwrap();
    assert_eq!(verified(&objects), ids);
    assert_eq!(up.asked("a", ids[0]), 1);
}

#[tokio::test]
async fn callers_are_verified_locally() {
    let up = Upstream::start().await;
    let ids = seed(&up, "r", 1);
    let dir = tempfile::tempdir().unwrap();
    let node = NodeRun::start(config(dir.path(), &up.url())).await;
    let forged = {
        let other = graft_auth::SigningKey::from_seed(&[9u8; 32]);
        let claims = graft_auth::Claims {
            principal: graft_proto::Principal {
                subject: "x".into(),
                kind: graft_proto::PrincipalKind::Agent,
                session: None,
                work_item: None,
                scopes: vec!["repo:r:read".into()],
            },
            exp: now_s() + 60,
            jti: "j".into(),
        };
        graft_auth::mint(&other, &claims)
            .unwrap()
            .expose()
            .to_owned()
    };
    for tok in [
        forged,
        token_until(&["repo:r:read"], now_s() - 1),
        "not-a-token".to_owned(),
    ] {
        let got = node
            .client(&tok)
            .await
            .get_objects(&cx(), get("r", &ids))
            .await;
        assert_eq!(got, Err(ApiError::Unauthenticated));
    }
    let anonymous =
        graft_client::Remote::connect(graft_client::Endpoint::NodeSocket(node.socket.clone()))
            .await
            .unwrap();
    let got = anonymous.get_objects(&cx(), get("r", &ids)).await;
    assert_eq!(got, Err(ApiError::Unauthenticated));
    let got = node
        .client(&token(&["repo:other:read"]))
        .await
        .get_objects(&cx(), get("r", &ids))
        .await;
    assert!(matches!(got, Err(ApiError::PermissionDenied { .. })));
    assert_eq!(
        up.asked("r", ids[0]),
        0,
        "refused calls never reach upstream"
    );
}

#[tokio::test]
async fn misses_are_not_cached_as_negatives() {
    let up = Upstream::start().await;
    let (id, frame) = blob(5);
    let dir = tempfile::tempdir().unwrap();
    let node = NodeRun::start(config(dir.path(), &up.url())).await;
    let client = node.client(&token(&["repo:r:read"])).await;
    let objects = client.get_objects(&cx(), get("r", &[id])).await.unwrap();
    assert_eq!(objects.missing, vec![id]);
    up.put("r", id, frame);
    let objects = client.get_objects(&cx(), get("r", &[id])).await.unwrap();
    assert_eq!(verified(&objects), vec![id]);
}

#[tokio::test]
async fn the_cache_stays_within_its_bound_and_refetches_what_it_evicted() {
    let up = Upstream::start().await;
    let ids = seed(&up, "r", 60);
    let dir = tempfile::tempdir().unwrap();
    let mut cfg = config(dir.path(), &up.url());
    cfg.max_bytes = 1_000;
    let node = NodeRun::start(cfg).await;
    let client = node.client(&token(&["repo:r:read"])).await;
    let objects = client.get_objects(&cx(), get("r", &ids)).await.unwrap();
    assert_eq!(verified(&objects).len(), ids.len());
    node.settle().await;
    // No blob frame here is over 200 bytes.
    let largest = 200;
    let stored: u64 = cached_files(&node.dir)
        .iter()
        .map(|p| std::fs::metadata(p).unwrap().len())
        .sum();
    assert!(stored <= 1_000 + largest, "cache holds {stored} bytes");
    let objects = client.get_objects(&cx(), get("r", &ids)).await.unwrap();
    assert_eq!(verified(&objects).len(), ids.len());
    let asked: usize = ids.iter().map(|id| up.asked("r", *id)).sum();
    assert!(asked > ids.len(), "evicted objects are fetched again");
}

#[tokio::test]
async fn below_the_free_space_floor_answers_are_served_but_not_cached() {
    let up = Upstream::start().await;
    let ids = seed(&up, "r", 3);
    let dir = tempfile::tempdir().unwrap();
    let mut cfg = config(dir.path(), &up.url());
    cfg.min_free_bytes = u64::MAX;
    let node = NodeRun::start(cfg).await;
    let client = node.client(&token(&["repo:r:read"])).await;
    for _ in 0..2 {
        let objects = client.get_objects(&cx(), get("r", &ids)).await.unwrap();
        assert_eq!(verified(&objects).len(), 3);
        node.settle().await;
    }
    assert!(cached_files(&node.dir).is_empty());
    assert_eq!(up.asked("r", ids[0]), 2);
    assert_eq!(
        node.node
            .metrics()
            .cache_skipped
            .with_label_values(&["disk"])
            .get(),
        6
    );
}

#[tokio::test]
async fn crash_leftovers_are_cleaned_and_a_broken_index_wipes_the_cache() {
    let up = Upstream::start().await;
    let ids = seed(&up, "r", 2);
    let dir = tempfile::tempdir().unwrap();
    let cfg = config(dir.path(), &up.url());
    {
        let mut node = NodeRun::start(cfg.clone()).await;
        let client = node.client(&token(&["repo:r:read"])).await;
        client.get_objects(&cx(), get("r", &ids)).await.unwrap();
        node.settle().await;
        drop(client);
        node.stop().await;
    }
    // A write cut short by a crash leaves a temp file beside the objects,
    // and no saved cache counters: the put removed them before it wrote
    // (graft-store-fs, WI-2341), so the next open walks the cache.
    let _ = std::fs::remove_file(cfg.cache_dir.join("objects").join(".usage"));
    let shard = cached_files(&cfg.cache_dir)[0].parent().unwrap().to_owned();
    let tmp = shard.join(format!("{}.1.0.tmp", "0".repeat(62)));
    std::fs::write(&tmp, b"GRF1 partial").unwrap();
    {
        let mut node = NodeRun::start(cfg.clone()).await;
        assert!(!tmp.exists(), "temp files are removed at open");
        assert_eq!(cached_files(&cfg.cache_dir).len(), 2);
        node.stop().await;
    }
    std::fs::write(cfg.cache_dir.join("repos"), b"garbage").unwrap();
    let node = NodeRun::start(cfg.clone()).await;
    assert!(
        cached_files(&cfg.cache_dir).is_empty(),
        "the cache was wiped"
    );
    let client = node.client(&token(&["repo:r:read"])).await;
    let objects = client.get_objects(&cx(), get("r", &ids)).await.unwrap();
    assert_eq!(verified(&objects).len(), 2);
}

#[tokio::test]
async fn large_answers_are_bounded_and_the_rest_deferred() {
    let up = Upstream::start().await;
    let ids: Vec<ObjectId> = (0..1_100)
        .map(|i| {
            let payload = format!("tiny {i}");
            let id = ObjectId::hash(ObjectKind::Blob, payload.as_bytes());
            up.put(
                "r",
                id,
                Frame::encode(ObjectKind::Blob, payload.as_bytes()).to_bytes(),
            );
            id
        })
        .collect();
    let dir = tempfile::tempdir().unwrap();
    let node = NodeRun::start(config(dir.path(), &up.url())).await;
    let client = Arc::new(node.client(&token(&["repo:r:read"])).await);
    let objects = client.get_objects(&cx(), get("r", &ids)).await.unwrap();
    assert_eq!(verified(&objects).len(), 1024);
    assert_eq!(objects.deferred, ids[1024..].to_vec());
}
