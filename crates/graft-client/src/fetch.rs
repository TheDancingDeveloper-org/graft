//! Named-object fetch (`docs/design.md` §7.3): breadth-first, pipelined,
//! resumable, verified.

use graft_model::{ChangesetId, RepoPath};
use graft_proto::CallContext;
use graft_proto::api::ObjectApi;
use graft_proto::error::ApiResult;

/// What a fetch did.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct FetchStats {
    /// Trees received.
    pub trees: u64,
    /// Blobs received.
    pub blobs: u64,
    /// Bytes received (encoded).
    pub bytes: u64,
    /// Requests sent.
    pub requests: u64,
}

/// Fetches every object under `prefixes` (all of the snapshot when empty)
/// that the local store lacks.
pub async fn fetch_snapshot<A: ObjectApi>(
    _api: &A,
    _cx: &CallContext,
    _repo: &str,
    _at: ChangesetId,
    _prefixes: &[RepoPath],
) -> ApiResult<FetchStats> {
    todo!("G1.11")
}
