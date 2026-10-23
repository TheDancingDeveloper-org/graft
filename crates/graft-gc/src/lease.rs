//! The per-repository sweep lease: one sweeper per repository at a time.
//!
//! The lease is a derived value in the metadata store, taken and renewed by
//! compare-and-swap, so two processes on one store (the server and an
//! `admin gc`, or two servers) never sweep a repository at once. Its value
//! is the holder's owner string and the time it lapses: a sweeper that dies
//! holding it blocks the repository only until then. An in-process set in
//! front of it keeps two collectors of one process from racing the CAS.
//!
//! A second sweeper would still be safe, because every step is guarded by
//! its own preconditions; it would only be wasteful (design §8.6).

use std::collections::HashSet;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use graft_store::{MetaStore, MetaTxn, Precondition, RepoId, StoreResult, TxnOutcome, Write};

/// The derived-value kind of the sweep lease.
const LEASE_KIND: &str = "gc-lease";

/// The leases of the repositories being swept, and who this process is.
/// Clones share the set, so every collector of one process should share
/// one.
#[derive(Clone, Debug)]
pub struct SweepLeases {
    held: Arc<Mutex<HashSet<RepoId>>>,
    owner: String,
}

impl Default for SweepLeases {
    fn default() -> Self {
        Self::new()
    }
}

impl SweepLeases {
    /// No leases held, owned by this process: the host name, a random
    /// nonce drawn once per process, the pid, and a counter so that two
    /// lease sets of one process are distinct owners too. The pid alone is
    /// not unique: two containers sharing one store are both pid 1. This is
    /// a lease owner, never a durable id, so randomness is fine here.
    pub fn new() -> Self {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let n = NEXT.fetch_add(1, Ordering::Relaxed);
        Self::with_owner(format!(
            "{}-{:016x}-pid{}-{n}",
            host_name(),
            process_nonce(),
            std::process::id()
        ))
    }

    /// No leases held, owned by `owner` (tests, and a host-qualified name).
    pub fn with_owner(owner: impl Into<String>) -> Self {
        Self {
            held: Arc::default(),
            owner: owner.into(),
        }
    }

    /// The owner string written into the store lease.
    pub fn owner(&self) -> &str {
        &self.owner
    }

    /// Takes `repo`'s lease until `now_ms + ttl_ms`, or `None` when another
    /// sweep holds it, in this process or in the store.
    pub async fn try_acquire<M: MetaStore>(
        &self,
        meta: &M,
        repo: RepoId,
        now_ms: i64,
        ttl_ms: i64,
    ) -> StoreResult<Option<SweepLease>> {
        if !self.lock().insert(repo) {
            return Ok(None);
        }
        let mut lease = SweepLease {
            leases: self.clone(),
            repo,
            value: Vec::new(),
        };
        let current = meta.derived(repo, LEASE_KIND, 1, b"").await?;
        if let Some(v) = &current
            && let Some((until, owner)) = decode(v)
            && until > now_ms
            && owner != self.owner
        {
            return Ok(None);
        }
        let value = encode(now_ms.saturating_add(ttl_ms), &self.owner);
        if swap(meta, repo, current, value.clone()).await? {
            lease.value = value;
            Ok(Some(lease))
        } else {
            Ok(None)
        }
    }

    /// Whether a sweep of this process holds `repo`'s lease.
    pub fn is_held(&self, repo: RepoId) -> bool {
        self.lock().contains(&repo)
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, HashSet<RepoId>> {
        self.held.lock().unwrap_or_else(|e| e.into_inner())
    }
}

/// The host name, or `unknown-host` when it cannot be read.
fn host_name() -> String {
    std::env::var("HOSTNAME")
        .ok()
        .or_else(|| std::fs::read_to_string("/etc/hostname").ok())
        .map(|h| h.trim().to_owned())
        .filter(|h| !h.is_empty())
        .unwrap_or_else(|| "unknown-host".to_owned())
}

/// A random 64-bit nonce, drawn once per process: the standard library's
/// per-process random hash keys, mixed with the start time.
fn process_nonce() -> u64 {
    use std::hash::{BuildHasher, Hasher};
    static NONCE: std::sync::OnceLock<u64> = std::sync::OnceLock::new();
    *NONCE.get_or_init(|| {
        let mut h = std::collections::hash_map::RandomState::new().build_hasher();
        let t = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| d.as_nanos());
        h.write_u128(t);
        h.write_u32(std::process::id());
        h.finish()
    })
}

/// A held lease. [`SweepLease::release`] gives the store lease back;
/// dropping it releases only the in-process hold, and the store lease then
/// lapses at its time.
#[derive(Debug)]
pub struct SweepLease {
    leases: SweepLeases,
    repo: RepoId,
    value: Vec<u8>,
}

impl SweepLease {
    /// Extends the lease to `now_ms + ttl_ms`. False when it was lost:
    /// another sweeper took it after it lapsed.
    pub async fn renew<M: MetaStore>(
        &mut self,
        meta: &M,
        now_ms: i64,
        ttl_ms: i64,
    ) -> StoreResult<bool> {
        let value = encode(now_ms.saturating_add(ttl_ms), &self.leases.owner);
        let ok = swap(meta, self.repo, Some(self.value.clone()), value.clone()).await?;
        if ok {
            self.value = value;
        }
        Ok(ok)
    }

    /// Gives the lease back: it lapses now, unless another sweeper already
    /// holds it.
    pub async fn release<M: MetaStore>(self, meta: &M) -> StoreResult<()> {
        let value = encode(i64::MIN, &self.leases.owner);
        swap(meta, self.repo, Some(self.value.clone()), value).await?;
        Ok(())
    }
}

impl Drop for SweepLease {
    fn drop(&mut self) {
        self.leases.lock().remove(&self.repo);
    }
}

/// The CAS: the lease value goes from `from` to `to`. False when it moved.
async fn swap<M: MetaStore>(
    meta: &M,
    repo: RepoId,
    from: Option<Vec<u8>>,
    to: Vec<u8>,
) -> StoreResult<bool> {
    let txn = MetaTxn {
        preconditions: vec![Precondition::DerivedIs {
            kind: LEASE_KIND.to_owned(),
            version: 1,
            key: Vec::new(),
            value: from,
        }],
        writes: vec![Write::PutDerived {
            kind: LEASE_KIND.to_owned(),
            version: 1,
            key: Vec::new(),
            value: to,
        }],
    };
    Ok(matches!(
        meta.commit(repo, txn).await?,
        TxnOutcome::Committed(_)
    ))
}

/// `until_ms` (8 bytes, big-endian) then the owner.
fn encode(until_ms: i64, owner: &str) -> Vec<u8> {
    let mut v = until_ms.to_be_bytes().to_vec();
    v.extend_from_slice(owner.as_bytes());
    v
}

fn decode(v: &[u8]) -> Option<(i64, &str)> {
    let (until, owner) = v.split_at_checked(8)?;
    Some((
        i64::from_be_bytes(until.try_into().ok()?),
        std::str::from_utf8(owner).ok()?,
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use graft_store::memory::MemoryStore;

    /// A minimal executor: the memory store's futures are always ready.
    fn block<F: std::future::Future>(f: F) -> F::Output {
        let mut f = std::pin::pin!(f);
        let mut cx = std::task::Context::from_waker(std::task::Waker::noop());
        loop {
            if let std::task::Poll::Ready(v) = f.as_mut().poll(&mut cx) {
                return v;
            }
        }
    }

    #[test]
    fn one_holder_per_repository_across_processes_until_released() {
        block(async {
            let meta = MemoryStore::new();
            let repo = meta.create_repo("r").await.expect("repo");
            let a = SweepLeases::with_owner("a");
            let b = SweepLeases::with_owner("b");
            let held = a.try_acquire(&meta, repo, 0, 100).await.unwrap();
            let held = held.expect("free");
            assert!(a.try_acquire(&meta, repo, 0, 100).await.unwrap().is_none());
            assert!(
                b.try_acquire(&meta, repo, 50, 100).await.unwrap().is_none(),
                "another process sees the store lease"
            );
            held.release(&meta).await.unwrap();
            assert!(!a.is_held(repo));
            let by_b = b.try_acquire(&meta, repo, 60, 100).await.unwrap();
            assert!(by_b.is_some(), "released");
        });
    }

    #[test]
    fn owners_are_distinct_and_carry_a_nonce() {
        let (a, b) = (SweepLeases::new(), SweepLeases::new());
        assert_ne!(a.owner(), b.owner());
        assert!(a.owner().contains(&format!("{:016x}", process_nonce())));
        assert!(!a.owner().starts_with("pid"), "{}", a.owner());
    }

    #[test]
    fn a_live_lease_of_another_owner_is_not_retaken() {
        block(async {
            let meta = MemoryStore::new();
            let repo = meta.create_repo("r").await.expect("repo");
            // Two processes, as two containers that are both pid 1 would be.
            let a = SweepLeases::new();
            let b = SweepLeases::new();
            let _held = a.try_acquire(&meta, repo, 0, 100).await.unwrap().unwrap();
            for now in [0, 1, 50, 99] {
                assert!(
                    b.try_acquire(&meta, repo, now, 100)
                        .await
                        .unwrap()
                        .is_none(),
                    "retaken at {now}"
                );
            }
        });
    }

    #[test]
    fn a_dead_holder_blocks_only_until_the_lease_lapses() {
        block(async {
            let meta = MemoryStore::new();
            let repo = meta.create_repo("r").await.expect("repo");
            let a = SweepLeases::with_owner("a");
            let b = SweepLeases::with_owner("b");
            let mut held = a.try_acquire(&meta, repo, 0, 100).await.unwrap().unwrap();
            assert!(b.try_acquire(&meta, repo, 99, 100).await.unwrap().is_none());
            let taken = b.try_acquire(&meta, repo, 100, 100).await.unwrap();
            assert!(taken.is_some(), "lapsed");
            assert!(
                !held.renew(&meta, 101, 100).await.unwrap(),
                "the old holder has lost it"
            );
        });
    }
}
