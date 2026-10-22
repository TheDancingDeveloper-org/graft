//! Bounds on `Watch` streams (`docs/design.md` §7.2, "every response is
//! bounded").
//!
//! A watch holds a task and polls the journal, so each is limited:
//!
//! - its idle wait is capped at `max_watch_wait_ms`, whatever the client
//!   asks for;
//! - it ends at the earlier of its token's expiry and `max_watch_lifetime_s`
//!   after it started; the token is re-checked before every page, and an
//!   expired one ends the stream with `Unauthenticated`;
//! - at most `max_watches_per_principal` run for one principal and
//!   `max_watches` in total; past either, `Watch` answers
//!   `Unavailable { retry_after }`.
//!
//! A client that reaches the lifetime simply resumes from the last
//! `next_lsn` it saw.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use graft_proto::ApiError;

/// How long a refused watch should wait before trying again.
pub(crate) const WATCH_RETRY_MS: u64 = 1_000;

/// The configured bounds.
#[derive(Clone, Copy, Debug)]
pub(crate) struct WatchLimits {
    /// Longest idle wait.
    pub(crate) max_wait: Duration,
    /// Longest stream.
    pub(crate) max_lifetime: Duration,
    /// Concurrent streams per principal.
    pub(crate) per_principal: usize,
    /// Concurrent streams in total.
    pub(crate) total: usize,
}

#[derive(Debug, Default)]
struct Counts {
    total: usize,
    by_principal: HashMap<String, usize>,
}

/// The running watches.
#[derive(Debug)]
pub(crate) struct Watches {
    limits: WatchLimits,
    counts: Arc<Mutex<Counts>>,
}

/// One running watch; dropping it frees its slot.
#[derive(Debug)]
pub(crate) struct WatchSlot {
    principal: String,
    counts: Arc<Mutex<Counts>>,
}

impl Watches {
    pub(crate) fn new(limits: WatchLimits) -> Self {
        Self {
            limits,
            counts: Arc::default(),
        }
    }

    pub(crate) fn limits(&self) -> WatchLimits {
        self.limits
    }

    /// Takes a slot for `principal`, or `Unavailable` when either limit is
    /// reached.
    pub(crate) fn acquire(&self, principal: &str) -> Result<WatchSlot, ApiError> {
        let mut c = self.counts.lock().unwrap_or_else(|e| e.into_inner());
        let mine = c.by_principal.get(principal).copied().unwrap_or(0);
        if c.total >= self.limits.total || mine >= self.limits.per_principal {
            return Err(ApiError::Unavailable {
                retry_after_ms: WATCH_RETRY_MS,
            });
        }
        c.total += 1;
        *c.by_principal.entry(principal.to_owned()).or_default() += 1;
        Ok(WatchSlot {
            principal: principal.to_owned(),
            counts: self.counts.clone(),
        })
    }
}

impl Drop for WatchSlot {
    fn drop(&mut self) {
        let mut c = self.counts.lock().unwrap_or_else(|e| e.into_inner());
        c.total = c.total.saturating_sub(1);
        if let Some(n) = c.by_principal.get_mut(&self.principal) {
            *n -= 1;
            if *n == 0 {
                c.by_principal.remove(&self.principal);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn watches(per_principal: usize, total: usize) -> Watches {
        Watches::new(WatchLimits {
            max_wait: Duration::from_secs(1),
            max_lifetime: Duration::from_secs(1),
            per_principal,
            total,
        })
    }

    #[test]
    fn slots_are_limited_per_principal_and_in_total() {
        let w = watches(2, 3);
        let a1 = w.acquire("a").expect("a1");
        let _a2 = w.acquire("a").expect("a2");
        assert_eq!(
            w.acquire("a").expect_err("a is full"),
            ApiError::Unavailable {
                retry_after_ms: WATCH_RETRY_MS
            }
        );
        let _b1 = w.acquire("b").expect("b1");
        assert!(w.acquire("b").is_err(), "the total is full");
        drop(a1);
        let _b2 = w.acquire("b").expect("a freed slot is reusable");
        assert!(w.acquire("c").is_err());
    }
}
