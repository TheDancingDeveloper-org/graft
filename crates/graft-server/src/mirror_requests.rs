//! Mirror requests (D41, `docs/notes/mirror-service.md` §3.5): batch cuts,
//! followed-line rewinds and data purges, recorded by the server and
//! executed by the mirror service.
//!
//! - **Idempotency.** `(repo, kind, client_request_id)` names one request.
//!   A retry with the same key returns the request first recorded.
//! - **One live request per line.** A line has a batch slot (its
//!   `open_batch` pointer) and a rewind slot, each moved only by CAS in the
//!   transaction that records or finishes the request. A cut or a rewind
//!   refuses with the typed `BatchOpen` while either slot is held, so two
//!   cuts can't both win and a rewind never runs under an open batch.
//! - **Claims are fenced.** The service claims by CAS from `pending` to
//!   `claimed` under a `LeaseHeld` precondition. A claim whose epoch is no
//!   longer held returns to `pending`, on the next read or claim.
//! - **Expiry is lazy.** A live request past its deadline becomes
//!   `expired` when it is next read. An expired or failed cut releases the
//!   batch slot only if no PR was recorded, so expiry never orphans a PR.
//!   The PR branch is `graft/batch-<id>`, so the service can find a PR it
//!   created but never recorded.

use graft_auth::Scope;
use graft_model::{ChangesetId, ObjectId};
use graft_proto::types::followed_ref;
use graft_proto::{ApiError, Principal, pb};
use graft_store::{
    BatchState, LeaseRecord, LineSlot, MetaStore, MetaTxn, MirrorRequestKind, MirrorRequestQuery,
    MirrorRequestRecord, MirrorRequestResult, MirrorRequestState, Precondition, RepoId, TxnOutcome,
    Write,
};

use crate::mirrors::{
    MAX_PAGE, any_of, lease_caller, lease_lost, may_read, name, refused, repo_id, store,
};

/// The error code of a request the operator cancelled.
pub(crate) const CANCELLED: &str = "cancelled";

/// How long a request may stay live (§3.5).
pub(crate) const REQUEST_TIMEOUT_MS: i64 = 30 * 60_000;

/// The longest idempotency key accepted.
const MAX_CLIENT_ID: usize = 128;

/// The most excludes on a cut, and lands on a result.
const MAX_EXCLUDES: usize = 256;
const MAX_LANDS: usize = 1_000;

/// The longest PR URL accepted, and error code.
const MAX_URL: usize = 2048;
const MAX_CODE: usize = 64;

/// How many live requests one claim looks at.
const CLAIM_SCAN: u32 = 64;

/// How often a write retries after losing a race it can retry.
pub(crate) const MAX_TRIES: usize = 8;

/// The deterministic branch of a batch: the service looks for a PR on it
/// before creating one.
pub(crate) fn batch_branch(id: u64) -> String {
    format!("graft/batch-{id}")
}

pub(crate) fn busy() -> ApiError {
    ApiError::Unavailable {
        retry_after_ms: 100,
    }
}

fn not_found(repo: &str, id: u64) -> ApiError {
    ApiError::NotFound {
        what: format!("mirror request {id} of repository {repo}"),
    }
}

fn wrong_state(detail: impl Into<String>) -> ApiError {
    refused("mirror-request-state", detail)
}

fn client_id(s: &str) -> Result<String, ApiError> {
    if s.is_empty()
        || s.len() > MAX_CLIENT_ID
        || s.chars().any(|c| c.is_control() || c.is_whitespace())
    {
        return Err(refused(
            "mirror-field",
            format!("client_request_id is 1 to {MAX_CLIENT_ID} bytes with no whitespace"),
        ));
    }
    Ok(s.to_owned())
}

fn change_id(field: &str, b: &[u8]) -> Result<ChangesetId, ApiError> {
    <[u8; 32]>::try_from(b)
        .map(|a| ChangesetId(ObjectId(a)))
        .map_err(|_| refused("mirror-field", format!("{field} is not a 32-byte id")))
}

fn change_ids(field: &str, wire: &[Vec<u8>], max: usize) -> Result<Vec<ChangesetId>, ApiError> {
    if wire.len() > max {
        return Err(ApiError::LimitExceeded {
            limit: field.to_owned(),
            max: max as u64,
        });
    }
    wire.iter().map(|b| change_id(field, b)).collect()
}

fn bounded(field: &str, s: Option<String>, max: usize) -> Result<Option<String>, ApiError> {
    match s {
        Some(v) if v.is_empty() || v.len() > max || v.chars().any(char::is_control) => {
            Err(refused(
                "mirror-field",
                format!("{field} is 1 to {max} bytes with no control characters"),
            ))
        }
        v => Ok(v),
    }
}

/// A typed error code: lowercase letters, digits and `_`.
fn code(s: &str) -> Result<String, ApiError> {
    if s.is_empty()
        || s.len() > MAX_CODE
        || !s
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_')
    {
        return Err(refused(
            "mirror-field",
            format!("error_code is 1 to {MAX_CODE} of [a-z0-9_]"),
        ));
    }
    Ok(s.to_owned())
}

/// The slot a live request of `kind` holds.
fn slot_of(kind: MirrorRequestKind) -> Option<LineSlot> {
    match kind {
        MirrorRequestKind::CutBatch => Some(LineSlot::Batch),
        MirrorRequestKind::Rewind => Some(LineSlot::Rewind),
        _ => None,
    }
}

/// Whether `r`, as it now is, no longer holds its line's slot.
fn frees_slot(r: &MirrorRequestRecord) -> bool {
    match r.kind {
        MirrorRequestKind::CutBatch => {
            r.batch == Some(BatchState::Settled)
                || (!r.state.is_live()
                    && r.result.pr_number.is_none()
                    && r.batch == Some(BatchState::Requested))
        }
        MirrorRequestKind::Rewind => !r.state.is_live(),
        _ => false,
    }
}

/// A finished cut that recorded a PR has an open batch.
fn batch_after(r: &MirrorRequestRecord) -> Option<BatchState> {
    match r.batch {
        Some(BatchState::Requested) if r.result.pr_number.is_some() => Some(BatchState::Open),
        Some(BatchState::Requested) if r.state == MirrorRequestState::Done => {
            Some(BatchState::Settled)
        }
        b => b,
    }
}

/// Whether `r` is a cut a service has claimed that recorded no PR: the
/// service may have opened the PR on `batch_branch(r.id)` and crashed
/// before recording it. Such a cut never expires; it stays claimable until
/// a service runs it again, finds the PR on its branch and records it (or
/// opens it, or fails it). Expiring it would free the line's slot, and the
/// next cut, on another branch, would open a second batch PR.
fn may_have_opened_a_pr(r: &MirrorRequestRecord) -> bool {
    r.kind == MirrorRequestKind::CutBatch
        && r.claimed_at_ms.is_some()
        && r.result.pr_number.is_none()
}

/// What `r` becomes when read at `now_ms`: expired past its deadline
/// (unless [`may_have_opened_a_pr`]), or back to `pending` when its claim's
/// epoch is no longer held.
/// `None` when nothing changes.
fn lazily(
    r: &MirrorRequestRecord,
    lease: Option<&LeaseRecord>,
    now_ms: i64,
) -> Option<MirrorRequestRecord> {
    if !r.state.is_live() {
        return None;
    }
    let mut next = r.clone();
    let claim_held = r
        .claimed_by_epoch
        .is_some_and(|e| lease.is_some_and(|l| l.held(e, now_ms)));
    if now_ms >= r.deadline_ms && !may_have_opened_a_pr(r) {
        next.state = MirrorRequestState::Expired;
        next.batch = batch_after(&next);
    } else if r.state == MirrorRequestState::Claimed && !claim_held {
        // `claimed_at_ms` stays: it records that a service ran the request.
        next.state = MirrorRequestState::Pending;
        next.claimed_by_epoch = None;
    } else {
        return None;
    }
    next.version += 1;
    next.updated_at_ms = now_ms;
    Some(next)
}

/// A transaction writing `next` over `prev` by CAS on its version, and
/// clearing its slot by CAS when it now frees it.
async fn rewrite<M: MetaStore>(
    meta: &M,
    id: RepoId,
    prev: &MirrorRequestRecord,
    next: MirrorRequestRecord,
    fence: Option<Precondition>,
) -> Result<MetaTxn, ApiError> {
    let mut txn = MetaTxn {
        preconditions: vec![Precondition::MirrorRequestVersionIs {
            id: prev.id,
            version: Some(prev.version),
        }],
        writes: Vec::new(),
    };
    txn.preconditions.extend(fence);
    if let Some(slot) = slot_of(next.kind)
        && frees_slot(&next)
        && meta.line_slot(id, &next.line, slot).await.map_err(store)? == Some(next.id)
    {
        txn.preconditions.push(Precondition::LineSlotIs {
            line: next.line.clone(),
            slot,
            request: Some(next.id),
        });
        txn.writes.push(Write::SetLineSlot {
            line: next.line.clone(),
            slot,
            request: None,
        });
    }
    txn.writes.push(Write::PutMirrorRequest(next));
    Ok(txn)
}

/// Re-reads request `rid`, or `NotFound`.
async fn reread<M: MetaStore>(
    meta: &M,
    id: RepoId,
    repo: &str,
    rid: u64,
) -> Result<MirrorRequestRecord, ApiError> {
    meta.get_mirror_request(id, rid)
        .await
        .map_err(store)?
        .ok_or_else(|| not_found(repo, rid))
}

/// Applies the lazy transitions to `r` and commits them by CAS. A lost race
/// re-reads and tries again.
async fn settle<M: MetaStore>(
    meta: &M,
    id: RepoId,
    repo: &str,
    mut r: MirrorRequestRecord,
    now_ms: i64,
) -> Result<MirrorRequestRecord, ApiError> {
    for _ in 0..MAX_TRIES {
        let lease = meta.get_lease(id).await.map_err(store)?;
        let Some(next) = lazily(&r, lease.as_ref(), now_ms) else {
            return Ok(r);
        };
        let txn = rewrite(meta, id, &r, next.clone(), None).await?;
        match meta.commit(id, txn).await.map_err(store)? {
            TxnOutcome::Committed(_) => return Ok(next),
            TxnOutcome::PreconditionFailed(_) => r = reread(meta, id, repo, r.id).await?,
        }
    }
    Err(busy())
}

/// What a new request carries.
struct Draft {
    kind: MirrorRequestKind,
    line: String,
    client_request_id: String,
    base: Option<ChangesetId>,
    excludes: Vec<ChangesetId>,
    upstream: Option<String>,
}

/// Records `draft`, or returns the request its key already names. A cut or
/// rewind needs both of its line's slots clear.
async fn create<M: MetaStore>(
    meta: &M,
    p: &Principal,
    repo: &str,
    now_ms: i64,
    draft: Draft,
) -> Result<pb::MirrorRequest, ApiError> {
    let id = repo_id(meta, repo).await?;
    for _ in 0..MAX_TRIES {
        if let Some(r) = meta
            .find_mirror_request(id, draft.kind, &draft.client_request_id)
            .await
            .map_err(store)?
        {
            let r = settle(meta, id, repo, r, now_ms).await?;
            return Ok(view(repo, r));
        }
        let mut pre = vec![Precondition::MirrorRequestKeyIs {
            kind: draft.kind,
            client_request_id: draft.client_request_id.clone(),
            id: None,
        }];
        let mut writes = Vec::new();
        let rid = meta.last_mirror_request_id(id).await.map_err(store)? + 1;
        if let Some(own) = slot_of(draft.kind) {
            for slot in [LineSlot::Batch, LineSlot::Rewind] {
                if let Some(holder) = meta.line_slot(id, &draft.line, slot).await.map_err(store)? {
                    // The holder may have expired since it was last read.
                    if let Some(h) = meta.get_mirror_request(id, holder).await.map_err(store)? {
                        settle(meta, id, repo, h, now_ms).await?;
                    }
                    if meta
                        .line_slot(id, &draft.line, slot)
                        .await
                        .map_err(store)?
                        .is_some()
                    {
                        return Err(ApiError::BatchOpen {
                            repo: repo.to_owned(),
                            line: draft.line.clone(),
                            request: holder,
                        });
                    }
                }
                pre.push(Precondition::LineSlotIs {
                    line: draft.line.clone(),
                    slot,
                    request: None,
                });
            }
            writes.push(Write::SetLineSlot {
                line: draft.line.clone(),
                slot: own,
                request: Some(rid),
            });
        }
        pre.push(Precondition::MirrorRequestVersionIs {
            id: rid,
            version: None,
        });
        let record = MirrorRequestRecord {
            id: rid,
            line: draft.line.clone(),
            kind: draft.kind,
            client_request_id: draft.client_request_id.clone(),
            base: draft.base,
            excludes: draft.excludes.clone(),
            upstream: draft.upstream.clone(),
            state: MirrorRequestState::Pending,
            batch: (draft.kind == MirrorRequestKind::CutBatch).then_some(BatchState::Requested),
            claimed_by_epoch: None,
            claimed_at_ms: None,
            deadline_ms: now_ms.saturating_add(REQUEST_TIMEOUT_MS),
            result: MirrorRequestResult::default(),
            requested_by: p.subject.clone(),
            created_at_ms: now_ms,
            updated_at_ms: now_ms,
            version: 1,
        };
        writes.push(Write::PutMirrorRequest(record.clone()));
        let txn = MetaTxn {
            preconditions: pre,
            writes,
        };
        // A lost race re-reads: the key, a slot or the id moved.
        if let TxnOutcome::Committed(_) = meta.commit(id, txn).await.map_err(store)? {
            return Ok(view(repo, record));
        }
    }
    Err(busy())
}

fn kind_wire(k: MirrorRequestKind) -> pb::MirrorRequestKind {
    match k {
        MirrorRequestKind::CutBatch => pb::MirrorRequestKind::CutBatch,
        MirrorRequestKind::SettleBatch => pb::MirrorRequestKind::SettleBatch,
        MirrorRequestKind::Exclude => pb::MirrorRequestKind::Exclude,
        MirrorRequestKind::Rewind => pb::MirrorRequestKind::Rewind,
        MirrorRequestKind::Purge => pb::MirrorRequestKind::Purge,
    }
}

fn state_wire(s: MirrorRequestState) -> pb::MirrorRequestState {
    match s {
        MirrorRequestState::Pending => pb::MirrorRequestState::Pending,
        MirrorRequestState::Claimed => pb::MirrorRequestState::Claimed,
        MirrorRequestState::Done => pb::MirrorRequestState::Done,
        MirrorRequestState::Failed => pb::MirrorRequestState::Failed,
        MirrorRequestState::Expired => pb::MirrorRequestState::Expired,
    }
}

fn batch_wire(b: Option<BatchState>) -> pb::BatchState {
    match b {
        None => pb::BatchState::Unspecified,
        Some(BatchState::Requested) => pb::BatchState::Requested,
        Some(BatchState::Open) => pb::BatchState::Open,
        Some(BatchState::Sealed) => pb::BatchState::Sealed,
        Some(BatchState::Settled) => pb::BatchState::Settled,
    }
}

/// The wire form of a request.
pub(crate) fn view(repo: &str, r: MirrorRequestRecord) -> pb::MirrorRequest {
    let ids = |v: Vec<ChangesetId>| v.into_iter().map(|c| c.0.0.to_vec()).collect();
    pb::MirrorRequest {
        repo: repo.to_owned(),
        id: r.id,
        line: r.line,
        kind: kind_wire(r.kind) as i32,
        client_request_id: r.client_request_id,
        base: r.base.map(|b| b.0.0.to_vec()),
        excludes: ids(r.excludes),
        upstream: r.upstream,
        state: state_wire(r.state) as i32,
        batch_state: batch_wire(r.batch) as i32,
        claimed_by_epoch: r.claimed_by_epoch,
        claimed_at_ms: r.claimed_at_ms,
        deadline_ms: r.deadline_ms,
        result: Some(pb::MirrorRequestOutcome {
            pr_number: r.result.pr_number,
            pr_url: r.result.pr_url,
            branch: r.result.branch,
            lands: ids(r.result.lands),
            error_code: r.result.error_code,
        }),
        requested_by: r.requested_by,
        created_at_ms: r.created_at_ms,
        updated_at_ms: r.updated_at_ms,
    }
}

/// `CutBatch` (§3.5), under `repo:<name>:admin`.
pub(crate) async fn cut_batch<M: MetaStore>(
    meta: &M,
    p: &Principal,
    now_ms: i64,
    req: pb::CutBatchRequest,
) -> Result<pb::MirrorRequest, ApiError> {
    any_of(
        p,
        &[Scope::Admin {
            repo: req.repo.clone(),
        }],
    )?;
    let draft = Draft {
        kind: MirrorRequestKind::CutBatch,
        line: name("line", &req.line)?,
        client_request_id: client_id(&req.client_request_id)?,
        base: req
            .base
            .as_deref()
            .map(|b| change_id("base", b))
            .transpose()?,
        excludes: change_ids("excludes", &req.excludes, MAX_EXCLUDES)?,
        upstream: None,
    };
    create(meta, p, &req.repo, now_ms, draft).await
}

/// `RewindFollowed` (§3.5, WI-3142), under `mirror:bind`: only on a
/// followed line, and never while a batch holds it.
pub(crate) async fn rewind_followed<M: MetaStore>(
    meta: &M,
    p: &Principal,
    now_ms: i64,
    req: pb::RewindFollowedRequest,
) -> Result<pb::MirrorRequest, ApiError> {
    any_of(p, &[Scope::MirrorBind])?;
    let line = name("line", &req.line)?;
    let client_request_id = client_id(&req.client_request_id)?;
    let id = repo_id(meta, &req.repo).await?;
    if meta
        .get_ref(id, &followed_ref(&line))
        .await
        .map_err(store)?
        .is_none()
    {
        return Err(refused(
            "mirror-not-followed",
            format!("line {line} is not followed"),
        ));
    }
    let draft = Draft {
        kind: MirrorRequestKind::Rewind,
        line,
        client_request_id,
        base: None,
        excludes: Vec::new(),
        upstream: None,
    };
    create(meta, p, &req.repo, now_ms, draft).await
}

/// `PurgeMirror` (§3.2): records a purge of an upstream's data. The caller
/// has checked the grant and that the row is removed or disabled.
pub(crate) async fn record_purge<M: MetaStore>(
    meta: &M,
    p: &Principal,
    now_ms: i64,
    repo: &str,
    upstream: String,
    client_request_id: &str,
) -> Result<pb::MirrorRequest, ApiError> {
    let draft = Draft {
        kind: MirrorRequestKind::Purge,
        line: String::new(),
        client_request_id: client_id(client_request_id)?,
        base: None,
        excludes: Vec::new(),
        upstream: Some(upstream),
    };
    create(meta, p, repo, now_ms, draft).await
}

/// The writes that cancel every live purge of `upstream`, for the
/// transaction that re-enables its row: a purge recorded before a
/// re-enable never runs after it, so a later disable needs a fresh purge.
/// Each write is fenced by CAS on the request's version.
pub(crate) async fn cancel_purges<M: MetaStore>(
    meta: &M,
    id: RepoId,
    upstream: &str,
    now_ms: i64,
) -> Result<MetaTxn, ApiError> {
    let mut txn = MetaTxn {
        preconditions: Vec::new(),
        writes: Vec::new(),
    };
    let mut after = None;
    loop {
        let rows = meta
            .list_mirror_requests(
                id,
                MirrorRequestQuery {
                    kind: Some(MirrorRequestKind::Purge),
                    line: None,
                    states: vec![MirrorRequestState::Pending, MirrorRequestState::Claimed],
                    after,
                    oldest_first: true,
                    limit: MAX_PAGE,
                },
            )
            .await
            .map_err(store)?;
        let full = rows.len() == MAX_PAGE as usize;
        after = rows.last().map(|r| r.id);
        for r in rows {
            if r.upstream.as_deref() != Some(upstream) || !r.state.is_live() {
                continue;
            }
            let mut next = r.clone();
            next.state = MirrorRequestState::Failed;
            next.result.error_code = Some(CANCELLED.to_owned());
            next.version = r.version + 1;
            next.updated_at_ms = now_ms;
            let part = rewrite(meta, id, &r, next, None).await?;
            txn.preconditions.extend(part.preconditions);
            txn.writes.extend(part.writes);
        }
        if !full {
            return Ok(txn);
        }
    }
}

/// `GetBatch`: the line's open batch, if any.
pub(crate) async fn get_batch<M: MetaStore>(
    meta: &M,
    p: &Principal,
    now_ms: i64,
    req: pb::BatchKey,
) -> Result<pb::BatchView, ApiError> {
    may_read(p, &req.repo)?;
    let id = repo_id(meta, &req.repo).await?;
    let Some(rid) = meta
        .line_slot(id, &req.line, LineSlot::Batch)
        .await
        .map_err(store)?
    else {
        return Ok(pb::BatchView { batch: None });
    };
    let r = reread(meta, id, &req.repo, rid).await?;
    let r = settle(meta, id, &req.repo, r, now_ms).await?;
    let batch = (!frees_slot(&r)).then(|| view(&req.repo, r));
    Ok(pb::BatchView { batch })
}

/// `ListBatches`: a repository's cuts, newest first, paginated.
pub(crate) async fn list_batches<M: MetaStore>(
    meta: &M,
    p: &Principal,
    now_ms: i64,
    req: pb::ListBatchesRequest,
) -> Result<pb::BatchPage, ApiError> {
    may_read(p, &req.repo)?;
    let id = repo_id(meta, &req.repo).await?;
    let after = if req.page_token.is_empty() {
        None
    } else {
        Some(
            req.page_token
                .parse::<u64>()
                .map_err(|_| refused("mirror-field", "page_token is not one this server issued"))?,
        )
    };
    let limit = if req.limit == 0 {
        MAX_PAGE
    } else {
        req.limit.min(MAX_PAGE)
    };
    let rows = meta
        .list_mirror_requests(
            id,
            MirrorRequestQuery {
                kind: Some(MirrorRequestKind::CutBatch),
                line: req.line,
                states: Vec::new(),
                after,
                oldest_first: false,
                limit,
            },
        )
        .await
        .map_err(store)?;
    let next_page_token = if rows.len() == limit as usize {
        rows.last().map(|r| r.id.to_string()).unwrap_or_default()
    } else {
        String::new()
    };
    let mut batches = Vec::with_capacity(rows.len());
    for r in rows {
        batches.push(view(
            &req.repo,
            settle(meta, id, &req.repo, r, now_ms).await?,
        ));
    }
    Ok(pb::BatchPage {
        batches,
        next_page_token,
    })
}

/// `GetRequest`: one request's state and result.
pub(crate) async fn get_request<M: MetaStore>(
    meta: &M,
    p: &Principal,
    now_ms: i64,
    req: pb::MirrorRequestKey,
) -> Result<pb::MirrorRequest, ApiError> {
    may_read(p, &req.repo)?;
    let id = repo_id(meta, &req.repo).await?;
    let r = reread(meta, id, &req.repo, req.id).await?;
    Ok(view(
        &req.repo,
        settle(meta, id, &req.repo, r, now_ms).await?,
    ))
}

/// The lease is held under `epoch`, or `LeaseLost`.
async fn holds<M: MetaStore>(
    meta: &M,
    id: RepoId,
    repo: &str,
    epoch: u64,
    now_ms: i64,
) -> Result<(), ApiError> {
    if meta
        .get_lease(id)
        .await
        .map_err(store)?
        .is_some_and(|l| l.held(epoch, now_ms))
    {
        Ok(())
    } else {
        Err(lease_lost(repo))
    }
}

/// `ClaimRequest`: the mirror service claims the oldest pending request
/// under its lease epoch. A request claimed under an epoch no longer held
/// is pending again, so it is claimable.
pub(crate) async fn claim<M: MetaStore>(
    meta: &M,
    p: &Principal,
    now_ms: i64,
    req: pb::ClaimRequestRequest,
) -> Result<pb::ClaimRequestResult, ApiError> {
    lease_caller(p, &req.repo)?;
    let id = repo_id(meta, &req.repo).await?;
    holds(meta, id, &req.repo, req.epoch, now_ms).await?;
    let rows = meta
        .list_mirror_requests(
            id,
            MirrorRequestQuery {
                kind: None,
                line: None,
                states: vec![MirrorRequestState::Pending, MirrorRequestState::Claimed],
                after: None,
                oldest_first: true,
                limit: CLAIM_SCAN,
            },
        )
        .await
        .map_err(store)?;
    for r in rows {
        let mut r = settle(meta, id, &req.repo, r, now_ms).await?;
        for _ in 0..MAX_TRIES {
            if r.state != MirrorRequestState::Pending {
                break;
            }
            let mut next = r.clone();
            next.state = MirrorRequestState::Claimed;
            next.claimed_by_epoch = Some(req.epoch);
            next.claimed_at_ms = Some(now_ms);
            next.version += 1;
            next.updated_at_ms = now_ms;
            let fence = Precondition::LeaseHeld {
                epoch: req.epoch,
                now_ms,
            };
            let txn = rewrite(meta, id, &r, next.clone(), Some(fence)).await?;
            match meta.commit(id, txn).await.map_err(store)? {
                TxnOutcome::Committed(_) => {
                    return Ok(pb::ClaimRequestResult {
                        request: Some(view(&req.repo, next)),
                    });
                }
                TxnOutcome::PreconditionFailed(_) => {
                    holds(meta, id, &req.repo, req.epoch, now_ms).await?;
                    r = reread(meta, id, &req.repo, r.id).await?;
                }
            }
        }
    }
    Ok(pb::ClaimRequestResult { request: None })
}

/// Reads request `rid` for a fenced service write, after the lazy
/// transitions: the caller holds the lease under `epoch`.
async fn fenced<M: MetaStore>(
    meta: &M,
    p: &Principal,
    now_ms: i64,
    repo: &str,
    rid: u64,
    epoch: u64,
) -> Result<(RepoId, MirrorRequestRecord), ApiError> {
    lease_caller(p, repo)?;
    let id = repo_id(meta, repo).await?;
    holds(meta, id, repo, epoch, now_ms).await?;
    let r = reread(meta, id, repo, rid).await?;
    Ok((id, settle(meta, id, repo, r, now_ms).await?))
}

/// Commits `change(r)` over `r` under the lease fence, re-reading on a lost
/// race. `change` refuses a request in the wrong state.
async fn apply<M, F>(
    meta: &M,
    p: &Principal,
    now_ms: i64,
    repo: &str,
    rid: u64,
    epoch: u64,
    change: F,
) -> Result<pb::MirrorRequest, ApiError>
where
    M: MetaStore,
    F: Fn(&MirrorRequestRecord) -> Result<MirrorRequestRecord, ApiError>,
{
    let (id, mut r) = fenced(meta, p, now_ms, repo, rid, epoch).await?;
    for _ in 0..MAX_TRIES {
        let mut next = change(&r)?;
        next.version = r.version + 1;
        next.updated_at_ms = now_ms;
        let fence = Precondition::LeaseHeld { epoch, now_ms };
        let txn = rewrite(meta, id, &r, next.clone(), Some(fence)).await?;
        match meta.commit(id, txn).await.map_err(store)? {
            TxnOutcome::Committed(_) => return Ok(view(repo, next)),
            TxnOutcome::PreconditionFailed(_) => {
                holds(meta, id, repo, epoch, now_ms).await?;
                let fresh = reread(meta, id, repo, rid).await?;
                r = settle(meta, id, repo, fresh, now_ms).await?;
            }
        }
    }
    Err(busy())
}

/// The request is claimed under `epoch`.
fn claimed_by(r: &MirrorRequestRecord, epoch: u64) -> Result<(), ApiError> {
    if r.state == MirrorRequestState::Claimed && r.claimed_by_epoch == Some(epoch) {
        Ok(())
    } else {
        Err(wrong_state(format!(
            "request {} is {}, not claimed under epoch {epoch}",
            r.id,
            r.state.as_str()
        )))
    }
}

fn outcome(
    id: u64,
    kind: MirrorRequestKind,
    wire: pb::MirrorRequestOutcome,
) -> Result<MirrorRequestResult, ApiError> {
    let branch = bounded("result.branch", wire.branch, 256)?;
    if kind == MirrorRequestKind::CutBatch
        && branch.as_ref().is_some_and(|b| *b != batch_branch(id))
    {
        return Err(refused(
            "mirror-field",
            format!("a batch's branch is {}", batch_branch(id)),
        ));
    }
    Ok(MirrorRequestResult {
        pr_number: wire.pr_number,
        pr_url: bounded("result.pr_url", wire.pr_url, MAX_URL)?,
        branch,
        lands: change_ids("result.lands", &wire.lands, MAX_LANDS)?,
        error_code: wire.error_code.as_deref().map(code).transpose()?,
    })
}

/// `CompleteRequest`: records a claimed request's result and, with `done`,
/// finishes it. A finished cut with a PR is an open batch; one without is
/// settled at once, freeing the line.
pub(crate) async fn complete<M: MetaStore>(
    meta: &M,
    p: &Principal,
    now_ms: i64,
    req: pb::CompleteRequestRequest,
) -> Result<pb::MirrorRequest, ApiError> {
    let done = req.done;
    let wire = req.result.unwrap_or_default();
    apply(meta, p, now_ms, &req.repo, req.id, req.epoch, |r| {
        claimed_by(r, req.epoch)?;
        let mut next = r.clone();
        next.result = outcome(r.id, r.kind, wire.clone())?;
        if done {
            next.state = MirrorRequestState::Done;
            next.batch = batch_after(&next);
        }
        Ok(next)
    })
    .await
}

/// `FailRequest`: finishes a claimed request with a typed error. A failed
/// cut that recorded a PR keeps its batch open.
pub(crate) async fn fail<M: MetaStore>(
    meta: &M,
    p: &Principal,
    now_ms: i64,
    req: pb::FailRequestRequest,
) -> Result<pb::MirrorRequest, ApiError> {
    let error = code(&req.error_code)?;
    apply(meta, p, now_ms, &req.repo, req.id, req.epoch, |r| {
        claimed_by(r, req.epoch)?;
        let mut next = r.clone();
        next.state = MirrorRequestState::Failed;
        next.result.error_code = Some(error.clone());
        next.batch = batch_after(&next);
        Ok(next)
    })
    .await
}

/// `SetBatchState`: moves a batch with a PR `open → sealed → settled`.
/// Settling clears the line's pointer in the same transaction.
pub(crate) async fn set_batch_state<M: MetaStore>(
    meta: &M,
    p: &Principal,
    now_ms: i64,
    req: pb::SetBatchStateRequest,
) -> Result<pb::MirrorRequest, ApiError> {
    let to = match pb::BatchState::try_from(req.state) {
        Ok(pb::BatchState::Sealed) => BatchState::Sealed,
        Ok(pb::BatchState::Settled) => BatchState::Settled,
        _ => {
            return Err(refused(
                "mirror-field",
                "a batch moves only to sealed or settled",
            ));
        }
    };
    apply(meta, p, now_ms, &req.repo, req.id, req.epoch, |r| {
        let ok = matches!(
            (r.batch, to),
            (Some(BatchState::Open), _) | (Some(BatchState::Sealed), BatchState::Settled)
        );
        if r.kind != MirrorRequestKind::CutBatch || !ok {
            return Err(wrong_state(format!(
                "request {} has no open or sealed batch to move to {}",
                r.id,
                to.as_str()
            )));
        }
        let mut next = r.clone();
        next.batch = Some(to);
        Ok(next)
    })
    .await
}

/// `CancelRequest`, under `mirror:bind`: the operator's way out of a
/// request no service will finish. A live request fails with the code
/// `cancelled`, and a cut's batch is settled, so its line is free for the
/// next cut. No lease fence: the operator holds none. A service still
/// running the request is refused when it next completes or fails it.
pub(crate) async fn cancel<M: MetaStore>(
    meta: &M,
    p: &Principal,
    now_ms: i64,
    req: pb::CancelRequestRequest,
) -> Result<pb::MirrorRequest, ApiError> {
    any_of(p, &[Scope::MirrorBind])?;
    let id = repo_id(meta, &req.repo).await?;
    for _ in 0..MAX_TRIES {
        let r = reread(meta, id, &req.repo, req.id).await?;
        let r = settle(meta, id, &req.repo, r, now_ms).await?;
        let holds_batch = matches!(
            r.batch,
            Some(BatchState::Requested | BatchState::Open | BatchState::Sealed)
        ) && !frees_slot(&r);
        if !r.state.is_live() && !holds_batch {
            return Err(wrong_state(format!(
                "request {} is {} and holds no line",
                r.id,
                r.state.as_str()
            )));
        }
        let mut next = r.clone();
        if r.state.is_live() {
            next.state = MirrorRequestState::Failed;
            next.result.error_code = Some(CANCELLED.to_owned());
        }
        if r.kind == MirrorRequestKind::CutBatch {
            next.batch = Some(BatchState::Settled);
        }
        next.version = r.version + 1;
        next.updated_at_ms = now_ms;
        let txn = rewrite(meta, id, &r, next.clone(), None).await?;
        if let TxnOutcome::Committed(_) = meta.commit(id, txn).await.map_err(store)? {
            return Ok(view(&req.repo, next));
        }
    }
    Err(busy())
}

#[cfg(test)]
mod tests;
