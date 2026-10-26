//! `Land` and `Attest` (`docs/design.md` §8.1, §8.3, §8.6, decision D6).
//!
//! A land checks its idempotency record, the caller's scopes and the line's
//! policy, then loops until its attempt budget or deadline runs out:
//!
//! 1. read the line's tip;
//! 2. take the stack head as the candidate when the stack sits on the tip,
//!    otherwise rebase the stack onto it (a conflict ends the land with
//!    `Conflict` and a `LandAttempt` entry);
//! 3. store new candidates as drafts held by a land-attempt lease
//!    (`land:<expires_ms>:<attempt>`), so a crashed server's candidates are
//!    collected after the lease like any loser's;
//! 4. on a `queued` line, test the candidate;
//! 5. commit one transaction: the line's CAS, its line index rows, the
//!    records that land (cleared of holder and successor) and the drafts the
//!    candidates supersede, the `lands` row and the land result, and the
//!    `RefMoved` and `LandAttempt` journal entries;
//! 6. on a lost CAS, back off (full jitter, 2 ms base, 200 ms cap) and loop.
//!
//! A retried land finds its `lands` row and returns the same `Landed`, read
//! from the land result row written beside it. Out of attempts, it returns
//! `Contended`.
//!
//! An attestation is the latest `Attest` for a `(changeset, check)`. The
//! `attested` policy and path rules read the stack head's attestations,
//! following `graft.rebased-from` back through rebases (a sync) for at most
//! [`REBASED_HOPS`] hops: a file-level rebase onto a newer tip is trusted
//! (D6). A `queued` line reads the candidate's own attestations only, or
//! asks [`LandHooks::test_candidate`].
//!
//! With `merge = line` the land's rebase may resolve paths by line merge
//! (D16); the result names them in `merged_paths`. On an `attested` line
//! with `attest_merged`, a line-merged result must carry its own
//! attestations: the lookup stops at a changeset whose rebase merged
//! (`graft.merged-paths`), and a land whose own rebase would merge is
//! `Rejected`, so the caller syncs, has CI attest the merged head, and lands
//! that.
//!
//! Durable formats, in the derived rows (`MetaStore::derived`):
//!
//! - kind `land` v1, key `head(32) ‖ line`: `u8 1 ‖ landed(32) ‖ u64 version ‖
//!   u64 pos ‖ u8 rebased ‖ u32 n ‖ (u32 len ‖ path)…`
//! - kind `attest` v1, key `changeset(32) ‖ check`: `u8 1 ‖ u8 passed ‖
//!   u8 kind ‖ i64 at_ms ‖ u32 len ‖ url ‖ u32 len ‖ actor`
//!
//! Integers are big-endian; `kind` is 0 human, 1 agent, 2 ci, 3 service.

use std::collections::hash_map::RandomState;
use std::future::Future;
use std::hash::{BuildHasher, Hasher};
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll, Waker};
use std::time::Duration;

use graft_model::{Changeset, ChangesetId, ObjectId, ObjectKind, RepoPath};
use graft_proto::error::{MAX_STALE_PATHS, StaleBase};
use graft_proto::types::{Attest, Land, Landed};
use graft_proto::{ApiError, CallContext, PrincipalKind};
use graft_store::{
    MetaStore, MetaTxn, ObjectStore, Precondition, RecordState, RefKind, RefRecord, RepoId,
    TxnOutcome, Write,
};

use crate::access::{Need, require, require_changes};
use crate::journal::{self, LandOutcome};
use crate::merge::MergeSource;
use crate::policy::{
    Authority, Candidate, HUMAN_REVIEW, HookFuture, LandPolicy, LandedEvent, LineConfig, Verdict,
};
use crate::rebase::{MERGED_PATHS, REBASED_FROM, Step};
use crate::refs::check_ref_name;
use crate::workspace::transition;
use crate::write::Lease;
use crate::{Engine, errors};

/// Derived kind of a land's result, keyed by `(head, line)`.
pub const LAND_RESULT: &str = "land";

/// Derived kind of an attestation, keyed by `(changeset, check)`.
pub const ATTESTATION: &str = "attest";

/// Version of both derived kinds.
pub(crate) const KIND_VERSION: u32 = 1;

/// The longest check name, in bytes.
pub const MAX_CHECK_NAME: usize = 128;

/// The longest attestation URL, in bytes.
pub const MAX_ATTEST_URL: usize = 2048;

/// How many `graft.rebased-from` links an attestation lookup follows.
pub const REBASED_HOPS: usize = 16;

/// Sleeps for the land backoff. The engine has no runtime of its own, so
/// the server can install one that uses its timer; the default parks a
/// short-lived thread.
pub trait Sleeper: Send + Sync {
    /// A future that completes after `ms` milliseconds.
    fn sleep(&self, ms: u64) -> HookFuture<'static, ()>;
}

/// The default [`Sleeper`]: one thread per sleep, which wakes the task.
/// Sleeps happen only after a lost CAS, never on an uncontended land.
#[derive(Clone, Copy, Debug, Default)]
pub struct ThreadSleeper;

impl Sleeper for ThreadSleeper {
    fn sleep(&self, ms: u64) -> HookFuture<'static, ()> {
        Box::pin(ThreadSleep { ms, shared: None })
    }
}

#[derive(Default)]
struct SleepState {
    done: bool,
    waker: Option<Waker>,
}

struct ThreadSleep {
    ms: u64,
    shared: Option<Arc<Mutex<SleepState>>>,
}

impl Future for ThreadSleep {
    type Output = ();

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<()> {
        let this = self.get_mut();
        if this.ms == 0 {
            return Poll::Ready(());
        }
        if let Some(shared) = &this.shared {
            let mut state = shared.lock().unwrap_or_else(|e| e.into_inner());
            if state.done {
                return Poll::Ready(());
            }
            state.waker = Some(cx.waker().clone());
            return Poll::Pending;
        }
        let shared = Arc::new(Mutex::new(SleepState {
            done: false,
            waker: Some(cx.waker().clone()),
        }));
        let theirs = shared.clone();
        let ms = this.ms;
        let spawned = std::thread::Builder::new()
            .name("graft-land-backoff".to_owned())
            .spawn(move || {
                std::thread::sleep(Duration::from_millis(ms));
                let mut state = theirs.lock().unwrap_or_else(|e| e.into_inner());
                state.done = true;
                if let Some(waker) = state.waker.take() {
                    waker.wake();
                }
            });
        if spawned.is_err() {
            // No thread to sleep on: retry at once rather than fail.
            return Poll::Ready(());
        }
        this.shared = Some(shared);
        Poll::Pending
    }
}

/// A uniformly random delay in `0..=ceiling` (the "full jitter" of the
/// backoff). Process-random keys, so racing servers do not sleep in step.
fn jitter(ceiling: u64, attempt: u32) -> u64 {
    if ceiling == 0 {
        return 0;
    }
    let mut h = RandomState::new().build_hasher();
    h.write_u32(attempt);
    h.finish() % (ceiling + 1)
}

/// The backoff ceiling before attempt `attempt + 1`: `base · 2^attempt`,
/// capped.
pub(crate) fn backoff_ceiling(base: u64, cap: u64, attempt: u32) -> u64 {
    base.saturating_mul(1u64 << attempt.min(32)).min(cap)
}

/// What one land attempt came to.
enum Attempt {
    /// The line moved (or already held the stack).
    Done(Landed),
    /// The line's CAS lost: back off and retry.
    Lost,
    /// A record the attempt read changed meanwhile: retry.
    Raced,
}

/// A stored land result.
#[derive(Clone, Debug, PartialEq, Eq)]
struct LandResult {
    landed: ChangesetId,
    version: u64,
    pos: u64,
    rebased: bool,
    merged_paths: Vec<RepoPath>,
}

impl LandResult {
    fn landed(self) -> Landed {
        Landed::Landed {
            landed: self.landed,
            line_version: self.version,
            pos: self.pos,
            rebased: self.rebased,
            merged_paths: self.merged_paths,
        }
    }

    fn encode(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(54);
        out.push(1);
        out.extend_from_slice(&self.landed.0.0);
        out.extend_from_slice(&self.version.to_be_bytes());
        out.extend_from_slice(&self.pos.to_be_bytes());
        out.push(u8::from(self.rebased));
        out.extend_from_slice(&(self.merged_paths.len() as u32).to_be_bytes());
        for path in &self.merged_paths {
            put_str(&mut out, path.as_str());
        }
        out
    }

    fn decode(bytes: &[u8]) -> Option<Self> {
        let mut r = Reader(bytes);
        if r.u8()? != 1 {
            return None;
        }
        let landed = ChangesetId(ObjectId(r.take(32)?.try_into().ok()?));
        let version = r.u64()?;
        let pos = r.u64()?;
        let rebased = r.u8()? != 0;
        let n = r.u32()?;
        let mut merged_paths = Vec::new();
        for _ in 0..n {
            merged_paths.push(RepoPath::new(r.str()?).ok()?);
        }
        r.0.is_empty().then_some(Self {
            landed,
            version,
            pos,
            rebased,
            merged_paths,
        })
    }
}

/// A stored attestation.
#[derive(Clone, Debug, PartialEq, Eq)]
struct Attestation {
    passed: bool,
    kind: PrincipalKind,
    url: String,
}

fn kind_byte(kind: PrincipalKind) -> u8 {
    match kind {
        PrincipalKind::Human => 0,
        PrincipalKind::Agent => 1,
        PrincipalKind::Ci => 2,
        PrincipalKind::Service => 3,
    }
}

pub(crate) fn encode_attestation(
    passed: bool,
    kind: PrincipalKind,
    at_ms: i64,
    url: &str,
    actor: &str,
) -> Vec<u8> {
    let mut out = Vec::with_capacity(19 + url.len() + actor.len());
    out.push(1);
    out.push(u8::from(passed));
    out.push(kind_byte(kind));
    out.extend_from_slice(&at_ms.to_be_bytes());
    put_str(&mut out, url);
    put_str(&mut out, actor);
    out
}

fn decode_attestation(bytes: &[u8]) -> Option<Attestation> {
    let mut r = Reader(bytes);
    if r.u8()? != 1 {
        return None;
    }
    let passed = r.u8()? != 0;
    let kind = match r.u8()? {
        0 => PrincipalKind::Human,
        1 => PrincipalKind::Agent,
        2 => PrincipalKind::Ci,
        3 => PrincipalKind::Service,
        _ => return None,
    };
    let _at_ms = r.u64()?;
    let url = r.str()?.to_owned();
    let _actor = r.str()?;
    r.0.is_empty().then_some(Attestation { passed, kind, url })
}

fn put_str(out: &mut Vec<u8>, s: &str) {
    out.extend_from_slice(&(s.len() as u32).to_be_bytes());
    out.extend_from_slice(s.as_bytes());
}

struct Reader<'a>(&'a [u8]);

impl<'a> Reader<'a> {
    fn take(&mut self, n: usize) -> Option<&'a [u8]> {
        if self.0.len() < n {
            return None;
        }
        let (head, rest) = self.0.split_at(n);
        self.0 = rest;
        Some(head)
    }
    fn u8(&mut self) -> Option<u8> {
        Some(self.take(1)?[0])
    }
    fn u32(&mut self) -> Option<u32> {
        Some(u32::from_be_bytes(self.take(4)?.try_into().ok()?))
    }
    fn u64(&mut self) -> Option<u64> {
        Some(u64::from_be_bytes(self.take(8)?.try_into().ok()?))
    }
    fn str(&mut self) -> Option<&'a str> {
        let n = self.u32()? as usize;
        std::str::from_utf8(self.take(n)?).ok()
    }
}

/// `changeset(32) ‖ text`: the key of both derived kinds.
pub(crate) fn key(id: ChangesetId, text: &str) -> Vec<u8> {
    let mut out = Vec::with_capacity(32 + text.len());
    out.extend_from_slice(&id.0.0);
    out.extend_from_slice(text.as_bytes());
    out
}

fn rejected(policy: &str, detail: String) -> ApiError {
    ApiError::Rejected {
        policy: policy.to_owned(),
        detail,
    }
}

/// A check name: 1 to 128 bytes, no whitespace or control characters.
pub(crate) fn check_check_name(check: &str) -> Result<(), ApiError> {
    let ok = !check.is_empty()
        && check.len() <= MAX_CHECK_NAME
        && !check.chars().any(|c| c.is_whitespace() || c.is_control());
    if ok {
        Ok(())
    } else {
        Err(errors::invalid(None, "check-name"))
    }
}

/// Whether a line requires line-merged results to be re-attested.
fn attest_merged(line: &LineConfig) -> bool {
    matches!(
        line.policy,
        LandPolicy::Attested {
            attest_merged: true,
            ..
        }
    )
}

/// How many merged paths a `Rejected` detail names.
const DETAIL_PATHS: usize = 8;

/// The `Rejected { detail }` of a land whose rebase merged by line on an
/// `attest_merged` line.
fn merged_detail(paths: &[RepoPath], tip: ChangesetId) -> String {
    let mut named: Vec<&str> = paths
        .iter()
        .take(DETAIL_PATHS)
        .map(RepoPath::as_str)
        .collect();
    if paths.len() > DETAIL_PATHS {
        named.push("…");
    }
    format!(
        "rebasing onto {tip} merges {} by line, and merged results must be re-attested: sync, attest the merged head and land it",
        named.join(", ")
    )
}

/// Why a head lacks a check, for `Rejected { detail }`.
enum Missing {
    /// No attestation, or none by a person where one is needed.
    Absent(String),
    /// The latest attestation failed.
    Failed(String, String),
}

impl Missing {
    fn detail(self, id: ChangesetId) -> String {
        match self {
            Missing::Absent(check) => format!("{id} has no passing {check} attestation"),
            Missing::Failed(check, url) => format!("{check} failed on {id} ({url})"),
        }
    }
}

impl<M: MetaStore, O: ObjectStore> Engine<M, O> {
    /// The configuration of line `name`.
    pub fn line_config(&self, name: &str) -> LineConfig {
        match self.config.lines.iter().find(|l| l.name == name) {
            Some(line) => line.clone(),
            None => LineConfig {
                name: name.to_owned(),
                ..self.config.default_line.clone()
            },
        }
    }

    /// The line ref `name`: present and a line.
    pub(crate) async fn line_ref(&self, repo: RepoId, name: &str) -> Result<RefRecord, ApiError> {
        let line = self
            .meta
            .get_ref(repo, name)
            .await
            .map_err(errors::store)?
            .ok_or_else(|| errors::not_found(format!("ref {name}")))?;
        if line.kind != RefKind::Line {
            return Err(errors::invalid(None, "not-a-line"));
        }
        Ok(line)
    }

    /// The result of an earlier land of `head` onto `line`, if there was one.
    async fn land_record(
        &self,
        repo: RepoId,
        head: ChangesetId,
        line: &str,
    ) -> Result<Option<Landed>, ApiError> {
        if planted!(self.planted, SkipIdempotency) {
            return Ok(None);
        }
        let Some(landed) = self
            .meta
            .land(repo, head, line)
            .await
            .map_err(errors::store)?
        else {
            return Ok(None);
        };
        let stored = self
            .meta
            .derived(repo, LAND_RESULT, KIND_VERSION, &key(head, line))
            .await
            .map_err(errors::store)?;
        if let Some(result) = stored.as_deref().and_then(LandResult::decode) {
            return Ok(Some(result.landed()));
        }
        // A `lands` row without its result (written by something other than
        // `Land`): answer from the line index and the current ref.
        let pos = self
            .meta
            .line_pos(repo, line, landed)
            .await
            .map_err(errors::store)?
            .unwrap_or_default();
        let version = self.line_ref(repo, line).await?.version;
        Ok(Some(
            LandResult {
                landed,
                version,
                pos,
                rebased: landed != head,
                merged_paths: Vec::new(),
            }
            .landed(),
        ))
    }

    /// The latest attestation of `check` on `id` itself.
    async fn attestation(
        &self,
        repo: RepoId,
        id: ChangesetId,
        check: &str,
    ) -> Result<Option<Attestation>, ApiError> {
        let stored = self
            .meta
            .derived(repo, ATTESTATION, KIND_VERSION, &key(id, check))
            .await
            .map_err(errors::store)?;
        Ok(stored.as_deref().and_then(decode_attestation))
    }

    /// The latest attestation of `check` on `id`, or on the drafts it was
    /// rebased from (at most [`REBASED_HOPS`] links). With `merged_own`,
    /// a changeset whose rebase merged by line passes on no attestation
    /// from before it: its content was never tested.
    async fn inherited_attestation(
        &self,
        repo: RepoId,
        id: ChangesetId,
        cs: &Changeset,
        check: &str,
        merged_own: bool,
    ) -> Result<Option<Attestation>, ApiError> {
        let link = |c: &Changeset| {
            if merged_own && c.metadata.contains_key(MERGED_PATHS) {
                None
            } else {
                c.metadata.get(REBASED_FROM).cloned()
            }
        };
        let mut cur = id;
        let mut from = link(cs);
        for _ in 0..=REBASED_HOPS {
            if let Some(found) = self.attestation(repo, cur, check).await? {
                return Ok(Some(found));
            }
            let Some(prev) = from.take().and_then(|t| t.parse::<ChangesetId>().ok()) else {
                return Ok(None);
            };
            cur = prev;
            from = match self.changeset_opt(repo, prev).await {
                Ok(Some((pcs, _))) => link(&pcs),
                Ok(None) | Err(ApiError::Collected { .. }) => None,
                Err(e) => return Err(e),
            };
        }
        Ok(None)
    }

    /// The first of `checks` the head does not pass (with a person's
    /// attestation when `human`), or `None`. `merged_own` is the line's
    /// `attest_merged`.
    async fn head_lacks(
        &self,
        repo: RepoId,
        id: ChangesetId,
        cs: &Changeset,
        checks: &[String],
        human: bool,
        merged_own: bool,
    ) -> Result<Option<Missing>, ApiError> {
        for check in checks {
            match self
                .inherited_attestation(repo, id, cs, check, merged_own)
                .await?
            {
                Some(a) if a.passed => {}
                Some(a) => return Ok(Some(Missing::Failed(check.clone(), a.url))),
                None => return Ok(Some(Missing::Absent(check.clone()))),
            }
        }
        if human {
            match self
                .inherited_attestation(repo, id, cs, HUMAN_REVIEW, merged_own)
                .await?
            {
                Some(a) if a.passed && a.kind == PrincipalKind::Human => {}
                Some(a) if !a.passed => {
                    return Ok(Some(Missing::Failed(HUMAN_REVIEW.to_owned(), a.url)));
                }
                _ => return Ok(Some(Missing::Absent(HUMAN_REVIEW.to_owned()))),
            }
        }
        Ok(None)
    }

    /// The policy pre-checks on the stack head (design §8.3): the
    /// `attested` policy's checks, then the path rules' checks and human
    /// review.
    async fn pre_checks(
        &self,
        repo: RepoId,
        line: &LineConfig,
        head: ChangesetId,
        head_cs: &Changeset,
        stack: &[(ChangesetId, Changeset)],
    ) -> Result<(), ApiError> {
        let merged_own = attest_merged(line);
        if let LandPolicy::Attested { checks, .. } = &line.policy
            && let Some(missing) = self
                .head_lacks(repo, head, head_cs, checks, false, merged_own)
                .await?
        {
            return Err(rejected(line.policy.name(), missing.detail(head)));
        }
        let paths = stack
            .iter()
            .flat_map(|(_, cs)| cs.changes.iter().map(|c| c.path()));
        let (checks, human) = line.rule_checks(paths);
        if let Some(missing) = self
            .head_lacks(repo, head, head_cs, &checks, human, merged_own)
            .await?
        {
            let policy = match &missing {
                Missing::Absent(c) | Missing::Failed(c, _) if c == HUMAN_REVIEW && human => {
                    "human-review"
                }
                _ => "path-rule",
            };
            return Err(rejected(policy, missing.detail(head)));
        }
        Ok(())
    }

    /// A `queued` line's test of its candidate: its own passing
    /// attestations for every check, or else the CI hook's verdict.
    async fn test_candidate(
        &self,
        repo: RepoId,
        candidate: Candidate,
    ) -> Result<Verdict, ApiError> {
        let mut attested = true;
        for check in &candidate.checks {
            if !self
                .attestation(repo, candidate.candidate, check)
                .await?
                .is_some_and(|a| a.passed)
            {
                attested = false;
                break;
            }
        }
        if attested {
            return Ok(Verdict::Pass);
        }
        Ok(self.hooks.test_candidate(&candidate).await)
    }

    /// The land-attempt lease for attempt `attempt` of this call: its
    /// holder, naming its expiry and the call, and the expiry its
    /// candidates get when written (design §8.6).
    fn lease(&self, cx: &CallContext, head: ChangesetId, attempt: u32) -> Lease {
        let expires = self
            .clock
            .now()
            .ms
            .saturating_add(i64::try_from(self.config.land_lease_ms).unwrap_or(i64::MAX));
        let seed = format!(
            "{}\0{}\0{head}\0{attempt}",
            cx.principal.subject, cx.request_id
        );
        let tag = ObjectId::hash(ObjectKind::Blob, seed.as_bytes()).to_hex();
        Lease {
            holder: format!("land:{expires}:{}", &tag[..16]),
            expires_at_ms: expires,
        }
    }

    pub(crate) async fn do_land(
        &self,
        cx: &CallContext,
        mut req: Land,
    ) -> Result<Landed, ApiError> {
        check_ref_name(&req.line)?;
        require(cx, &req.repo, Need::Land(&req.line))?;
        let repo = self.repo_id(&req.repo).await?;
        if req.squash {
            return Err(rejected(
                "squash",
                "landing a stack as one changeset is not built yet: squash the stack with the `Stack` operation and land its head".to_owned(),
            ));
        }
        // Each attempt reads the tip before the land record. A record is
        // written in the transaction of a line move (or guarded by the tip
        // it was computed on), so an attempt that finds no record cannot be
        // racing a land of the same head that already moved past its tip,
        // and never overwrites another attempt's result.
        let mut line = self.line_ref(repo, &req.line).await?;
        // A retry finds the earlier result, whatever has changed since.
        if let Some(done) = self.land_record(repo, req.head, &req.line).await? {
            return Ok(done);
        }
        let config = self.line_config(&req.line);
        if config.authority == Authority::GitPrimary {
            return Err(rejected(
                "authority",
                format!(
                    "line {} is git-primary; land through the PR bridge",
                    req.line
                ),
            ));
        }
        let source = match &req.merged_from {
            Some(from) => Some(
                self.merge_source(cx, repo, &req.repo, &req.line, from)
                    .await?,
            ),
            None => None,
        };
        let (mut head_cs, head_record) = self.changeset(repo, req.head).await?;
        if head_record.state == RecordState::Landed {
            return Err(errors::invalid(None, "draft-landed"));
        }
        let mut stack = self.chain(repo, req.head, line.target).await?;
        for (_, cs) in &stack {
            require_changes(cx, &req.repo, &cs.changes)?;
        }
        // A merge resolution lands as its head stamped with what it merged
        // (`graft.merged-from`, for readers and export), once the caller
        // may land it. The stamp is deterministic, so a retry builds the
        // same head and finds its earlier result. It changes no path, so
        // the checks above hold for it.
        if let Some(stamped) = source.as_ref().and_then(|s| s.stamp(&head_cs)) {
            let id = stamped.id();
            if let Some(done) = self.land_record(repo, id, &req.line).await? {
                return Ok(done);
            }
            req.head = self
                .insert_unheld(repo, stamped.clone(), Vec::new())
                .await?
                .id;
            head_cs = stamped;
            stack = self.chain(repo, req.head, line.target).await?;
        }
        self.pre_checks(repo, &config, req.head, &head_cs, &stack)
            .await?;

        // The lease outlives every land deadline, so a live land never races
        // the collection of its own candidates.
        let start = self.clock.now().ms;
        let lease = i64::try_from(self.config.land_lease_ms).unwrap_or(i64::MAX);
        let wanted = req
            .deadline_ms
            .map_or(lease, |ms| i64::try_from(ms).unwrap_or(i64::MAX).min(lease));
        let mut deadline = start.saturating_add(wanted);
        if let Some(cx_deadline) = cx.deadline_ms {
            deadline = deadline.min(cx_deadline);
        }

        let budget = self.config.land_attempts.max(1);
        for attempt in 0..budget {
            if attempt > 0 {
                line = self.line_ref(repo, &req.line).await?;
                // A concurrent land of the same head may have won.
                if let Some(done) = self.land_record(repo, req.head, &req.line).await? {
                    return Ok(done);
                }
                stack = self.chain(repo, req.head, line.target).await?;
            }
            let outcome = self
                .attempt(
                    cx,
                    repo,
                    &req,
                    &config,
                    &line,
                    &stack,
                    attempt,
                    source.as_ref(),
                )
                .await?;
            match outcome {
                Attempt::Done(landed) => return Ok(landed),
                Attempt::Raced => {}
                Attempt::Lost => {
                    let remaining = deadline.saturating_sub(self.clock.now().ms);
                    if remaining <= 0 || attempt + 1 == budget {
                        break;
                    }
                    let ceiling = backoff_ceiling(
                        self.config.land_backoff_base_ms,
                        self.config.land_backoff_cap_ms,
                        attempt,
                    );
                    let ms = jitter(ceiling, attempt).min(remaining as u64);
                    self.sleeper.sleep(ms).await;
                }
            }
            if self.clock.now().ms >= deadline {
                break;
            }
        }
        let tip = self.line_ref(repo, &req.line).await?.target;
        Err(ApiError::Contended {
            line: req.line,
            tip,
        })
    }

    /// One attempt against the line at `line` (its tip and version).
    #[allow(clippy::too_many_arguments)]
    async fn attempt(
        &self,
        cx: &CallContext,
        repo: RepoId,
        req: &Land,
        config: &LineConfig,
        line: &RefRecord,
        stack: &[(ChangesetId, Changeset)],
        attempt: u32,
        source: Option<&MergeSource>,
    ) -> Result<Attempt, ApiError> {
        let tip = line.target;
        let bottom = stack
            .first()
            .and_then(|(_, cs)| cs.first_parent())
            .ok_or_else(|| errors::invalid(None, "land-root"))?;

        let (candidate, steps, merged_paths) = if bottom == tip {
            (req.head, Vec::new(), Vec::new())
        } else {
            let plan = match self.plan(repo, stack, tip, req.merge, config).await {
                Ok(plan) => plan,
                Err(ApiError::Conflict { paths, .. }) => {
                    let txn = MetaTxn {
                        preconditions: Vec::new(),
                        writes: vec![journal::land_attempt(
                            req.head,
                            &req.line,
                            LandOutcome::Conflict(tip),
                        )],
                    };
                    self.meta.commit(repo, txn).await.map_err(errors::store)?;
                    let truncated = paths.len() > MAX_STALE_PATHS;
                    let paths = paths
                        .into_iter()
                        .take(MAX_STALE_PATHS)
                        .map(|path| path.path)
                        .collect();
                    return Err(ApiError::StaleBase(Box::new(StaleBase {
                        current_tip: tip,
                        paths,
                        truncated,
                    })));
                }
                Err(e) => return Err(e),
            };
            if attest_merged(config) && !plan.merged.is_empty() {
                return Err(rejected(
                    config.policy.name(),
                    merged_detail(&plan.merged, tip),
                ));
            }
            if plan.head != tip {
                let lease = self.lease(cx, req.head, attempt);
                for cs in &plan.new {
                    let uploads = plan.uploads(cs);
                    match self
                        .insert_candidate(repo, cs.clone(), lease.clone(), uploads)
                        .await
                    {
                        Ok(_) => {}
                        // Collected under a stalled attempt: this one fails
                        // like a lost CAS.
                        Err(ApiError::Collected { .. }) => return Ok(Attempt::Raced),
                        Err(e) => return Err(e),
                    }
                }
            }
            (plan.head, plan.steps, plan.merged)
        };

        if let LandPolicy::Queued { checks } = &config.policy
            && candidate != tip
        {
            let verdict = self
                .test_candidate(
                    repo,
                    Candidate {
                        repo: req.repo.clone(),
                        line: req.line.clone(),
                        head: req.head,
                        candidate,
                        tip,
                        checks: checks.clone(),
                    },
                )
                .await?;
            if let Verdict::Fail(detail) = verdict {
                return Err(rejected(
                    "queued",
                    format!("candidate {candidate}: {detail}"),
                ));
            }
        }

        let mut txn = MetaTxn {
            preconditions: vec![Precondition::RefIs {
                name: req.line.clone(),
                expected: Some((tip, line.version)),
            }],
            writes: Vec::new(),
        };
        let mut result = LandResult {
            landed: candidate,
            version: line.version,
            pos: 0,
            rebased: candidate != req.head,
            merged_paths,
        };
        if candidate == tip {
            // Every change is already on the line: nothing moves.
            result.pos = self
                .meta
                .line_pos(repo, &req.line, tip)
                .await
                .map_err(errors::store)?
                .ok_or_else(|| errors::invalid(None, "tip-not-indexed"))?;
        } else {
            let rows = match self
                .line_rows(
                    repo,
                    &req.line,
                    Some(tip),
                    candidate,
                    &mut txn.preconditions,
                )
                .await
            {
                Ok(rows) => rows,
                Err(ApiError::Collected { .. }) => return Ok(Attempt::Raced),
                Err(e) => return Err(e),
            };
            result.pos = rows
                .iter()
                .find_map(|w| match w {
                    Write::PutLinePos { changeset, pos, .. } if *changeset == candidate => {
                        Some(*pos)
                    }
                    _ => None,
                })
                .ok_or_else(|| errors::invalid(None, "candidate-not-indexed"))?;
            result.version = line.version + 1;
            txn.writes = rows;
        }
        self.supersede(repo, &steps, tip, &mut txn).await?;
        // On a post-land checked line a landing that moves the line gets a
        // `pending` land-checks row, and a revert marks what it reverts
        // (D25, D37). Neither ever refuses or delays the land.
        let checked = config.checks.is_post_land() && candidate != tip;
        if checked {
            let reverted = self.revert_writes(repo, config, stack, &steps).await?;
            txn.writes.extend(reverted);
        }
        txn.writes.push(Write::PutLand {
            draft: req.head,
            line: req.line.clone(),
            landed: candidate,
            pos: checked.then_some(result.pos),
            checked,
        });
        txn.writes.push(Write::PutDerived {
            kind: LAND_RESULT.to_owned(),
            version: KIND_VERSION,
            key: key(req.head, &req.line),
            value: result.encode(),
        });
        if candidate != tip {
            txn.writes.push(Write::SetRef(RefRecord {
                name: req.line.clone(),
                target: candidate,
                version: result.version,
                kind: RefKind::Line,
            }));
            txn.writes.push(journal::ref_moved(
                &req.line,
                Some(tip),
                candidate,
                result.version,
                &cx.principal.subject,
            ));
            // A merge landing records its merge point with the move (D29).
            if let Some(src) = source {
                txn.writes.extend(src.writes(&req.line, result.pos));
            }
        }
        txn.writes.push(journal::land_attempt(
            req.head,
            &req.line,
            LandOutcome::Landed(candidate),
        ));

        match self.meta.commit(repo, txn).await.map_err(errors::store)? {
            TxnOutcome::Committed(_) => {}
            TxnOutcome::PreconditionFailed(0) => return Ok(Attempt::Lost),
            TxnOutcome::PreconditionFailed(_) => return Ok(Attempt::Raced),
        }
        if let (Some(post), true) = (&config.post_land, candidate != tip) {
            self.hooks.landed(&LandedEvent {
                repo: req.repo.clone(),
                line: req.line.clone(),
                head: req.head,
                landed: candidate,
                version: result.version,
                checks: post.checks.clone(),
            });
        }
        Ok(Attempt::Done(result.landed()))
    }

    /// Marks the drafts a land's rebase replaced as superseded, each by its
    /// successor, or (when all its changes were already there) by the
    /// successor below it or the tip. Each move is guarded by the record's
    /// state.
    async fn supersede(
        &self,
        repo: RepoId,
        steps: &[Step],
        tip: ChangesetId,
        txn: &mut MetaTxn,
    ) -> Result<(), ApiError> {
        let mut below = tip;
        let expires = self.retained_until(self.clock.now().ms);
        for step in steps {
            let successor = step.to.unwrap_or(below);
            if step.to != Some(step.from) {
                let (_, record) = self.changeset(repo, step.from).await?;
                if record.state != RecordState::Landed {
                    transition(
                        step.from,
                        record,
                        RecordState::Superseded,
                        None,
                        Some(successor),
                        expires,
                        txn,
                    );
                }
            }
            below = successor;
        }
        Ok(())
    }

    /// Records a check result: the pre-v0.2 `Attest`, as a `check`
    /// attestation with status `pass` or `fail` (see
    /// [`Engine::do_attest_v2`]).
    pub(crate) async fn do_attest(&self, cx: &CallContext, req: Attest) -> Result<(), ApiError> {
        self.do_attest_v2(cx, req.into()).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn land_results_round_trip() {
        let result = LandResult {
            landed: ChangesetId(ObjectId([3; 32])),
            version: 7,
            pos: 42,
            rebased: true,
            merged_paths: vec![RepoPath::new("a/b").expect("path")],
        };
        let bytes = result.encode();
        assert_eq!(LandResult::decode(&bytes), Some(result.clone()));
        assert_eq!(LandResult::decode(&bytes[..bytes.len() - 1]), None);
        let mut longer = bytes.clone();
        longer.push(0);
        assert_eq!(LandResult::decode(&longer), None);
        // The format is durable: pin its bytes.
        let mut want = vec![1];
        want.extend_from_slice(&[3; 32]);
        want.extend_from_slice(&7u64.to_be_bytes());
        want.extend_from_slice(&42u64.to_be_bytes());
        want.push(1);
        want.extend_from_slice(&1u32.to_be_bytes());
        want.extend_from_slice(&3u32.to_be_bytes());
        want.extend_from_slice(b"a/b");
        assert_eq!(bytes, want);
    }

    #[test]
    fn attestations_round_trip() {
        let bytes = encode_attestation(false, PrincipalKind::Ci, 5, "https://ci/1", "bot");
        assert_eq!(
            decode_attestation(&bytes),
            Some(Attestation {
                passed: false,
                kind: PrincipalKind::Ci,
                url: "https://ci/1".to_owned(),
            })
        );
        let mut want = vec![1, 0, 2];
        want.extend_from_slice(&5i64.to_be_bytes());
        want.extend_from_slice(&12u32.to_be_bytes());
        want.extend_from_slice(b"https://ci/1");
        want.extend_from_slice(&3u32.to_be_bytes());
        want.extend_from_slice(b"bot");
        assert_eq!(bytes, want);
        assert_eq!(decode_attestation(&bytes[1..]), None);
    }

    #[test]
    fn backoff_doubles_from_the_base_to_the_cap() {
        let ceilings: Vec<u64> = (0..9).map(|a| backoff_ceiling(2, 200, a)).collect();
        assert_eq!(ceilings, [2, 4, 8, 16, 32, 64, 128, 200, 200]);
        assert_eq!(backoff_ceiling(2, 200, u32::MAX), 200);
        for attempt in 0..64 {
            assert!(jitter(8, attempt) <= 8);
        }
        assert_eq!(jitter(0, 3), 0);
    }

    #[test]
    fn check_names() {
        for ok in ["ci/test", "review/human", &"x".repeat(MAX_CHECK_NAME)] {
            assert!(check_check_name(ok).is_ok(), "{ok}");
        }
        for bad in ["", "ci test", "ci\ntest", &"x".repeat(MAX_CHECK_NAME + 1)] {
            assert!(check_check_name(bad).is_err(), "{bad:?}");
        }
    }

    #[test]
    fn a_zero_sleep_is_ready_at_once() {
        let mut sleep = ThreadSleeper.sleep(0);
        let waker = Waker::noop();
        let mut cx = Context::from_waker(waker);
        assert!(sleep.as_mut().poll(&mut cx).is_ready());
    }
}
