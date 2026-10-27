//! `GetLineConfig` (v0.2, WI-2614): the request and answer round-trip, the
//! answer is bounded at decode, and the trait default is not-implemented.

use std::future::Future;
use std::pin::pin;
use std::task::{Context, Poll, Waker};

use graft_model::ChangesetId;
use graft_proto::types::*;
use graft_proto::{ApiError, CallContext, Principal, PrincipalKind, RepoApi, pb};
use prost::Message;

fn block_on<F: Future>(f: F) -> F::Output {
    let mut f = pin!(f);
    let mut cx = Context::from_waker(Waker::noop());
    loop {
        if let Poll::Ready(v) = f.as_mut().poll(&mut cx) {
            return v;
        }
    }
}

/// Accept 1: request and answer survive encode and decode.
#[test]
fn get_line_config_round_trips() {
    let req = GetLineConfig {
        repo: "r".into(),
        line: "backlog-1".into(),
    };
    let bytes = pb::GetLineConfigRequest::from(req.clone()).encode_to_vec();
    let back = GetLineConfig::from(pb::GetLineConfigRequest::decode(bytes.as_slice()).unwrap());
    assert_eq!(back, req);

    for config in [
        LineConfig {
            line: "main".into(),
            checks: LineChecksMode::PostLand,
            required: vec!["ci/fmt".into(), "ci/test".into()],
            retries: 3,
        },
        LineConfig {
            line: "dev".into(),
            checks: LineChecksMode::None,
            required: vec![],
            retries: 0,
        },
    ] {
        let bytes = pb::LineConfig::from(config.clone()).encode_to_vec();
        let back = LineConfig::try_from(pb::LineConfig::decode(bytes.as_slice()).unwrap()).unwrap();
        assert_eq!(back, config);
    }
}

#[test]
fn line_config_rejects_unknown_mode_and_too_many_checks() {
    let mut wire = pb::LineConfig::from(LineConfig {
        line: "main".into(),
        ..LineConfig::default()
    });
    wire.checks = pb::LineChecksMode::Unspecified as i32;
    assert_eq!(
        LineConfig::try_from(wire.clone()).unwrap_err().field,
        "checks"
    );
    wire.checks = 99;
    assert_eq!(
        LineConfig::try_from(wire.clone()).unwrap_err().field,
        "checks"
    );

    wire.checks = pb::LineChecksMode::PostLand as i32;
    wire.required = (0..=MAX_LINE_CHECKS).map(|i| format!("c{i}")).collect();
    assert_eq!(
        LineConfig::try_from(wire.clone()).unwrap_err().field,
        "required"
    );
    wire.required.pop();
    assert_eq!(
        LineConfig::try_from(wire).unwrap().required.len(),
        MAX_LINE_CHECKS
    );
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

#[test]
fn get_line_config_defaults_to_unimplemented() {
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
    let req = GetLineConfig {
        repo: "r".into(),
        line: "main".into(),
    };
    let err = block_on(Old.get_line_config(&cx, req)).unwrap_err();
    assert!(
        matches!(err, ApiError::Rejected { ref policy, .. } if policy == "unimplemented"),
        "{err:?}"
    );
}
