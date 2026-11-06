//! Mirror requests (§3.5). Each scenario runs on the in-memory fake and on
//! SQLite, which must agree; the races run on SQLite across threads.

use super::*;
use crate::mirrors::acquire_lease;
use graft_proto::PrincipalKind;
use graft_store::memory::MemoryStore;
use graft_store::{RefKind, RefRecord};
use graft_store_sqlite::{SqliteConfig, SqliteMetaStore};
use std::sync::Arc;

const MIN: i64 = 60_000;

fn who(subject: &str, scopes: &[&str]) -> Principal {
    Principal {
        subject: subject.into(),
        kind: PrincipalKind::Human,
        session: None,
        work_item: None,
        scopes: scopes.iter().map(|s| s.to_string()).collect(),
    }
}

fn admin() -> Principal {
    who("admin", &["repo:r:admin"])
}

fn bind() -> Principal {
    who("op", &["mirror:bind"])
}

fn service() -> Principal {
    Principal {
        kind: PrincipalKind::Mirror,
        ..who("mirror", &["repo:*:mirror"])
    }
}

fn cut(line: &str, key: &str) -> pb::CutBatchRequest {
    pb::CutBatchRequest {
        repo: "r".into(),
        line: line.into(),
        base: None,
        excludes: Vec::new(),
        client_request_id: key.into(),
    }
}

fn rewind(line: &str, key: &str) -> pb::RewindFollowedRequest {
    pb::RewindFollowedRequest {
        repo: "r".into(),
        line: line.into(),
        client_request_id: key.into(),
    }
}

fn claim_req(epoch: u64) -> pb::ClaimRequestRequest {
    pb::ClaimRequestRequest {
        repo: "r".into(),
        epoch,
    }
}

fn with_pr(id: u64, epoch: u64, done: bool) -> pb::CompleteRequestRequest {
    pb::CompleteRequestRequest {
        repo: "r".into(),
        id,
        epoch,
        result: Some(pb::MirrorRequestOutcome {
            pr_number: Some(41),
            pr_url: Some("https://github.com/o/r/pull/41".into()),
            branch: Some(batch_branch(id)),
            lands: vec![vec![7; 32]],
            error_code: None,
        }),
        done,
    }
}

fn key(id: u64) -> pb::MirrorRequestKey {
    pb::MirrorRequestKey {
        repo: "r".into(),
        id,
    }
}

fn batch_key(line: &str) -> pb::BatchKey {
    pb::BatchKey {
        repo: "r".into(),
        line: line.into(),
    }
}

fn state(r: &pb::MirrorRequest) -> pb::MirrorRequestState {
    pb::MirrorRequestState::try_from(r.state).expect("state")
}

fn batch(r: &pb::MirrorRequest) -> pb::BatchState {
    pb::BatchState::try_from(r.batch_state).expect("batch state")
}

fn held_by(r: Result<impl std::fmt::Debug, ApiError>, request: u64) {
    match r {
        Err(ApiError::BatchOpen {
            repo,
            line,
            request: got,
        }) => {
            assert_eq!((repo.as_str(), line.as_str(), got), ("r", "main", request));
        }
        other => panic!("expected BatchOpen by {request}, got {other:?}"),
    }
}

fn rejected(r: Result<impl std::fmt::Debug, ApiError>, policy: &str) {
    match r {
        Err(ApiError::Rejected { policy: p, .. }) => assert_eq!(p, policy),
        other => panic!("expected {policy}, got {other:?}"),
    }
}

async fn take(s: &impl MetaStore, instance: &str, now_ms: i64) -> u64 {
    acquire_lease(
        s,
        &service(),
        now_ms,
        pb::AcquireLeaseRequest {
            repo: "r".into(),
            instance: instance.into(),
        },
    )
    .await
    .expect("lease")
    .epoch
}

async fn followed(s: &impl MetaStore, line: &str) {
    let id = s.repo("r").await.expect("repo").expect("exists");
    let name = followed_ref(line);
    let txn = MetaTxn {
        preconditions: vec![Precondition::RefIs {
            name: name.clone(),
            expected: None,
        }],
        writes: vec![Write::SetRef(RefRecord {
            name,
            target: ChangesetId(ObjectId([1; 32])),
            version: 1,
            kind: RefKind::Tool,
        })],
    };
    assert!(matches!(
        s.commit(id, txn).await,
        Ok(TxnOutcome::Committed(_))
    ));
}

async fn memory() -> MemoryStore {
    let s = MemoryStore::new();
    s.create_repo("r").await.expect("repo");
    s
}

async fn sqlite(dir: &tempfile::TempDir) -> Arc<SqliteMetaStore> {
    let s = Arc::new(
        SqliteMetaStore::open(SqliteConfig::new(dir.path().join("meta.db"))).expect("open"),
    );
    s.create_repo("r").await.expect("repo");
    s
}

/// Runs `scenario` on both stores.
macro_rules! both {
    ($scenario:ident) => {{
        $scenario(&memory().await).await;
        let dir = tempfile::tempdir().expect("dir");
        $scenario(&*sqlite(&dir).await).await;
    }};
}

async fn idempotent_retry(s: &impl MetaStore) {
    let first = cut_batch(s, &admin(), 0, cut("main", "k1"))
        .await
        .expect("cut");
    assert_eq!(state(&first), pb::MirrorRequestState::Pending);
    assert_eq!(batch(&first), pb::BatchState::Requested);
    assert_eq!(first.deadline_ms, REQUEST_TIMEOUT_MS);
    // The retry returns the original request, even with other arguments.
    let mut again = cut("main", "k1");
    again.excludes = vec![vec![9; 32]];
    let retry = cut_batch(s, &admin(), MIN, again).await.expect("retry");
    assert_eq!(retry, first);
    // A request under another key is refused: the batch holds the line.
    held_by(
        cut_batch(s, &admin(), MIN, cut("main", "k2")).await,
        first.id,
    );
    // Once claimed and finished, the retry still returns that request.
    let epoch = take(s, "i1", MIN).await;
    let claimed = claim(s, &service(), MIN, claim_req(epoch))
        .await
        .expect("claim")
        .request
        .expect("one");
    assert_eq!(claimed.id, first.id);
    complete(s, &service(), MIN, with_pr(first.id, epoch, true))
        .await
        .expect("complete");
    let retry = cut_batch(s, &admin(), 2 * MIN, cut("main", "k1"))
        .await
        .expect("retry");
    assert_eq!(retry.id, first.id);
    assert_eq!(state(&retry), pb::MirrorRequestState::Done);
    assert_eq!(batch(&retry), pb::BatchState::Open);
    let id = s.repo("r").await.expect("repo").expect("exists");
    let all = s
        .list_mirror_requests(id, MirrorRequestQuery::default())
        .await
        .expect("list");
    assert_eq!(all.len(), 1, "a retry never records a second request");
}

#[tokio::test]
async fn a_retried_cut_returns_the_original_request() {
    both!(idempotent_retry);
}

async fn lifecycle(s: &impl MetaStore) {
    let r = cut_batch(s, &admin(), 0, cut("main", "k1"))
        .await
        .expect("cut");
    let epoch = take(s, "i1", 0).await;
    claim(s, &service(), 0, claim_req(epoch))
        .await
        .expect("claim");
    let done = complete(s, &service(), 0, with_pr(r.id, epoch, true))
        .await
        .expect("complete");
    assert_eq!(batch(&done), pb::BatchState::Open);
    assert_eq!(done.result.as_ref().and_then(|o| o.pr_number), Some(41));
    let open = get_batch(s, &admin(), 0, batch_key("main"))
        .await
        .expect("get");
    assert_eq!(open.batch.map(|b| b.id), Some(r.id));
    // Sealed still holds the line.
    let seal = pb::SetBatchStateRequest {
        repo: "r".into(),
        id: r.id,
        epoch,
        state: pb::BatchState::Sealed as i32,
    };
    set_batch_state(s, &service(), 0, seal.clone())
        .await
        .expect("seal");
    held_by(cut_batch(s, &admin(), 0, cut("main", "k2")).await, r.id);
    // Settling clears the pointer by CAS; a later cut opens the next batch.
    let settle_it = pb::SetBatchStateRequest {
        state: pb::BatchState::Settled as i32,
        ..seal
    };
    set_batch_state(s, &service(), 0, settle_it.clone())
        .await
        .expect("settle");
    assert!(
        get_batch(s, &admin(), 0, batch_key("main"))
            .await
            .expect("get")
            .batch
            .is_none()
    );
    rejected(
        set_batch_state(s, &service(), 0, settle_it).await,
        "mirror-request-state",
    );
    let next = cut_batch(s, &admin(), 0, cut("main", "k2"))
        .await
        .expect("next");
    assert!(next.id > r.id);
    // History lists both, newest first, one per page.
    let list = |token: String| pb::ListBatchesRequest {
        repo: "r".into(),
        line: None,
        page_token: token,
        limit: 1,
    };
    let page = list_batches(s, &admin(), 0, list(String::new()))
        .await
        .expect("list");
    let ids = |p: &pb::BatchPage| p.batches.iter().map(|b| b.id).collect::<Vec<_>>();
    assert_eq!(ids(&page), vec![next.id]);
    let rest = list_batches(s, &admin(), 0, list(page.next_page_token))
        .await
        .expect("list");
    assert_eq!(ids(&rest), vec![r.id]);
}

#[tokio::test]
async fn a_batch_moves_requested_open_sealed_settled() {
    both!(lifecycle);
}

/// Contenders racing `CutBatch` on one line of one SQLite store, across
/// threads, each with its own key: exactly one is recorded, and every other
/// gets the typed `BatchOpen` naming it.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn concurrent_cuts_have_exactly_one_winner() {
    let dir = tempfile::tempdir().expect("dir");
    let s = sqlite(&dir).await;
    let tasks: Vec<_> = (0..8)
        .map(|i| {
            let s = s.clone();
            tokio::spawn(
                async move { cut_batch(&*s, &admin(), 0, cut("main", &format!("k{i}"))).await },
            )
        })
        .collect();
    let (mut won, mut lost) = (Vec::new(), Vec::new());
    for t in tasks {
        match t.await.expect("task") {
            Ok(r) => won.push(r.id),
            Err(ApiError::BatchOpen { request, .. }) => lost.push(request),
            Err(other) => panic!("unexpected {other:?}"),
        }
    }
    assert_eq!(won.len(), 1, "{won:?}");
    assert!(lost.iter().all(|r| *r == won[0]), "{lost:?}");
    let id = s.repo("r").await.expect("repo").expect("exists");
    let all = s
        .list_mirror_requests(id, MirrorRequestQuery::default())
        .await
        .expect("list");
    assert_eq!(all.len(), 1);
    assert_eq!(
        s.line_slot(id, "main", LineSlot::Batch)
            .await
            .expect("slot"),
        Some(won[0])
    );
}

/// Contenders retrying one key concurrently all get the one request.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn concurrent_retries_of_one_key_record_one_request() {
    let dir = tempfile::tempdir().expect("dir");
    let s = sqlite(&dir).await;
    let tasks: Vec<_> = (0..8)
        .map(|_| {
            let s = s.clone();
            tokio::spawn(async move { cut_batch(&*s, &admin(), 0, cut("main", "same")).await })
        })
        .collect();
    let mut ids = Vec::new();
    for t in tasks {
        ids.push(t.await.expect("task").expect("cut").id);
    }
    ids.dedup();
    assert_eq!(ids.len(), 1, "{ids:?}");
}

async fn stale_claim(s: &impl MetaStore) {
    let r = cut_batch(s, &admin(), 0, cut("main", "k1"))
        .await
        .expect("cut");
    let old = take(s, "i1", 0).await;
    let got = claim(s, &service(), 0, claim_req(old))
        .await
        .expect("claim");
    assert_eq!(got.request.map(|c| c.claimed_by_epoch), Some(Some(old)));
    // The holder crashes; its lease lapses and another process takes it.
    let later = crate::mirrors::LEASE_TTL_MS + 1;
    let new = take(s, "i2", later).await;
    assert!(new > old);
    // Read under the new epoch, the old claim is pending again.
    let seen = get_request(s, &admin(), later, key(r.id))
        .await
        .expect("get");
    assert_eq!(state(&seen), pb::MirrorRequestState::Pending);
    assert_eq!(seen.claimed_by_epoch, None);
    // The fenced-off process can neither finish it nor claim again.
    assert!(matches!(
        complete(s, &service(), later, with_pr(r.id, old, true)).await,
        Err(ApiError::LeaseLost { .. })
    ));
    assert!(matches!(
        claim(s, &service(), later, claim_req(old)).await,
        Err(ApiError::LeaseLost { .. })
    ));
    let again = claim(s, &service(), later, claim_req(new))
        .await
        .expect("claim")
        .request
        .expect("one");
    assert_eq!((again.id, again.claimed_by_epoch), (r.id, Some(new)));
    // Nothing else is claimable: the one request is held under `new`.
    assert!(
        claim(s, &service(), later, claim_req(new))
            .await
            .expect("claim")
            .request
            .is_none()
    );
}

#[tokio::test]
async fn a_claim_under_a_lost_epoch_returns_to_pending() {
    both!(stale_claim);
}

/// Crash recovery: the service created the PR and recorded it, then
/// crashed before finishing. The next holder reclaims the request and gets
/// the PR on the deterministic branch back, so it opens no second PR.
async fn crash_recovery(s: &impl MetaStore) {
    let r = cut_batch(s, &admin(), 0, cut("main", "k1"))
        .await
        .expect("cut");
    let old = take(s, "i1", 0).await;
    claim(s, &service(), 0, claim_req(old))
        .await
        .expect("claim");
    let recorded = complete(s, &service(), 0, with_pr(r.id, old, false))
        .await
        .expect("record");
    assert_eq!(state(&recorded), pb::MirrorRequestState::Claimed);
    let later = crate::mirrors::LEASE_TTL_MS + 1;
    let new = take(s, "i2", later).await;
    let again = claim(s, &service(), later, claim_req(new))
        .await
        .expect("claim")
        .request
        .expect("one");
    let result = again.result.expect("result");
    assert_eq!(result.branch, Some(batch_branch(r.id)));
    assert_eq!(result.pr_number, Some(41));
    // Any other branch is refused: the name is deterministic.
    let mut wrong = with_pr(r.id, new, true);
    if let Some(o) = wrong.result.as_mut() {
        o.branch = Some("graft/batch-other".into());
    }
    rejected(complete(s, &service(), later, wrong).await, "mirror-field");
    let done = complete(s, &service(), later, with_pr(r.id, new, true))
        .await
        .expect("finish");
    assert_eq!(batch(&done), pb::BatchState::Open);
}

#[tokio::test]
async fn a_recorded_pr_is_adopted_after_a_crash() {
    both!(crash_recovery);
}

async fn expiry_without_pr(s: &impl MetaStore) {
    // A cut no service ever claimed cannot have opened a PR: it expires,
    // and the line is free.
    let r = cut_batch(s, &admin(), 0, cut("main", "k1"))
        .await
        .expect("cut");
    let late = REQUEST_TIMEOUT_MS;
    let seen = get_request(s, &admin(), late, key(r.id))
        .await
        .expect("get");
    assert_eq!(state(&seen), pb::MirrorRequestState::Expired);
    assert!(
        get_batch(s, &admin(), late, batch_key("main"))
            .await
            .expect("get")
            .batch
            .is_none()
    );
    cut_batch(s, &admin(), late, cut("main", "k2"))
        .await
        .expect("next cut");
}

async fn crash_after_create(s: &impl MetaStore) {
    // A service claims the cut, opens its PR on batch-<id>, and crashes
    // before recording it. Nothing claims it again before the deadline.
    let r = cut_batch(s, &admin(), 0, cut("main", "k1"))
        .await
        .expect("cut");
    let epoch = take(s, "i1", 0).await;
    claim(s, &service(), 0, claim_req(epoch))
        .await
        .expect("claim");
    let late = REQUEST_TIMEOUT_MS * 4;
    let seen = get_request(s, &admin(), late, key(r.id))
        .await
        .expect("get");
    // It does not expire: the PR may exist, so the line stays held and no
    // second cut (on another branch) can open a second batch PR.
    assert_eq!(state(&seen), pb::MirrorRequestState::Pending);
    held_by(cut_batch(s, &admin(), late, cut("main", "k2")).await, r.id);
    // The restarted service claims the same request again, so it looks on
    // the same branch and adopts the PR it opened.
    let new = take(s, "i2", late).await;
    let again = claim(s, &service(), late, claim_req(new))
        .await
        .expect("claim")
        .request
        .expect("the orphaned cut is claimable");
    assert_eq!(again.id, r.id);
    let done = complete(s, &service(), late, with_pr(r.id, new, true))
        .await
        .expect("record the adopted PR");
    assert_eq!(batch(&done), pb::BatchState::Open);
    assert_eq!(done.result.and_then(|o| o.branch), Some(batch_branch(r.id)));
}

async fn expiry_with_pr(s: &impl MetaStore) {
    let r = cut_batch(s, &admin(), 0, cut("main", "k1"))
        .await
        .expect("cut");
    let epoch = take(s, "i1", 0).await;
    claim(s, &service(), 0, claim_req(epoch))
        .await
        .expect("claim");
    complete(s, &service(), 0, with_pr(r.id, epoch, false))
        .await
        .expect("record");
    let late = REQUEST_TIMEOUT_MS;
    // The cut's own check of the slot holder applies the expiry.
    held_by(cut_batch(s, &admin(), late, cut("main", "k2")).await, r.id);
    let seen = get_request(s, &admin(), late, key(r.id))
        .await
        .expect("get");
    assert_eq!(state(&seen), pb::MirrorRequestState::Expired);
    assert_eq!(batch(&seen), pb::BatchState::Open);
    // The live PR keeps the line until its batch settles.
    let open = get_batch(s, &admin(), late, batch_key("main"))
        .await
        .expect("get");
    assert_eq!(open.batch.map(|b| b.id), Some(r.id));
    // The first lease lapsed long ago; the service takes a new one. A late
    // completion is refused, and the service settles the batch instead.
    let new = take(s, "i2", late).await;
    rejected(
        complete(s, &service(), late, with_pr(r.id, new, true)).await,
        "mirror-request-state",
    );
    set_batch_state(
        s,
        &service(),
        late,
        pb::SetBatchStateRequest {
            repo: "r".into(),
            id: r.id,
            epoch: new,
            state: pb::BatchState::Settled as i32,
        },
    )
    .await
    .expect("settle");
    cut_batch(s, &admin(), late, cut("main", "k2"))
        .await
        .expect("next cut");
}

#[tokio::test]
async fn an_expired_cut_releases_the_line_only_without_a_pr() {
    both!(expiry_without_pr);
    both!(expiry_with_pr);
}

#[tokio::test]
async fn a_claimed_cut_without_a_pr_never_expires_and_is_retried_on_its_branch() {
    both!(crash_after_create);
}

fn cancel_req(id: u64) -> pb::CancelRequestRequest {
    pb::CancelRequestRequest {
        repo: "r".into(),
        id,
    }
}

async fn operator_cancel(s: &impl MetaStore) {
    // A claimed cut with no PR holds its line forever (crash_after_create);
    // its service is gone for good.
    let stuck = cut_batch(s, &admin(), 0, cut("main", "k1"))
        .await
        .expect("cut");
    let epoch = take(s, "i1", 0).await;
    claim(s, &service(), 0, claim_req(epoch))
        .await
        .expect("claim");
    let late = REQUEST_TIMEOUT_MS * 4;
    held_by(
        cut_batch(s, &admin(), late, cut("main", "k2")).await,
        stuck.id,
    );
    // Only the operator grant cancels: not the repo admin, not the service.
    for p in [admin(), service()] {
        assert!(matches!(
            cancel(s, &p, late, cancel_req(stuck.id)).await,
            Err(ApiError::PermissionDenied { .. })
        ));
    }
    let gone = cancel(s, &bind(), late, cancel_req(stuck.id))
        .await
        .expect("cancel");
    assert_eq!(state(&gone), pb::MirrorRequestState::Failed);
    assert_eq!(batch(&gone), pb::BatchState::Settled);
    assert_eq!(
        gone.result.and_then(|o| o.error_code).as_deref(),
        Some(CANCELLED)
    );
    // The line is free, and a late service write is refused.
    assert!(
        get_batch(s, &admin(), late, batch_key("main"))
            .await
            .expect("get")
            .batch
            .is_none()
    );
    let new = take(s, "i2", late).await;
    rejected(
        complete(s, &service(), late, with_pr(stuck.id, new, true)).await,
        "mirror-request-state",
    );
    let next = cut_batch(s, &admin(), late, cut("main", "k2"))
        .await
        .expect("the next cut");
    // A batch whose PR was closed by hand: done, open, never settled.
    let epoch = new;
    claim(s, &service(), late, claim_req(epoch))
        .await
        .expect("claim");
    let open = complete(s, &service(), late, with_pr(next.id, epoch, true))
        .await
        .expect("record");
    assert_eq!(batch(&open), pb::BatchState::Open);
    let settled = cancel(s, &bind(), late, cancel_req(next.id))
        .await
        .expect("cancel an open batch");
    assert_eq!(state(&settled), pb::MirrorRequestState::Done);
    assert_eq!(batch(&settled), pb::BatchState::Settled);
    cut_batch(s, &admin(), late, cut("main", "k3"))
        .await
        .expect("the line is free again");
    // A request that holds nothing is not cancelled twice.
    rejected(
        cancel(s, &bind(), late, cancel_req(next.id)).await,
        "mirror-request-state",
    );
    assert!(matches!(
        cancel(s, &bind(), late, cancel_req(999)).await,
        Err(ApiError::NotFound { .. })
    ));
}

#[tokio::test]
async fn the_operator_cancels_a_stuck_cut_and_frees_its_line() {
    both!(operator_cancel);
}

async fn rewind_rules(s: &impl MetaStore) {
    // Only on a followed line, and only under mirror:bind.
    rejected(
        rewind_followed(s, &bind(), 0, rewind("main", "w1")).await,
        "mirror-not-followed",
    );
    followed(s, "main").await;
    assert!(matches!(
        rewind_followed(s, &admin(), 0, rewind("main", "w1")).await,
        Err(ApiError::PermissionDenied { .. })
    ));
    // Refused while a batch is open.
    let b = cut_batch(s, &admin(), 0, cut("main", "k1"))
        .await
        .expect("cut");
    held_by(
        rewind_followed(s, &bind(), 0, rewind("main", "w1")).await,
        b.id,
    );
    let epoch = take(s, "i1", 0).await;
    claim(s, &service(), 0, claim_req(epoch))
        .await
        .expect("claim");
    // Nothing to batch: finished with no PR, the batch settles at once.
    let empty = complete(
        s,
        &service(),
        0,
        pb::CompleteRequestRequest {
            result: None,
            ..with_pr(b.id, epoch, true)
        },
    )
    .await
    .expect("nothing to batch");
    assert_eq!(batch(&empty), pb::BatchState::Settled);
    // One rewind per line; a retry returns it, and it holds off cuts.
    let w = rewind_followed(s, &bind(), 0, rewind("main", "w1"))
        .await
        .expect("rewind");
    let retry = rewind_followed(s, &bind(), 0, rewind("main", "w1"))
        .await
        .expect("retry");
    assert_eq!(retry.id, w.id);
    held_by(
        rewind_followed(s, &bind(), 0, rewind("main", "w2")).await,
        w.id,
    );
    held_by(cut_batch(s, &admin(), 0, cut("main", "k2")).await, w.id);
    // Finishing it, even as a failure, frees the line.
    let got = claim(s, &service(), 0, claim_req(epoch))
        .await
        .expect("claim");
    assert_eq!(got.request.map(|r| r.id), Some(w.id));
    fail(
        s,
        &service(),
        0,
        pb::FailRequestRequest {
            repo: "r".into(),
            id: w.id,
            epoch,
            error_code: "diverged".into(),
        },
    )
    .await
    .expect("fail");
    cut_batch(s, &admin(), 0, cut("main", "k2"))
        .await
        .expect("cut after rewind");
}

#[tokio::test]
async fn a_rewind_is_refused_while_a_batch_is_open() {
    both!(rewind_rules);
}

#[tokio::test]
async fn grants_and_fields_are_checked_at_the_boundary() {
    let s = memory().await;
    let reader = who("dev", &["repo:r:read"]);
    assert!(matches!(
        cut_batch(&s, &reader, 0, cut("main", "k1")).await,
        Err(ApiError::PermissionDenied { .. })
    ));
    rejected(
        cut_batch(&s, &admin(), 0, cut("main", "")).await,
        "mirror-field",
    );
    rejected(
        cut_batch(&s, &admin(), 0, cut("main", "a b")).await,
        "mirror-field",
    );
    let mut bad = cut("main", "k1");
    bad.base = Some(vec![1; 3]);
    rejected(cut_batch(&s, &admin(), 0, bad).await, "mirror-field");
    // The service calls need a mirror-kind token.
    assert!(matches!(
        claim(&s, &admin(), 0, claim_req(1)).await,
        Err(ApiError::PermissionDenied { .. })
    ));
    // A reader sees requests, but not another repository's reader.
    let r = cut_batch(&s, &admin(), 0, cut("main", "k1"))
        .await
        .expect("cut");
    get_request(&s, &reader, 0, key(r.id)).await.expect("read");
    assert!(matches!(
        get_request(&s, &who("x", &["repo:other:read"]), 0, key(r.id)).await,
        Err(ApiError::PermissionDenied { .. })
    ));
    assert!(matches!(
        get_request(&s, &reader, 0, key(r.id + 1)).await,
        Err(ApiError::NotFound { .. })
    ));
}

fn mirror_as(subject: &str) -> Principal {
    Principal {
        kind: PrincipalKind::Mirror,
        ..who(subject, &["repo:*:mirror"])
    }
}

async fn foreign_holder(s: &impl MetaStore) {
    use crate::mirrors::{release_lease, renew_lease};
    // The holder is "mirror/i1"; "mirr" and "other" are other subjects, and
    // "mirror/i1" itself is a subject whose prefix-match would pass.
    let epoch = take(s, "i1", 0).await;
    let req = || pb::LeaseRequest {
        repo: "r".into(),
        epoch,
    };
    for subject in ["other", "mirr", "mirror/i1"] {
        let p = mirror_as(subject);
        assert!(
            matches!(
                renew_lease(s, &p, 1, req()).await,
                Err(ApiError::LeaseLost { .. })
            ),
            "{subject} renewed"
        );
        assert!(
            matches!(
                release_lease(s, &p, 1, req()).await,
                Err(ApiError::LeaseLost { .. })
            ),
            "{subject} released"
        );
    }
    renew_lease(s, &service(), 1, req())
        .await
        .expect("the holder renews");
    release_lease(s, &service(), 1, req())
        .await
        .expect("the holder releases");
}

#[tokio::test]
async fn only_the_exact_holder_renews_or_releases_a_lease() {
    both!(foreign_holder);
}
