//! WI-2614: post-land retries come from `GetLineConfig`. Each case fails
//! if that call is skipped: the line's count, the flag, or the fallback
//! would not be the value asserted.

use std::sync::atomic::{AtomicUsize, Ordering};

use graft_attester::{EffectiveRetries, RetriesSource, call_context, effective_retries};
use graft_proto::api::RepoApi;
use graft_proto::types::*;
use graft_proto::{ApiError, CallContext};

macro_rules! rest {
    ($($name:ident($req:ty) -> $out:ty;)*) => {
        $(
            async fn $name(
                &self,
                _cx: &CallContext,
                _req: $req,
            ) -> graft_proto::error::ApiResult<$out> {
                Err(ApiError::unimplemented("not this test"))
            }
        )*
    };
}

struct Line {
    retries: u32,
    calls: AtomicUsize,
}

impl RepoApi for Line {
    rest! {
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
        merge_base(Pair) -> Option<graft_model::ChangesetId>;
        diff(Diff) -> DiffResult;
        blame(Blame) -> BlameResult;
        rebase(Rebase) -> graft_model::ChangesetId;
        revert(Revert) -> graft_model::ChangesetId;
        land(Land) -> Landed;
        attest(Attest) -> ();
        watch(Watch) -> JournalPage;
    }
    async fn get_line_config(
        &self,
        _cx: &CallContext,
        req: GetLineConfig,
    ) -> graft_proto::error::ApiResult<LineConfig> {
        assert_eq!(req.repo, "graft");
        assert_eq!(req.line, "integration");
        self.calls.fetch_add(1, Ordering::Relaxed);
        Ok(LineConfig {
            line: req.line,
            checks: LineChecksMode::PostLand,
            required: vec!["ci/test".to_owned()],
            retries: self.retries,
        })
    }
}

struct OldServer {
    calls: AtomicUsize,
}

impl RepoApi for OldServer {
    rest! {
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
        merge_base(Pair) -> Option<graft_model::ChangesetId>;
        diff(Diff) -> DiffResult;
        blame(Blame) -> BlameResult;
        rebase(Rebase) -> graft_model::ChangesetId;
        revert(Revert) -> graft_model::ChangesetId;
        land(Land) -> Landed;
        attest(Attest) -> ();
        watch(Watch) -> JournalPage;
    }
    async fn get_line_config(
        &self,
        _cx: &CallContext,
        _req: GetLineConfig,
    ) -> graft_proto::error::ApiResult<LineConfig> {
        self.calls.fetch_add(1, Ordering::Relaxed);
        Err(ApiError::unimplemented("GetLineConfig"))
    }
}

struct Missing;

impl RepoApi for Missing {
    rest! {
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
        merge_base(Pair) -> Option<graft_model::ChangesetId>;
        diff(Diff) -> DiffResult;
        blame(Blame) -> BlameResult;
        rebase(Rebase) -> graft_model::ChangesetId;
        revert(Revert) -> graft_model::ChangesetId;
        land(Land) -> Landed;
        attest(Attest) -> ();
        watch(Watch) -> JournalPage;
        get_line_config(GetLineConfig) -> LineConfig;
    }
}

#[tokio::test]
async fn retries_come_from_the_line() {
    let api = Line {
        retries: 4,
        calls: AtomicUsize::new(0),
    };
    let got = effective_retries(&api, &call_context(), "graft", "integration", None)
        .await
        .expect("line");
    assert_eq!(
        got,
        EffectiveRetries {
            retries: 4,
            source: RetriesSource::Line,
        }
    );
    assert_eq!(api.calls.load(Ordering::Relaxed), 1);
}

#[tokio::test]
async fn the_flag_overrides_the_line() {
    let api = Line {
        retries: 4,
        calls: AtomicUsize::new(0),
    };
    let got = effective_retries(&api, &call_context(), "graft", "integration", Some(1))
        .await
        .expect("flag");
    assert_eq!(
        got,
        EffectiveRetries {
            retries: 1,
            source: RetriesSource::Flag,
        }
    );
    assert_eq!(api.calls.load(Ordering::Relaxed), 1);
}

#[tokio::test]
async fn an_unimplemented_server_falls_back() {
    let api = OldServer {
        calls: AtomicUsize::new(0),
    };
    let none = effective_retries(&api, &call_context(), "graft", "integration", None)
        .await
        .expect("fallback");
    assert_eq!(
        none,
        EffectiveRetries {
            retries: 0,
            source: RetriesSource::Fallback,
        }
    );
    let flagged = effective_retries(&api, &call_context(), "graft", "integration", Some(2))
        .await
        .expect("flag fallback");
    assert_eq!(
        flagged,
        EffectiveRetries {
            retries: 2,
            source: RetriesSource::Fallback,
        }
    );
    assert_eq!(api.calls.load(Ordering::Relaxed), 2);
}

#[tokio::test]
async fn another_api_error_is_returned() {
    // The trait default is unimplemented, so it is the fallback, not an error.
    let got = effective_retries(&Missing, &call_context(), "graft", "integration", None)
        .await
        .expect("trait default falls back");
    assert_eq!(
        got,
        EffectiveRetries {
            retries: 0,
            source: RetriesSource::Fallback,
        }
    );
    let err = effective_retries(&NotFound, &call_context(), "graft", "gone", Some(3))
        .await
        .expect_err("not found");
    assert!(matches!(err, ApiError::NotFound { .. }));
}

struct NotFound;

impl RepoApi for NotFound {
    rest! {
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
        merge_base(Pair) -> Option<graft_model::ChangesetId>;
        diff(Diff) -> DiffResult;
        blame(Blame) -> BlameResult;
        rebase(Rebase) -> graft_model::ChangesetId;
        revert(Revert) -> graft_model::ChangesetId;
        land(Land) -> Landed;
        attest(Attest) -> ();
        watch(Watch) -> JournalPage;
    }
    async fn get_line_config(
        &self,
        _cx: &CallContext,
        req: GetLineConfig,
    ) -> graft_proto::error::ApiResult<LineConfig> {
        Err(ApiError::NotFound {
            what: format!("line {}", req.line),
        })
    }
}
