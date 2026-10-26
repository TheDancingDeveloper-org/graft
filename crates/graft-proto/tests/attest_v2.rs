//! The v0.2 attestation and review interface (WI-2590): old clients stay
//! compatible, every new read is bounded, and the stubs answer
//! not-implemented.

use std::future::Future;
use std::pin::pin;
use std::task::{Context, Poll, Waker};

use graft_model::{Author, AuthorKind, ChangesetId, ObjectId, RepoPath};
use graft_proto::types::*;
use graft_proto::{ApiError, CallContext, Principal, PrincipalKind, RepoApi, pb};
use prost::Message;

fn cs(n: u8) -> ChangesetId {
    ChangesetId(ObjectId([n; 32]))
}

fn block_on<F: Future>(f: F) -> F::Output {
    let mut f = pin!(f);
    let mut cx = Context::from_waker(Waker::noop());
    loop {
        if let Poll::Ready(v) = f.as_mut().poll(&mut cx) {
            return v;
        }
    }
}

/// Accept 1: bytes from the v0.1 client decode to `kind = Check` with the
/// status its `passed` flag meant.
#[test]
fn old_client_attest_decodes_as_check_with_same_status() {
    for passed in [true, false] {
        let old = Attest {
            repo: "r".into(),
            changeset: cs(7),
            check: "ci/test".into(),
            passed,
            url: "https://ci.example/run/1".into(),
        };
        let bytes = pb::AttestRequest::from(old.clone()).encode_to_vec();
        let v2 = AttestV2::try_from(pb::AttestRequest::decode(bytes.as_slice()).unwrap()).unwrap();
        assert_eq!(v2.kind, AttestKind::Check);
        assert_eq!(
            v2.status,
            if passed {
                AttestStatus::Pass
            } else {
                AttestStatus::Fail
            }
        );
        assert_eq!(v2, AttestV2::from(old.clone()));
        // And an old server still reads the old fields from a v0.2 client.
        let bytes = pb::AttestRequest::from(v2).encode_to_vec();
        let back = Attest::try_from(pb::AttestRequest::decode(bytes.as_slice()).unwrap()).unwrap();
        assert_eq!(back, old);
    }
}

#[test]
fn attest_v2_round_trips() {
    let v2 = AttestV2 {
        repo: "r".into(),
        changeset: cs(1),
        kind: AttestKind::Review,
        check: "review".into(),
        status: AttestStatus::Pending,
        by: Some("Reviewer".into()),
        url: String::new(),
    };
    let bytes = pb::AttestRequest::from(v2.clone()).encode_to_vec();
    let back = AttestV2::try_from(pb::AttestRequest::decode(bytes.as_slice()).unwrap()).unwrap();
    assert_eq!(back, v2);
}

#[test]
fn unknown_attest_kind_is_rejected_at_the_boundary() {
    let mut req = pb::AttestRequest::from(Attest {
        repo: "r".into(),
        changeset: cs(1),
        check: "c".into(),
        passed: true,
        url: String::new(),
    });
    req.kind = 99;
    assert_eq!(AttestV2::try_from(req).unwrap_err().field, "kind");
}

fn page(limit: u32) -> Page {
    Page { limit, token: None }
}

/// Accept 2: every new request carries a page, clamped on decode.
#[test]
fn review_reads_clamp_the_page_limit() {
    let over = MAX_REVIEW_PAGE + 1;
    let a = pb::GetAttestationsRequest::from(GetAttestations {
        repo: "r".into(),
        selector: AttestationSelector::ByLineRange {
            line: "main".into(),
            from_pos: 1,
            to_pos: 9,
        },
        kinds: vec![AttestKind::Check, AttestKind::Approval],
        page: page(over),
    });
    let a = GetAttestations::try_from(a).unwrap();
    assert_eq!(a.page.limit, MAX_REVIEW_PAGE);
    assert_eq!(a.kinds, vec![AttestKind::Check, AttestKind::Approval]);

    let l = pb::GetLandChecksRequest::from(GetLandChecks {
        repo: "r".into(),
        line: "main".into(),
        from_pos: 3,
        page: page(u32::MAX),
    });
    assert_eq!(
        GetLandChecks::try_from(l).unwrap().page.limit,
        MAX_REVIEW_PAGE
    );

    let q = pb::ReviewQueueRequest::from(ReviewQueue {
        repo: "r".into(),
        line: "main".into(),
        since_lsn: 5,
        path_prefix: Some(RepoPath::new("src").unwrap()),
        max_lines: Some(400),
        page: page(over),
    });
    let q = ReviewQueue::try_from(q).unwrap();
    assert_eq!(q.page.limit, MAX_REVIEW_PAGE);
    assert_eq!(q.path_prefix, Some(RepoPath::new("src").unwrap()));

    // Within the cap, the limit is kept.
    let a = pb::GetAttestationsRequest::from(GetAttestations {
        repo: "r".into(),
        selector: AttestationSelector::ByChangeset(cs(2)),
        kinds: vec![],
        page: page(10),
    });
    let a = GetAttestations::try_from(a).unwrap();
    assert_eq!(a.page.limit, 10);
    assert_eq!(a.selector, AttestationSelector::ByChangeset(cs(2)));
}

#[test]
fn get_attestations_needs_a_selector() {
    let req = pb::GetAttestationsRequest {
        repo: "r".into(),
        ..Default::default()
    };
    assert_eq!(
        GetAttestations::try_from(req).unwrap_err().field,
        "selector"
    );
}

#[test]
fn response_pages_round_trip() {
    let a = AttestationPage {
        attestations: vec![Attestation {
            changeset: cs(1),
            kind: AttestKind::Approval,
            check: "approve".into(),
            status: AttestStatus::Pass,
            by: Some("Owner".into()),
            url: String::new(),
        }],
        next: Some("t".into()),
    };
    let b = pb::AttestationPage::from(a.clone()).encode_to_vec();
    assert_eq!(
        AttestationPage::try_from(pb::AttestationPage::decode(b.as_slice()).unwrap()).unwrap(),
        a
    );

    let l = LandCheckPage {
        rows: vec![
            LandCheck {
                pos: 1,
                changeset: cs(1),
                state: LandCheckState::Good,
                revert: None,
            },
            LandCheck {
                pos: 2,
                changeset: cs(2),
                state: LandCheckState::Reverted,
                revert: Some(cs(3)),
            },
        ],
        next: None,
    };
    let b = pb::LandCheckPage::from(l.clone()).encode_to_vec();
    assert_eq!(
        LandCheckPage::try_from(pb::LandCheckPage::decode(b.as_slice()).unwrap()).unwrap(),
        l
    );

    let q = ReviewQueuePage {
        items: vec![ReviewItem {
            changeset: cs(4),
            pos: 4,
            lsn: 40,
            author: Author {
                name: "A".into(),
                email: "a@example.com".into(),
                kind: AuthorKind::Agent,
            },
            paths_touched: 3,
            lines_changed: 120,
        }],
        next_lsn: Some(41),
    };
    let b = pb::ReviewQueuePage::from(q.clone()).encode_to_vec();
    assert_eq!(
        ReviewQueuePage::try_from(pb::ReviewQueuePage::decode(b.as_slice()).unwrap()).unwrap(),
        q
    );
}

/// Accept 5: the new types hold ids, names and counts only, so `Debug`
/// shows nothing a log must not carry.
#[test]
fn debug_of_new_types_is_safe() {
    let v2 = AttestV2 {
        repo: "r".into(),
        changeset: cs(1),
        kind: AttestKind::Check,
        check: "ci/test".into(),
        status: AttestStatus::Error,
        by: Some("CI".into()),
        url: "https://ci.example/1".into(),
    };
    let s = format!("{v2:?}");
    assert!(s.contains("ci/test") && !s.to_lowercase().contains("token"));
    let q = ReviewQueue {
        repo: "r".into(),
        line: "main".into(),
        since_lsn: 0,
        path_prefix: None,
        max_lines: None,
        page: page(1),
    };
    assert!(!format!("{q:?}").to_lowercase().contains("token: some"));
}

/// A `RepoApi` that builds none of the v0.2 reads.
struct Old;

macro_rules! unused {
    ($($name:ident($req:ty) -> $out:ty;)*) => {
        $(async fn $name(&self, _: &CallContext, _: $req) -> graft_proto::error::ApiResult<$out> {
            unreachable!()
        })*
    };
}

impl RepoApi for Old {
    unused! {
        create_changeset(CreateChangeset) -> Created;
        get_changesets(GetChangesets) -> Changesets;
        resolve_ref(ResolveRef) -> RefView;
        list_refs(ListRefs) -> Vec<RefView>;
        update_ref(UpdateRef) -> RefView;
        stat(PathAt) -> EntryView;
        list_tree(ListTree) -> Entries;
        read_file(PathAt) -> FileContent;
        log(Log) -> History;
        is_ancestor(Pair) -> bool;
        merge_base(Pair) -> Option<ChangesetId>;
        diff(Diff) -> DiffResult;
        blame(Blame) -> BlameResult;
        rebase(Rebase) -> ChangesetId;
        revert(Revert) -> ChangesetId;
        land(Land) -> Landed;
        attest(Attest) -> ();
        watch(Watch) -> JournalPage;
    }
}

fn is_unimplemented(e: ApiError) -> bool {
    matches!(e, ApiError::Rejected { ref policy, .. } if policy == "unimplemented")
}

/// Accept 4: the new trait methods default to not-implemented.
#[test]
fn new_methods_default_to_unimplemented() {
    let cx = CallContext {
        principal: Principal {
            subject: "s".into(),
            kind: PrincipalKind::Agent,
            session: None,
            work_item: None,
            scopes: vec![],
        },
        request_id: "1".into(),
        deadline_ms: None,
    };
    let att = AttestV2::from(Attest {
        repo: "r".into(),
        changeset: cs(1),
        check: "c".into(),
        passed: true,
        url: String::new(),
    });
    assert!(is_unimplemented(
        block_on(Old.attest_v2(&cx, att)).unwrap_err()
    ));
    let ga = GetAttestations {
        repo: "r".into(),
        selector: AttestationSelector::ByChangeset(cs(1)),
        kinds: vec![],
        page: page(1),
    };
    assert!(is_unimplemented(
        block_on(Old.get_attestations(&cx, ga)).unwrap_err()
    ));
    let gl = GetLandChecks {
        repo: "r".into(),
        line: "main".into(),
        from_pos: 0,
        page: page(1),
    };
    assert!(is_unimplemented(
        block_on(Old.get_land_checks(&cx, gl)).unwrap_err()
    ));
    let rq = ReviewQueue {
        repo: "r".into(),
        line: "main".into(),
        since_lsn: 0,
        path_prefix: None,
        max_lines: None,
        page: page(1),
    };
    assert!(is_unimplemented(
        block_on(Old.review_queue(&cx, rq)).unwrap_err()
    ));
}
