//! Remote backends for the v0.2 attest reads and the generalised attest.
//!
//! The server is graft-testkit's loopback gRPC server with the real engine
//! behind it (`EngineRepo`). Pages are followed until the server says there
//! is no next page. Nothing here waits on a clock.

use graft_client::{Endpoint, Remote, attestations, land_checks, review_queue as walk_review};
use graft_engine::EngineConfig;
use graft_engine::policy::{LineChecks, LineConfig};
use graft_model::{
    Author, AuthorKind, Change, EntryKind, ObjectId, ObjectKind, RepoPath, Timestamp,
};
use graft_proto::api::{ObjectApi, RepoApi};
use graft_proto::types::{
    Attest, AttestKind, AttestStatus, AttestV2, AttestationSelector, CreateChangeset,
    GetAttestations, GetLandChecks, GetLineConfig, Land, Landed, LineChecksMode, MergeMode, Page,
    PutObjects, ResolveRef, ReviewQueue,
};
use graft_proto::{ApiError, CallContext, Frame};
use graft_testkit::fake_api::admin_cx;
use graft_testkit::server::TestServer;

#[path = "support/engine.rs"]
mod support;

fn checked() -> EngineConfig {
    EngineConfig {
        lines: vec![LineConfig {
            name: "main".to_owned(),
            checks: LineChecks::PostLand {
                required: vec!["build".to_owned()],
                retries: 0,
            },
            ..LineConfig::direct("main")
        }],
        ..EngineConfig::default()
    }
}

fn page(limit: u32, token: Option<String>) -> Page {
    Page { limit, token }
}

/// Lands `n` one-file changesets on `main` through the client API.
async fn land_n(
    api: &(impl RepoApi + ObjectApi),
    cx: &CallContext,
    n: u8,
) -> Vec<graft_model::ChangesetId> {
    let mut parent = api
        .resolve_ref(
            cx,
            ResolveRef {
                repo: "r".to_owned(),
                name: "main".to_owned(),
            },
        )
        .await
        .expect("resolve")
        .target;
    let mut landed = Vec::new();
    for i in 0..n {
        let name = format!("f{i}.txt");
        let body = format!("body {i}\n");
        let blob = ObjectId::hash(ObjectKind::Blob, body.as_bytes());
        api.put_objects(
            cx,
            PutObjects {
                repo: "r".to_owned(),
                blobs: vec![Frame::encode(ObjectKind::Blob, body.as_bytes())],
            },
        )
        .await
        .expect("put");
        let created = api
            .create_changeset(
                cx,
                CreateChangeset {
                    repo: "r".to_owned(),
                    parents: vec![parent],
                    changes: vec![Change::Put {
                        path: RepoPath::new(&name).unwrap(),
                        kind: EntryKind::File,
                        id: blob,
                    }],
                    message: format!("land {i}"),
                    author: Author {
                        name: "Tester".to_owned(),
                        email: "tester@example.com".to_owned(),
                        kind: AuthorKind::Human,
                    },
                    authored_at: Timestamp {
                        ms: 1_700_000_000_000,
                        offset_min: 0,
                    },
                    metadata: Default::default(),
                    inline_blobs: Vec::new(),
                    workspace: None,
                },
            )
            .await
            .expect("commit");
        let out = api
            .land(
                cx,
                Land {
                    repo: "r".to_owned(),
                    head: created.id,
                    line: "main".to_owned(),
                    squash: false,
                    merge: MergeMode::File,
                    deadline_ms: None,
                    merged_from: None,
                },
            )
            .await
            .expect("land");
        parent = match out {
            Landed::Landed { landed, .. } => landed,
            other => panic!("unexpected {other:?}"),
        };
        landed.push(parent);
    }
    landed
}

#[tokio::test]
async fn queries_page_across_the_in_process_server() {
    // graft-testkit's gRPC server does not serve the v0.2 RPCs. The
    // in-process engine is the server those calls round-trip against;
    // `Remote` still carries the same methods (see the not-implemented test).
    let engine = support::EngineRepo::with_config("r", checked(), &[("README", b"hi\n")]).await;
    let cx = engine.cx();
    let landed = land_n(engine.as_ref(), &cx, 3).await;

    for (status, check) in [
        (AttestStatus::Pending, "build"),
        (AttestStatus::Error, "lint"),
        (AttestStatus::Pass, "docs"),
    ] {
        engine
            .attest_v2(
                &cx,
                AttestV2 {
                    repo: "r".to_owned(),
                    changeset: landed[2],
                    kind: AttestKind::Check,
                    check: check.to_owned(),
                    status,
                    by: Some("ci".to_owned()),
                    url: String::new(),
                },
            )
            .await
            .expect("attest");
    }

    let mut token = None;
    let mut pages = 0u32;
    let mut statuses = Vec::new();
    loop {
        let page = engine
            .get_attestations(
                &cx,
                GetAttestations {
                    repo: "r".to_owned(),
                    selector: AttestationSelector::ByChangeset(landed[2]),
                    kinds: Vec::new(),
                    page: page(1, token.clone()),
                },
            )
            .await
            .expect("attestations");
        assert!(page.attestations.len() <= 1);
        if page.attestations.is_empty() {
            assert!(page.next.is_none());
            break;
        }
        pages += 1;
        token = page.next;
        statuses.extend(page.attestations.into_iter().map(|a| a.status));
        if token.is_none() {
            break;
        }
    }
    assert!(pages >= 2, "attestations used {pages} pages");
    assert_eq!(
        statuses,
        [
            AttestStatus::Pending,
            AttestStatus::Error,
            AttestStatus::Pass
        ]
    );

    let capped = attestations(
        engine.as_ref(),
        &cx,
        GetAttestations {
            repo: "r".to_owned(),
            selector: AttestationSelector::ByChangeset(landed[2]),
            kinds: Vec::new(),
            page: Page::default(),
        },
        2,
    )
    .await
    .expect("helper");
    assert_eq!(capped.len(), 2);
    assert_eq!(capped[0].status, AttestStatus::Pending);
    assert_eq!(capped[1].status, AttestStatus::Error);

    let mut token = None;
    let mut pages = 0u32;
    let mut ids = Vec::new();
    loop {
        let page = engine
            .get_land_checks(
                &cx,
                GetLandChecks {
                    repo: "r".to_owned(),
                    line: "main".to_owned(),
                    from_pos: 0,
                    page: page(1, token.clone()),
                },
            )
            .await
            .expect("land checks");
        assert!(page.rows.len() <= 1);
        if page.rows.is_empty() {
            assert!(page.next.is_none());
            break;
        }
        pages += 1;
        token = page.next;
        ids.extend(page.rows.into_iter().map(|row| row.changeset));
        if token.is_none() {
            break;
        }
    }
    assert!(pages >= 2, "land checks used {pages} pages");
    assert_eq!(ids, landed);

    let capped = land_checks(
        engine.as_ref(),
        &cx,
        GetLandChecks {
            repo: "r".to_owned(),
            line: "main".to_owned(),
            from_pos: 0,
            page: Page::default(),
        },
        2,
    )
    .await
    .expect("land check helper");
    assert_eq!(
        capped.iter().map(|row| row.changeset).collect::<Vec<_>>(),
        landed[..2]
    );

    let mut since = 0;
    let mut pages = 0u32;
    let mut items = Vec::new();
    loop {
        let got = engine
            .review_queue(
                &cx,
                ReviewQueue {
                    repo: "r".to_owned(),
                    line: "main".to_owned(),
                    since_lsn: since,
                    path_prefix: None,
                    max_lines: None,
                    page: page(1, None),
                },
            )
            .await
            .expect("queue");
        assert!(got.items.len() <= 1);
        if got.items.is_empty() {
            assert_eq!(got.next_lsn, None);
            break;
        }
        pages += 1;
        let next = got.next_lsn.expect("cursor moves");
        assert!(next > since);
        since = next;
        items.extend(got.items.into_iter().map(|item| item.changeset));
    }
    assert!(pages >= 2, "review queue used {pages} pages");
    assert_eq!(items, landed);

    let queued = walk_review(
        engine.as_ref(),
        &cx,
        ReviewQueue {
            repo: "r".to_owned(),
            line: "main".to_owned(),
            since_lsn: 0,
            path_prefix: None,
            max_lines: None,
            page: Page::default(),
        },
        2,
    )
    .await
    .expect("queue helper");
    assert_eq!(
        queued.iter().map(|item| item.changeset).collect::<Vec<_>>(),
        landed[..2]
    );
}

/// The pre-v0.2 signature (`check` + `passed`) still type-checks and lands
/// a passing check attestation.
#[tokio::test]
async fn old_attest_call_site_still_compiles() {
    let engine = support::EngineRepo::new("r", &[("README", b"hi\n")]).await;
    let server = TestServer::tcp(engine.clone()).await;
    let remote = Remote::connect(Endpoint::parse(server.url()))
        .await
        .expect("connect");
    let cx = admin_cx();
    let id = land_n(&remote, &cx, 1).await[0];
    // The old signature still type-checks against the remote backend.
    remote
        .attest(
            &cx,
            Attest {
                repo: "r".to_owned(),
                changeset: id,
                check: "ci/test".to_owned(),
                passed: true,
                url: String::new(),
            },
        )
        .await
        .expect("old attest");
    let page = engine
        .get_attestations(
            &engine.cx(),
            GetAttestations {
                repo: "r".to_owned(),
                selector: AttestationSelector::ByChangeset(id),
                kinds: Vec::new(),
                page: page(10, None),
            },
        )
        .await
        .expect("read back");
    assert_eq!(page.attestations.len(), 1);
    assert_eq!(page.attestations[0].status, AttestStatus::Pass);
    assert_eq!(page.attestations[0].kind, AttestKind::Check);
}

/// A peer that has not implemented the v0.2 review service answers with a
/// typed error, not a panic. graft-testkit's loopback server does not serve
/// `ReviewService`, so tonic's `Unimplemented` arrives with no graft detail
/// and maps to `Unavailable`. A backend that leaves the trait default in
/// place answers `Rejected` with policy `unimplemented`.
#[tokio::test]
async fn unimplemented_server_is_a_typed_error() {
    let engine = support::EngineRepo::new("r", &[("README", b"hi\n")]).await;
    let server = TestServer::tcp(engine).await;
    let remote = Remote::connect(Endpoint::parse(server.url()))
        .await
        .expect("connect");
    let cx = admin_cx();
    let land = remote
        .get_land_checks(
            &cx,
            GetLandChecks {
                repo: "r".to_owned(),
                line: "main".to_owned(),
                from_pos: 0,
                page: page(1, None),
            },
        )
        .await;
    assert!(
        matches!(land, Err(ApiError::Unavailable { .. })),
        "land checks: {land:?}"
    );
    let queue = remote
        .review_queue(
            &cx,
            ReviewQueue {
                repo: "r".to_owned(),
                line: "main".to_owned(),
                since_lsn: 0,
                path_prefix: None,
                max_lines: None,
                page: page(1, None),
            },
        )
        .await;
    assert!(
        matches!(queue, Err(ApiError::Unavailable { .. })),
        "review queue: {queue:?}"
    );

    /// Implements every required method as unreachable and keeps the v0.2
    /// defaults, which answer `Rejected { policy: "unimplemented" }`.
    struct OldOnly;
    macro_rules! dead {
        ($name:ident, $in:ty, $out:ty) => {
            fn $name(
                &self,
                _: &CallContext,
                _: $in,
            ) -> impl std::future::Future<Output = graft_proto::error::ApiResult<$out>> + Send {
                async { unreachable!("not called") }
            }
        };
    }
    impl RepoApi for OldOnly {
        dead!(
            create_changeset,
            CreateChangeset,
            graft_proto::types::Created
        );
        dead!(
            get_changesets,
            graft_proto::types::GetChangesets,
            graft_proto::types::Changesets
        );
        dead!(resolve_ref, ResolveRef, graft_proto::types::RefView);
        dead!(
            list_refs,
            graft_proto::types::ListRefs,
            Vec<graft_proto::types::RefView>
        );
        dead!(
            update_ref,
            graft_proto::types::UpdateRef,
            graft_proto::types::RefView
        );
        dead!(
            stat,
            graft_proto::types::PathAt,
            graft_proto::types::EntryView
        );
        dead!(
            list_tree,
            graft_proto::types::ListTree,
            graft_proto::types::Entries
        );
        dead!(
            read_file,
            graft_proto::types::PathAt,
            graft_proto::types::FileContent
        );
        dead!(log, graft_proto::types::Log, graft_proto::types::History);
        dead!(is_ancestor, graft_proto::types::Pair, bool);
        dead!(
            merge_base,
            graft_proto::types::Pair,
            Option<graft_model::ChangesetId>
        );
        dead!(
            diff,
            graft_proto::types::Diff,
            graft_proto::types::DiffResult
        );
        dead!(
            blame,
            graft_proto::types::Blame,
            graft_proto::types::BlameResult
        );
        dead!(rebase, graft_proto::types::Rebase, graft_model::ChangesetId);
        dead!(revert, graft_proto::types::Revert, graft_model::ChangesetId);
        dead!(land, Land, Landed);
        dead!(attest, Attest, ());
        dead!(
            watch,
            graft_proto::types::Watch,
            graft_proto::types::JournalPage
        );
    }
    let err = OldOnly
        .get_attestations(
            &cx,
            GetAttestations {
                repo: "r".to_owned(),
                selector: AttestationSelector::ByChangeset(graft_model::ChangesetId(
                    ObjectId::hash(ObjectKind::Changeset, b"none"),
                )),
                kinds: Vec::new(),
                page: page(1, None),
            },
        )
        .await;
    assert!(
        matches!(
            err,
            Err(ApiError::Rejected { ref policy, .. }) if policy == "unimplemented"
        ),
        "default: {err:?}"
    );
}

/// WI-2614: `Remote::get_line_config` round-trips the line's settings over
/// gRPC, and an unknown line is `NotFound`.
#[tokio::test]
async fn line_config_round_trips_through_remote() {
    let mut config = checked();
    config.lines[0].checks = LineChecks::PostLand {
        required: vec!["build".to_owned(), "test".to_owned()],
        retries: 2,
    };
    let engine = support::EngineRepo::with_config("r", config, &[("README", b"hi\n")]).await;
    let server = TestServer::tcp_workspaces(engine).await;
    let remote = Remote::connect(Endpoint::parse(server.url()))
        .await
        .expect("connect");
    let cx = admin_cx();
    let got = remote
        .get_line_config(
            &cx,
            GetLineConfig {
                repo: "r".to_owned(),
                line: "main".to_owned(),
            },
        )
        .await
        .expect("line config");
    assert_eq!(
        got,
        graft_proto::types::LineConfig {
            line: "main".to_owned(),
            checks: LineChecksMode::PostLand,
            required: vec!["build".to_owned(), "test".to_owned()],
            retries: 2,
        }
    );
    let missing = remote
        .get_line_config(
            &cx,
            GetLineConfig {
                repo: "r".to_owned(),
                line: "nope".to_owned(),
            },
        )
        .await;
    assert!(
        matches!(missing, Err(ApiError::NotFound { .. })),
        "{missing:?}"
    );
}
