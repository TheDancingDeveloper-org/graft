//! Forwarding: every repository, workspace and admin RPC (and `HasObjects`,
//! `PutObjects`) reaches the server with the caller's token, every time;
//! nothing else is forwarded; load is shed; an unreachable upstream fails
//! fast.

mod common;

use std::time::{Duration, Instant};

use common::*;
use graft_proto::types::GetObjects;
use graft_proto::{ApiError, ObjectApi as _, PROTOCOL_VERSION, VERSION_HEADER, pb};
use tonic::codegen::http::uri::PathAndQuery;
use tonic::metadata::MetadataValue;

/// Every RPC in the proto, as `/graft.v1.<Service>/<Method>`.
fn proto_methods() -> Vec<String> {
    let text = include_str!("../../graft-proto/proto/graft/v1/graft.proto");
    let mut service = "";
    let mut out = Vec::new();
    for line in text.lines().map(str::trim) {
        if let Some(rest) = line.strip_prefix("service ") {
            service = rest.split_whitespace().next().unwrap();
        } else if let Some(rest) = line.strip_prefix("rpc ") {
            let method = rest.split('(').next().unwrap().trim();
            out.push(format!("/graft.v1.{service}/{method}"));
        }
    }
    assert!(out.len() > 20, "parsed the proto");
    out
}

/// Sends an empty message to `path` with `token`; returns the status.
async fn raw(node: &NodeRun, path: &str, token: Option<&str>) -> tonic::Status {
    let mut grpc = tonic::client::Grpc::new(node.channel().await);
    grpc.ready().await.unwrap();
    let mut req = tonic::Request::new(pb::Empty {});
    let md = req.metadata_mut();
    md.insert(
        VERSION_HEADER,
        MetadataValue::try_from(PROTOCOL_VERSION.to_string()).unwrap(),
    );
    if let Some(t) = token {
        md.insert(
            "authorization",
            MetadataValue::try_from(format!("Bearer {t}")).unwrap(),
        );
    }
    let codec = tonic_prost::ProstCodec::<pb::Empty, pb::Empty>::default();
    let path = PathAndQuery::try_from(path.to_owned()).unwrap();
    match grpc.unary(req, path, codec).await {
        Ok(_) => tonic::Status::ok(""),
        Err(s) => s,
    }
}

#[tokio::test]
async fn every_rpc_but_get_objects_is_forwarded_with_the_callers_token_every_time() {
    let up = Upstream::start().await;
    let dir = tempfile::tempdir().unwrap();
    let node = NodeRun::start(config(dir.path(), &up.url())).await;
    let tok = token(&["repo:r:read"]);
    let methods: Vec<String> = proto_methods()
        .into_iter()
        .filter(|m| m != "/graft.v1.ObjectService/GetObjects")
        .collect();
    let listed: Vec<&str> = graft_node::forward::FORWARDED
        .iter()
        .map(|(p, _)| *p)
        .collect();
    assert_eq!(
        methods.iter().map(String::as_str).collect::<Vec<_>>(),
        listed,
        "the forward list matches the proto"
    );
    for round in 1..=2 {
        for m in &methods {
            let status = raw(&node, m, Some(&tok)).await;
            assert_eq!(status.message(), FAKE_MESSAGE, "{m} was not forwarded");
            let calls = up.calls();
            let seen = calls.iter().filter(|(p, _)| p == m).count();
            // Never answered from a cache: each call reaches upstream.
            assert_eq!(seen, round, "{m}");
            let last = calls.iter().rev().find(|(p, _)| p == m).unwrap();
            assert_eq!(last.1.as_deref(), Some(format!("Bearer {tok}").as_str()));
        }
    }
    // A forwarded call goes up unchanged: no token stays no token.
    let status = raw(&node, "/graft.v1.RepoService/ResolveRef", None).await;
    assert_eq!(status.message(), FAKE_MESSAGE);
    assert_eq!(up.calls().last().unwrap().1, None);
}

#[tokio::test]
async fn other_paths_are_not_forwarded() {
    let up = Upstream::start().await;
    let dir = tempfile::tempdir().unwrap();
    let node = NodeRun::start(config(dir.path(), &up.url())).await;
    let tok = token(&["repo:r:admin"]);
    for path in [
        "/graft.v1.RepoService/Nope",
        "/graft.v2.RepoService/ResolveRef",
        "/v1/repos/r/refs",
        "/grpc.reflection.v1.ServerReflection/ServerReflectionInfo",
    ] {
        let status = raw(&node, path, Some(&tok)).await;
        assert_eq!(status.code(), tonic::Code::Unimplemented, "{path}");
    }
    assert!(up.calls().is_empty());
}

#[tokio::test]
async fn an_unreachable_upstream_answers_unavailable_fast() {
    let mut up = Upstream::start().await;
    let dir = tempfile::tempdir().unwrap();
    let node = NodeRun::start(config(dir.path(), &up.url())).await;
    let tok = token(&["repo:r:read"]);
    assert_eq!(
        raw(&node, "/graft.v1.RepoService/ResolveRef", Some(&tok))
            .await
            .message(),
        FAKE_MESSAGE
    );
    up.stop().await;
    for _ in 0..5 {
        let started = Instant::now();
        let status = raw(&node, "/graft.v1.RepoService/ResolveRef", Some(&tok)).await;
        assert!(
            matches!(
                ApiError::from_status(&status),
                Some(ApiError::Unavailable { .. })
            ),
            "{status:?}"
        );
        assert!(
            started.elapsed() < Duration::from_secs(1),
            "{:?}",
            started.elapsed()
        );
    }
    assert!(
        node.node
            .metrics()
            .shed
            .with_label_values(&["breaker"])
            .get()
            > 0
    );

    // When it comes back, calls flow again once the breaker's backoff ends.
    let back = Upstream::start_on(&up.addr.to_string(), Fake::default()).await;
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let status = raw(&node, "/graft.v1.RepoService/ResolveRef", Some(&tok)).await;
        if status.message() == FAKE_MESSAGE {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "upstream never came back: {status:?}"
        );
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    assert!(!back.calls().is_empty());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn upstream_calls_are_bounded_and_the_excess_shed() {
    let up = Upstream::start().await;
    let (a, fa) = blob(1);
    let (b, fb) = blob(2);
    up.put("r", a, fa);
    up.put("r", b, fb);
    up.set_delay(Duration::from_millis(600));
    let dir = tempfile::tempdir().unwrap();
    let mut cfg = config(dir.path(), &up.url());
    cfg.max_upstream_calls = 1;
    cfg.max_upstream_waiting = 0;
    let node = NodeRun::start(cfg).await;
    let tok = token(&["repo:r:read"]);
    let first = node.client(&tok).await;
    let second = node.client(&tok).await;
    let slow = tokio::spawn(async move {
        first
            .get_objects(
                &cx(),
                GetObjects {
                    repo: "r".into(),
                    ids: vec![a],
                },
            )
            .await
    });
    tokio::time::sleep(Duration::from_millis(150)).await;
    let started = Instant::now();
    let shed = second
        .get_objects(
            &cx(),
            GetObjects {
                repo: "r".into(),
                ids: vec![b],
            },
        )
        .await;
    assert!(
        matches!(shed, Err(ApiError::Unavailable { .. })),
        "{shed:?}"
    );
    let forwarded = raw(&node, "/graft.v1.RepoService/ResolveRef", Some(&tok)).await;
    assert!(matches!(
        ApiError::from_status(&forwarded),
        Some(ApiError::Unavailable { .. })
    ));
    assert!(
        started.elapsed() < Duration::from_millis(400),
        "shed, not queued"
    );
    let objects = slow.await.unwrap().unwrap();
    assert_eq!(objects.found.len(), 1);
    assert!(node.node.metrics().shed.with_label_values(&["queue"]).get() >= 2);
}

#[tokio::test]
async fn the_in_flight_object_bound_sheds_misses() {
    let up = Upstream::start().await;
    let (a, fa) = blob(1);
    let (b, fb) = blob(2);
    up.put("r", a, fa);
    up.put("r", b, fb);
    up.set_delay(Duration::from_millis(500));
    let dir = tempfile::tempdir().unwrap();
    let mut cfg = config(dir.path(), &up.url());
    cfg.max_inflight_objects = 1;
    let node = NodeRun::start(cfg).await;
    let tok = token(&["repo:r:read"]);
    let first = node.client(&tok).await;
    let slow = tokio::spawn(async move {
        first
            .get_objects(
                &cx(),
                GetObjects {
                    repo: "r".into(),
                    ids: vec![a],
                },
            )
            .await
    });
    tokio::time::sleep(Duration::from_millis(150)).await;
    let shed = node
        .client(&tok)
        .await
        .get_objects(
            &cx(),
            GetObjects {
                repo: "r".into(),
                ids: vec![b],
            },
        )
        .await;
    assert!(
        matches!(shed, Err(ApiError::Unavailable { .. })),
        "{shed:?}"
    );
    assert_eq!(slow.await.unwrap().unwrap().found.len(), 1);
    assert_eq!(up.asked("r", b), 0);
}
