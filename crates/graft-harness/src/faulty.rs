//! Fault-injecting wrappers over the storage ports (`Faulty<M>` and
//! `Faulty<O>` in the design note, §3.1).
//!
//! Each call asks the shared [`Faults`] what to do: pass through, yield to
//! other tasks first, fail before the write, fail *after* the write (the
//! write commits but the caller sees an error: a lost acknowledgement),
//! crash the process at the commit, or refuse an object for want of space.
//!
//! A decision is keyed by what the call carries (the transaction's
//! contents, the object id) and how many times that same call was seen,
//! hashed with the seed, so it does not depend on the order in which the
//! engine issues calls. Every error fault that fires is logged with its key,
//! so the shrinker can switch faults off one at a time.
//!
//! The wrappers also observe: every committed transaction's ref writes and
//! their expected versions (the CAS check, §3.3 property 3), every record
//! the sweep collects, with the record and protection it replaced (the GC
//! check, property 6), and every object deleted.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::{Arc, Mutex, MutexGuard};

use graft_model::{ChangesetId, Clock, ObjectId, RepoPath};
use graft_store::{
    ChangesetRecord, DoomMark, Lsn, MetaStore, MetaTxn, ObjectStore, Precondition, Protection,
    RecordState, RefRecord, RepoId, StoreError, StoreResult, StoreStats, TimedRow, TxnOutcome,
    WorkspaceRecord, Write,
};
use graft_testkit::clock::FakeClock;

use crate::exec::Yield;
use crate::rng::{mix, stable_hash, unit};

/// How often each fault fires, per call.
#[derive(Clone, Debug, PartialEq)]
pub struct FaultRates {
    /// Yield to other tasks before the call.
    pub delay: f64,
    /// Most yields per delay.
    pub max_yields: u32,
    /// A metadata read or commit fails, nothing written.
    pub fail_before: f64,
    /// A commit is applied, then reported as failed (lost ack).
    pub lost_ack: f64,
    /// The process dies at a commit, before or after applying it.
    pub crash: f64,
    /// Object puts fail, nothing written, from one commit to the next.
    /// The engine writes a rebase's objects in hash-map order, so a fault
    /// on one put out of several would make the run depend on that order;
    /// a window where every put fails does not.
    pub put_fail: f64,
}

impl FaultRates {
    /// No faults and no delays.
    pub fn none() -> Self {
        Self {
            delay: 0.0,
            max_yields: 0,
            fail_before: 0.0,
            lost_ack: 0.0,
            crash: 0.0,
            put_fail: 0.0,
        }
    }

    /// Interleaving only.
    pub fn delays() -> Self {
        Self {
            delay: 0.3,
            max_yields: 3,
            ..Self::none()
        }
    }

    /// The default adversary: delays, errors, lost acks and crashes.
    pub fn hostile() -> Self {
        Self {
            delay: 0.3,
            max_yields: 3,
            fail_before: 0.02,
            lost_ack: 0.04,
            crash: 0.01,
            put_fail: 0.03,
        }
    }
}

/// A bug planted in the wrappers rather than in the engine (§3.4).
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum WrapperBug {
    /// Line ref writes skip the CAS: the wrapper rewrites the expected
    /// version to whatever is stored now, so a stale write wins.
    NonCasRef,
    /// Some object puts are acknowledged but the bytes are discarded.
    DroppedPut,
}

/// The kind of a fault that fired.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum FaultKind {
    /// Failed before writing.
    FailBefore,
    /// Written, then reported as failed.
    LostAck,
    /// Crashed before applying a commit.
    CrashBefore,
    /// Crashed after applying a commit.
    CrashAfter,
    /// Refused for want of space.
    Full,
    /// A put acknowledged and discarded ([`WrapperBug::DroppedPut`]).
    Dropped,
}

/// A fault that fired.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Fired {
    /// The decision key: switch it off to remove this fault.
    pub key: u64,
    /// What happened.
    pub kind: FaultKind,
    /// The call, briefly.
    pub call: String,
}

/// A ref write seen in a committed transaction.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RefWrite {
    /// The ref.
    pub name: String,
    /// The `RefIs` the engine sent (before any planted bug rewrote it).
    pub expected: Option<Option<(ChangesetId, u64)>>,
    /// The new target and version.
    pub new: (ChangesetId, u64),
}

/// A committed transaction, as far as the checks care.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CommittedTxn {
    /// Its ref writes, in order.
    pub refs: Vec<RefWrite>,
    /// Its journal entries: LSN, kind and body, in order.
    pub journal: Vec<(u64, String, Vec<u8>)>,
}

/// A record the sweep collected.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Collection {
    /// The changeset.
    pub id: ChangesetId,
    /// Its record before.
    pub record: ChangesetRecord,
    /// Its protection before.
    pub protection: Protection,
    /// The clock at the commit, ms.
    pub now_ms: i64,
    /// The holding workspace's lease end, when a workspace held it.
    pub workspace_lease_ms: Option<i64>,
}

/// The adversary's shared state.
#[derive(Debug)]
struct State {
    seed: u64,
    rates: FaultRates,
    enabled: bool,
    disabled: BTreeSet<u64>,
    seen: BTreeMap<u64, u32>,
    fired: Vec<Fired>,
    crashed: bool,
    full: bool,
    commits: u64,
    bugs: BTreeSet<WrapperBug>,
    txns: Vec<CommittedTxn>,
    collections: Vec<Collection>,
    deletes: Vec<ObjectId>,
    on_ref_write: Option<FaultKind>,
}

/// What a call does, as decided.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Action {
    Pass,
    FailBefore,
    LostAck,
    CrashBefore,
    CrashAfter,
}

/// The kind of call, for its decision.
#[derive(Clone, Copy, Debug)]
enum Call {
    Read,
    Commit,
    Put,
}

/// The adversary shared by both wrappers. Clones share one state.
#[derive(Clone, Debug)]
pub struct Faults {
    state: Arc<Mutex<State>>,
    clock: Arc<FakeClock>,
}

impl Faults {
    /// An adversary seeded with `seed`, firing at `rates`, reading `clock`.
    pub fn new(seed: u64, rates: FaultRates, clock: Arc<FakeClock>) -> Self {
        Self {
            state: Arc::new(Mutex::new(State {
                seed,
                rates,
                enabled: true,
                disabled: BTreeSet::new(),
                seen: BTreeMap::new(),
                fired: Vec::new(),
                crashed: false,
                full: false,
                commits: 0,
                bugs: BTreeSet::new(),
                txns: Vec::new(),
                collections: Vec::new(),
                deletes: Vec::new(),
                on_ref_write: None,
            })),
            clock,
        }
    }

    fn st(&self) -> MutexGuard<'_, State> {
        self.state.lock().expect("fault state")
    }

    /// Switches every fault and delay on or off (off for setup and checks).
    pub fn set_enabled(&self, on: bool) {
        self.st().enabled = on;
    }

    /// Never fires the faults with these keys (the shrinker's switch).
    pub fn disable(&self, keys: impl IntoIterator<Item = u64>) {
        self.st().disabled.extend(keys);
    }

    /// Plants a wrapper bug.
    pub fn plant(&self, bug: WrapperBug) {
        self.st().bugs.insert(bug);
    }

    /// Refuses every object put with `StoreError::Full` while `full`.
    pub fn set_full(&self, full: bool) {
        self.st().full = full;
    }

    /// Fires `kind` (`CrashBefore`, `CrashAfter` or `LostAck`) once, at the
    /// next commit that writes a ref: a fault placed by hand, for probes.
    pub fn at_next_ref_write(&self, kind: FaultKind) {
        self.st().on_ref_write = Some(kind);
    }

    /// Whether the process has crashed since the last restart.
    pub fn crashed(&self) -> bool {
        self.st().crashed
    }

    /// Restarts after a crash.
    pub fn restart(&self) {
        self.st().crashed = false;
    }

    /// The faults that fired.
    pub fn fired(&self) -> Vec<Fired> {
        self.st().fired.clone()
    }

    /// The committed transactions that wrote refs.
    pub fn txns(&self) -> Vec<CommittedTxn> {
        self.st().txns.clone()
    }

    /// The records the sweep collected.
    pub fn collections(&self) -> Vec<Collection> {
        self.st().collections.clone()
    }

    /// The objects deleted.
    pub fn deletes(&self) -> Vec<ObjectId> {
        self.st().deletes.clone()
    }

    fn now(&self) -> i64 {
        self.clock.now().ms
    }

    /// Decides a call: how long to yield first, and what to do.
    fn decide(&self, call: Call, what: &str) -> (u32, Action, u64) {
        let mut st = self.st();
        if !st.enabled {
            return (0, Action::Pass, 0);
        }
        let base = stable_hash(what.as_bytes());
        let n = {
            let n = st.seen.entry(base).or_insert(0);
            *n += 1;
            *n
        };
        let key = mix(st.seed ^ mix(base ^ u64::from(n).wrapping_mul(0x51_7cc1_b727_220a)));
        if matches!(call, Call::Commit) {
            st.commits += 1;
        }
        let epoch_key = mix(st.seed ^ mix(st.commits ^ 0x7075_7473));
        let r = &st.rates;
        let yields = if unit(mix(key ^ 1)) < r.delay {
            1 + (mix(key ^ 2) % u64::from(r.max_yields.max(1))) as u32
        } else {
            0
        };
        let roll = unit(mix(key ^ 3));
        let action = match call {
            Call::Read => {
                if roll < r.fail_before / 2.0 {
                    Action::FailBefore
                } else {
                    Action::Pass
                }
            }
            Call::Commit => {
                if roll < r.fail_before {
                    Action::FailBefore
                } else if roll < r.fail_before + r.lost_ack {
                    Action::LostAck
                } else if roll < r.fail_before + r.lost_ack + r.crash / 2.0 {
                    Action::CrashBefore
                } else if roll < r.fail_before + r.lost_ack + r.crash {
                    Action::CrashAfter
                } else {
                    Action::Pass
                }
            }
            Call::Put => {
                if unit(epoch_key) < r.put_fail {
                    Action::FailBefore
                } else {
                    Action::Pass
                }
            }
        };
        let key = if matches!(call, Call::Put) {
            epoch_key
        } else {
            key
        };
        let action = if action != Action::Pass && st.disabled.contains(&key) {
            Action::Pass
        } else {
            action
        };
        (yields, action, key)
    }

    fn fire(&self, key: u64, kind: FaultKind, call: &str) {
        let mut st = self.st();
        if matches!(kind, FaultKind::CrashBefore | FaultKind::CrashAfter) {
            st.crashed = true;
        }
        st.fired.push(Fired {
            key,
            kind,
            call: call.chars().take(80).collect(),
        });
    }

    fn dead() -> StoreError {
        StoreError::Backend("harness: process crashed".to_owned())
    }

    /// Yields, then fails a read if the process is down or the read is
    /// chosen to fail.
    async fn gate_read(&self, what: String) -> StoreResult<()> {
        let (yields, action, key) = self.decide(Call::Read, &what);
        Yield(yields).await;
        if self.crashed() {
            return Err(Self::dead());
        }
        if action == Action::FailBefore {
            self.fire(key, FaultKind::FailBefore, &what);
            return Err(StoreError::Busy(5));
        }
        Ok(())
    }
}

/// The CAS-carrying contents of a transaction, before a bug rewrites it.
fn ref_writes(txn: &MetaTxn) -> Vec<RefWrite> {
    txn.writes
        .iter()
        .filter_map(|w| match w {
            Write::SetRef(r) => Some(RefWrite {
                name: r.name.clone(),
                expected: txn.preconditions.iter().find_map(|p| match p {
                    Precondition::RefIs { name, expected } if *name == r.name => Some(*expected),
                    _ => None,
                }),
                new: (r.target, r.version),
            }),
            _ => None,
        })
        .collect()
}

/// A key for a transaction that does not depend on the order of its parts.
fn txn_key(txn: &MetaTxn) -> String {
    let mut parts: Vec<String> = txn
        .preconditions
        .iter()
        .map(|p| format!("{p:?}"))
        .chain(txn.writes.iter().map(|w| format!("{w:?}")))
        .collect();
    parts.sort();
    scrub_attempts(&format!("commit {}", parts.join("|")))
}

/// Masks the random nonce in land-attempt holders (`land:<ms>:<nonce>`), so
/// the decision for a transaction does not change from run to run.
fn scrub_attempts(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut rest = s;
    while let Some(at) = rest.find("land:") {
        let (head, tail) = rest.split_at(at + "land:".len());
        out.push_str(head);
        let digits = tail.bytes().take_while(u8::is_ascii_digit).count();
        out.push_str(&tail[..digits]);
        let tail = &tail[digits..];
        match tail.strip_prefix(':') {
            Some(after) => {
                let nonce = after.bytes().take_while(u8::is_ascii_hexdigit).count();
                out.push_str(":?");
                rest = &after[nonce..];
            }
            None => rest = tail,
        }
    }
    out.push_str(rest);
    out
}

/// The metadata store under faults.
#[derive(Clone, Debug)]
pub struct FaultyMeta<M> {
    inner: M,
    faults: Faults,
}

impl<M: MetaStore> FaultyMeta<M> {
    /// Wraps `inner`.
    pub fn new(inner: M, faults: Faults) -> Self {
        Self { inner, faults }
    }

    /// The store underneath, without faults.
    pub fn inner(&self) -> &M {
        &self.inner
    }

    /// Rewrites a line ref write to skip its CAS ([`WrapperBug::NonCasRef`]).
    async fn skip_cas(&self, repo: RepoId, txn: &mut MetaTxn) -> StoreResult<()> {
        let Some(name) = txn.writes.iter().find_map(|w| match w {
            Write::SetRef(r) if r.kind == graft_store::RefKind::Line => Some(r.name.clone()),
            _ => None,
        }) else {
            return Ok(());
        };
        let now = self.inner.get_ref(repo, &name).await?;
        let current = now.as_ref().map(|r| (r.target, r.version));
        for p in &mut txn.preconditions {
            if let Precondition::RefIs { name: n, expected } = p
                && *n == name
            {
                *expected = current;
            }
        }
        for w in &mut txn.writes {
            if let Write::SetRef(r) = w
                && r.name == name
            {
                r.version = current.map_or(1, |(_, v)| v + 1);
            }
        }
        Ok(())
    }

    /// The collections in `txn`, with what they replaced.
    async fn collections(&self, repo: RepoId, txn: &MetaTxn) -> StoreResult<Vec<Collection>> {
        let mut out = Vec::new();
        for w in &txn.writes {
            let Write::PutChangeset { id, record, .. } = w else {
                continue;
            };
            if record.state != RecordState::Collected {
                continue;
            }
            let Some((record, protection)) = txn.preconditions.iter().find_map(|p| match p {
                Precondition::RecordIs {
                    id: pid,
                    record,
                    protection,
                } if pid == id => Some((record.clone(), *protection)),
                _ => None,
            }) else {
                continue;
            };
            let workspace_lease_ms =
                match record.holder.as_deref().and_then(|h| h.strip_prefix("ws:")) {
                    Some(name) => self
                        .inner
                        .workspace(repo, name)
                        .await?
                        .map(|w| w.lease_until_ms),
                    None => None,
                };
            out.push(Collection {
                id: *id,
                record,
                protection,
                now_ms: self.faults.now(),
                workspace_lease_ms,
            });
        }
        Ok(out)
    }
}

macro_rules! read {
    ($self:ident, $what:expr, $call:expr) => {{
        $self.faults.gate_read($what).await?;
        $call.await
    }};
}

impl<M: MetaStore> MetaStore for FaultyMeta<M> {
    async fn create_repo(&self, name: &str) -> StoreResult<RepoId> {
        read!(
            self,
            format!("create_repo {name}"),
            self.inner.create_repo(name)
        )
    }
    async fn repo(&self, name: &str) -> StoreResult<Option<RepoId>> {
        read!(self, format!("repo {name}"), self.inner.repo(name))
    }
    async fn commit(&self, repo: RepoId, mut txn: MetaTxn) -> StoreResult<TxnOutcome> {
        let what = txn_key(&txn);
        let (yields, mut action, key) = self.faults.decide(Call::Commit, &what);
        let placed = {
            let mut st = self.faults.st();
            let writes_ref = txn.writes.iter().any(|w| matches!(w, Write::SetRef(_)));
            if writes_ref {
                st.on_ref_write.take()
            } else {
                None
            }
        };
        match placed {
            Some(FaultKind::CrashBefore) => action = Action::CrashBefore,
            Some(FaultKind::CrashAfter) => action = Action::CrashAfter,
            Some(FaultKind::LostAck) => action = Action::LostAck,
            _ => {}
        }
        Yield(yields).await;
        if self.faults.crashed() {
            return Err(Faults::dead());
        }
        match action {
            Action::FailBefore => {
                self.faults.fire(key, FaultKind::FailBefore, &what);
                return Err(StoreError::Busy(5));
            }
            Action::CrashBefore => {
                self.faults.fire(key, FaultKind::CrashBefore, &what);
                return Err(Faults::dead());
            }
            _ => {}
        }
        let refs = ref_writes(&txn);
        if self.faults.st().bugs.contains(&WrapperBug::NonCasRef) {
            self.skip_cas(repo, &mut txn).await?;
        }
        let collections = self.collections(repo, &txn).await?;
        let entries: Vec<(String, Vec<u8>)> = txn
            .writes
            .iter()
            .filter_map(|w| match w {
                Write::Journal { kind, body } => Some((kind.clone(), body.clone())),
                _ => None,
            })
            .collect();
        let out = self.inner.commit(repo, txn).await?;
        if let TxnOutcome::Committed(lsns) = &out {
            let mut st = self.faults.st();
            if !refs.is_empty() {
                let journal = lsns
                    .iter()
                    .zip(entries)
                    .map(|(l, (k, b))| (*l, k, b))
                    .collect();
                st.txns.push(CommittedTxn { refs, journal });
            }
            st.collections.extend(collections);
        }
        match action {
            Action::LostAck => {
                self.faults.fire(key, FaultKind::LostAck, &what);
                Err(StoreError::Backend("harness: lost ack".to_owned()))
            }
            Action::CrashAfter => {
                self.faults.fire(key, FaultKind::CrashAfter, &what);
                Err(Faults::dead())
            }
            _ => Ok(out),
        }
    }
    async fn changeset(
        &self,
        repo: RepoId,
        id: ChangesetId,
    ) -> StoreResult<Option<(Vec<u8>, ChangesetRecord)>> {
        read!(
            self,
            format!("changeset {id}"),
            self.inner.changeset(repo, id)
        )
    }
    async fn get_ref(&self, repo: RepoId, name: &str) -> StoreResult<Option<RefRecord>> {
        read!(
            self,
            format!("get_ref {name}"),
            self.inner.get_ref(repo, name)
        )
    }
    async fn list_refs(
        &self,
        repo: RepoId,
        prefix: &str,
        limit: u32,
    ) -> StoreResult<Vec<RefRecord>> {
        read!(
            self,
            format!("list_refs {prefix} {limit}"),
            self.inner.list_refs(repo, prefix, limit)
        )
    }
    async fn workspace(&self, repo: RepoId, name: &str) -> StoreResult<Option<WorkspaceRecord>> {
        read!(
            self,
            format!("workspace {name}"),
            self.inner.workspace(repo, name)
        )
    }
    async fn expired_workspaces(
        &self,
        repo: RepoId,
        before_ms: i64,
        limit: u32,
    ) -> StoreResult<Vec<WorkspaceRecord>> {
        read!(
            self,
            format!("expired_workspaces {before_ms} {limit}"),
            self.inner.expired_workspaces(repo, before_ms, limit)
        )
    }
    async fn list_workspaces(
        &self,
        repo: RepoId,
        owner: &str,
        after: &str,
        limit: u32,
    ) -> StoreResult<Vec<WorkspaceRecord>> {
        read!(
            self,
            format!("list_workspaces {owner} {after} {limit}"),
            self.inner.list_workspaces(repo, owner, after, limit)
        )
    }
    async fn count_records(&self, repo: RepoId, state: RecordState) -> StoreResult<u64> {
        read!(
            self,
            format!("count_records {state:?}"),
            self.inner.count_records(repo, state)
        )
    }
    async fn count_workspaces(&self, repo: RepoId, before_ms: Option<i64>) -> StoreResult<u64> {
        read!(
            self,
            format!("count_workspaces {before_ms:?}"),
            self.inner.count_workspaces(repo, before_ms)
        )
    }
    async fn derived(
        &self,
        repo: RepoId,
        kind: &str,
        version: u32,
        key: &[u8],
    ) -> StoreResult<Option<Vec<u8>>> {
        read!(
            self,
            format!("derived {kind} {version} {key:?}"),
            self.inner.derived(repo, kind, version, key)
        )
    }
    async fn line_pos(
        &self,
        repo: RepoId,
        line: &str,
        id: ChangesetId,
    ) -> StoreResult<Option<u64>> {
        read!(
            self,
            format!("line_pos {line} {id}"),
            self.inner.line_pos(repo, line, id)
        )
    }
    async fn line_holders(
        &self,
        repo: RepoId,
        id: ChangesetId,
        limit: u32,
    ) -> StoreResult<Vec<(String, u64)>> {
        read!(
            self,
            format!("line_holders {id} {limit}"),
            self.inner.line_holders(repo, id, limit)
        )
    }
    async fn scan_line_path(
        &self,
        repo: RepoId,
        line: &str,
        path: &RepoPath,
        before: u64,
        limit: u32,
    ) -> StoreResult<Vec<(u64, ChangesetId)>> {
        read!(
            self,
            format!("scan_line_path {line} {path} {before} {limit}"),
            self.inner.scan_line_path(repo, line, path, before, limit)
        )
    }
    async fn holders(&self, repo: RepoId, object: ObjectId) -> StoreResult<u32> {
        read!(
            self,
            format!("holders {object:?}"),
            self.inner.holders(repo, object)
        )
    }
    async fn land(
        &self,
        repo: RepoId,
        draft: ChangesetId,
        line: &str,
    ) -> StoreResult<Option<ChangesetId>> {
        read!(
            self,
            format!("land {draft} {line}"),
            self.inner.land(repo, draft, line)
        )
    }
    async fn journal(
        &self,
        repo: RepoId,
        from: Lsn,
        limit: u32,
    ) -> StoreResult<Vec<(Lsn, String, Vec<u8>)>> {
        read!(
            self,
            format!("journal {from} {limit}"),
            self.inner.journal(repo, from, limit)
        )
    }
    async fn list_records(
        &self,
        repo: RepoId,
        state: RecordState,
        after: Option<ChangesetId>,
        limit: u32,
    ) -> StoreResult<Vec<(ChangesetId, ChangesetRecord)>> {
        read!(
            self,
            format!("list_records {state:?} {after:?} {limit}"),
            self.inner.list_records(repo, state, after, limit)
        )
    }
    async fn list_all_workspaces(
        &self,
        repo: RepoId,
        after: &str,
        limit: u32,
    ) -> StoreResult<Vec<WorkspaceRecord>> {
        read!(
            self,
            format!("list_all_workspaces {after} {limit}"),
            self.inner.list_all_workspaces(repo, after, limit)
        )
    }
    async fn holder_rows(
        &self,
        repo: RepoId,
        object: ObjectId,
        after: Option<String>,
        limit: u32,
    ) -> StoreResult<Vec<String>> {
        read!(
            self,
            format!("holder_rows {object:?} {after:?} {limit}"),
            self.inner.holder_rows(repo, object, after, limit)
        )
    }
    async fn protection(&self, repo: RepoId, id: ChangesetId) -> StoreResult<Option<Protection>> {
        read!(
            self,
            format!("protection {id}"),
            self.inner.protection(repo, id)
        )
    }
    async fn expired_records(
        &self,
        repo: RepoId,
        now_ms: i64,
        after: Option<(i64, ChangesetId)>,
        limit: u32,
    ) -> StoreResult<Vec<(i64, ChangesetId)>> {
        read!(
            self,
            format!("expired_records {now_ms} {after:?} {limit}"),
            self.inner.expired_records(repo, now_ms, after, limit)
        )
    }
    async fn held_objects(
        &self,
        repo: RepoId,
        holder: &str,
        after: Option<ObjectId>,
        limit: u32,
    ) -> StoreResult<Vec<ObjectId>> {
        read!(
            self,
            format!("held_objects {holder} {after:?} {limit}"),
            self.inner.held_objects(repo, holder, after, limit)
        )
    }
    async fn expired_rows(
        &self,
        repo: RepoId,
        now_ms: i64,
        after: Option<TimedRow>,
        limit: u32,
    ) -> StoreResult<Vec<TimedRow>> {
        read!(
            self,
            format!("expired_rows {now_ms} {after:?} {limit}"),
            self.inner.expired_rows(repo, now_ms, after, limit)
        )
    }
    async fn doom_marks(
        &self,
        repo: RepoId,
        ids: &[ObjectId],
    ) -> StoreResult<Vec<(ObjectId, DoomMark)>> {
        read!(
            self,
            format!("doom_marks {ids:?}"),
            self.inner.doom_marks(repo, ids)
        )
    }
    async fn old_doomed(
        &self,
        repo: RepoId,
        until_ms: i64,
        after: Option<(i64, ObjectId)>,
        limit: u32,
    ) -> StoreResult<Vec<(ObjectId, DoomMark)>> {
        read!(
            self,
            format!("old_doomed {until_ms} {after:?} {limit}"),
            self.inner.old_doomed(repo, until_ms, after, limit)
        )
    }
    async fn lapsed_workspaces(
        &self,
        repo: RepoId,
        before_ms: i64,
        after: Option<(i64, String)>,
        limit: u32,
    ) -> StoreResult<Vec<WorkspaceRecord>> {
        read!(
            self,
            format!("lapsed_workspaces {before_ms} {after:?} {limit}"),
            self.inner.lapsed_workspaces(repo, before_ms, after, limit)
        )
    }
    async fn scan_draft_paths(
        &self,
        repo: RepoId,
        line: &str,
        prefix: &RepoPath,
        after: Option<(RepoPath, String)>,
        limit: u32,
    ) -> StoreResult<Vec<(RepoPath, String)>> {
        read!(
            self,
            format!("scan_draft_paths {line} {prefix} {after:?} {limit}"),
            self.inner
                .scan_draft_paths(repo, line, prefix, after, limit)
        )
    }
    async fn workspace_draft_paths(
        &self,
        repo: RepoId,
        workspace: &str,
        after: Option<RepoPath>,
        limit: u32,
    ) -> StoreResult<Vec<RepoPath>> {
        read!(
            self,
            format!("workspace_draft_paths {workspace} {after:?} {limit}"),
            self.inner
                .workspace_draft_paths(repo, workspace, after, limit)
        )
    }
    async fn gc_protection(&self, repo: RepoId) -> StoreResult<bool> {
        read!(
            self,
            "gc_protection".to_owned(),
            self.inner.gc_protection(repo)
        )
    }
}

/// The object store under faults.
#[derive(Clone, Debug)]
pub struct FaultyObjects<O> {
    inner: O,
    faults: Faults,
}

impl<O: ObjectStore> FaultyObjects<O> {
    /// Wraps `inner`.
    pub fn new(inner: O, faults: Faults) -> Self {
        Self { inner, faults }
    }

    /// The store underneath, without faults.
    pub fn inner(&self) -> &O {
        &self.inner
    }
}

impl<O: ObjectStore> ObjectStore for FaultyObjects<O> {
    async fn put(&self, repo: RepoId, id: ObjectId, frame: Vec<u8>) -> StoreResult<()> {
        let what = format!("put {id:?}");
        let (yields, action, key) = self.faults.decide(Call::Put, &what);
        Yield(yields).await;
        if self.faults.crashed() {
            return Err(Faults::dead());
        }
        let full = {
            let st = self.faults.st();
            st.enabled && st.full
        };
        if full {
            self.faults.fire(key, FaultKind::Full, &what);
            return Err(StoreError::Full);
        }
        if action == Action::FailBefore {
            self.faults.fire(key, FaultKind::FailBefore, &what);
            return Err(StoreError::Busy(5));
        }
        let drop_it = {
            let st = self.faults.st();
            st.enabled
                && st.bugs.contains(&WrapperBug::DroppedPut)
                && unit(mix(st.seed ^ stable_hash(what.as_bytes()))) < 0.25
        };
        if drop_it {
            self.faults.fire(key, FaultKind::Dropped, &what);
            return Ok(());
        }
        self.inner.put(repo, id, frame).await
    }
    async fn get(&self, repo: RepoId, id: ObjectId) -> StoreResult<Option<Vec<u8>>> {
        read!(self, format!("get {id:?}"), self.inner.get(repo, id))
    }
    async fn has(&self, repo: RepoId, ids: &[ObjectId]) -> StoreResult<Vec<ObjectId>> {
        read!(self, format!("has {ids:?}"), self.inner.has(repo, ids))
    }
    async fn delete(&self, repo: RepoId, id: ObjectId) -> StoreResult<()> {
        self.faults.gate_read(format!("delete {id:?}")).await?;
        self.inner.delete(repo, id).await?;
        self.faults.st().deletes.push(id);
        Ok(())
    }
    async fn stats(&self) -> StoreResult<StoreStats> {
        read!(self, "stats".to_owned(), self.inner.stats())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn land_attempt_nonces_are_masked() {
        assert_eq!(
            scrub_attempts("a land:17:9f3c b land:18:x land:"),
            "a land:17:? b land:18:?x land:"
        );
    }
}
