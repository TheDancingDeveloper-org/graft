//! The service's runtime limits (D41 §4): exponential back-off, the
//! concurrent-fetch and per-host caps, and the per-host, per-credential
//! request budget that stretches poll intervals as it fills.

use std::collections::{HashMap, VecDeque};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use tokio::sync::Semaphore;

/// Exponential back-off: `base`, doubling per failure, capped at `cap`.
#[derive(Clone, Debug)]
pub struct Backoff {
    base: Duration,
    cap: Duration,
    failures: u32,
}

impl Backoff {
    /// A back-off with nothing failed yet.
    pub fn new(base: Duration, cap: Duration) -> Backoff {
        Backoff {
            base,
            cap,
            failures: 0,
        }
    }

    /// Counts a failure and returns the wait before the next try.
    pub fn fail(&mut self) -> Duration {
        self.failures = self.failures.saturating_add(1);
        self.current()
    }

    /// The wait for the failures counted so far (`base` with none).
    pub fn current(&self) -> Duration {
        let shift = self.failures.saturating_sub(1).min(20);
        self.base.saturating_mul(1 << shift).min(self.cap)
    }

    /// Failures in a row.
    pub fn failures(&self) -> u32 {
        self.failures
    }

    /// A success: start again from `base`.
    pub fn reset(&mut self) {
        self.failures = 0;
    }
}

/// The share of a budget at which intervals start to stretch.
pub const STRETCH_AT: f64 = 0.8;

/// The window a budget is measured over.
pub const BUDGET_WINDOW_MS: i64 = 60 * 60 * 1000;

/// Requests made per `(host, credential)` over the last hour, and the
/// factor every interval on that key is stretched by once the hour's use
/// passes [`STRETCH_AT`] of the budget. The credential is a fingerprint
/// kept in memory only.
#[derive(Debug)]
pub struct RateBudget {
    per_hour: u32,
    used: HashMap<(String, u64), VecDeque<(i64, u32)>>,
}

impl RateBudget {
    /// A budget of `per_hour` requests per key.
    pub fn new(per_hour: u32) -> RateBudget {
        RateBudget {
            per_hour: per_hour.max(1),
            used: HashMap::new(),
        }
    }

    fn trim(q: &mut VecDeque<(i64, u32)>, now_ms: i64) {
        while q
            .front()
            .is_some_and(|(at, _)| *at <= now_ms - BUDGET_WINDOW_MS)
        {
            q.pop_front();
        }
    }

    /// Records `requests` made on `key` at `now_ms`.
    pub fn record(&mut self, key: (String, u64), now_ms: i64, requests: u32) {
        let q = self.used.entry(key).or_default();
        Self::trim(q, now_ms);
        q.push_back((now_ms, requests));
    }

    /// Requests on `key` in the last hour.
    pub fn used(&mut self, key: &(String, u64), now_ms: i64) -> u64 {
        let Some(q) = self.used.get_mut(key) else {
            return 0;
        };
        Self::trim(q, now_ms);
        let n = q.iter().map(|(_, r)| u64::from(*r)).sum();
        if q.is_empty() {
            self.used.remove(key);
        }
        n
    }

    /// How much to stretch an interval on `key`: 1 until the hour's use
    /// reaches [`STRETCH_AT`] of the budget, then in proportion to it, so
    /// every task on the key slows evenly.
    pub fn stretch(&mut self, key: &(String, u64), now_ms: i64) -> f64 {
        let used = self.used(key, now_ms) as f64;
        let soft = f64::from(self.per_hour) * STRETCH_AT;
        (used / soft).max(1.0)
    }
}

/// The concurrent-fetch cap and the per-host caps.
#[derive(Debug)]
pub struct FetchSlots {
    all: Arc<Semaphore>,
    per_host: usize,
    hosts: Mutex<HashMap<String, Arc<Semaphore>>>,
}

/// Permits held for one poll.
#[derive(Debug)]
pub struct FetchPermit {
    _all: tokio::sync::OwnedSemaphorePermit,
    _host: tokio::sync::OwnedSemaphorePermit,
}

impl FetchSlots {
    /// At most `all` polls fetching at once, `per_host` of them per host.
    pub fn new(all: usize, per_host: usize) -> FetchSlots {
        FetchSlots {
            all: Arc::new(Semaphore::new(all.max(1))),
            per_host: per_host.max(1),
            hosts: Mutex::new(HashMap::new()),
        }
    }

    /// Waits for a slot on `host` and an overall one.
    pub async fn acquire(&self, host: &str) -> FetchPermit {
        let host_sem = self
            .hosts
            .lock()
            .expect("hosts")
            .entry(host.to_owned())
            .or_insert_with(|| Arc::new(Semaphore::new(self.per_host)))
            .clone();
        let host = host_sem.acquire_owned().await.expect("never closed");
        let all = self
            .all
            .clone()
            .acquire_owned()
            .await
            .expect("never closed");
        FetchPermit {
            _all: all,
            _host: host,
        }
    }

    /// Fetches running now.
    pub fn busy(&self, cap: usize) -> usize {
        cap - self.all.available_permits()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn backoff_doubles_to_its_cap_and_resets() {
        let mut b = Backoff::new(Duration::from_secs(1), Duration::from_secs(60));
        let waits: Vec<u64> = (0..8).map(|_| b.fail().as_secs()).collect();
        assert_eq!(waits, [1, 2, 4, 8, 16, 32, 60, 60]);
        b.reset();
        assert_eq!(b.fail(), Duration::from_secs(1));
    }

    #[test]
    fn budget_stretches_past_eighty_percent_and_recovers() {
        let key = ("github.com".to_owned(), 7);
        let mut budget = RateBudget::new(100);
        budget.record(key.clone(), 0, 79);
        assert_eq!(budget.stretch(&key, 1), 1.0);
        budget.record(key.clone(), 2, 81);
        // 160 used of a soft 80: twice the interval.
        assert_eq!(budget.stretch(&key, 3), 2.0);
        // Another key is not slowed.
        assert_eq!(budget.stretch(&("github.com".to_owned(), 8), 3), 1.0);
        // An hour on, the window is empty again.
        assert_eq!(budget.stretch(&key, BUDGET_WINDOW_MS + 3), 1.0);
    }

    #[tokio::test]
    async fn fetch_slots_cap_per_host_and_overall() {
        let slots = FetchSlots::new(2, 1);
        let a = slots.acquire("a").await;
        // `a` is full: a second poll on it waits; `b` gets the last slot.
        let waiting = tokio::time::timeout(Duration::from_millis(1), slots.acquire("a")).await;
        assert!(waiting.is_err());
        let b = slots.acquire("b").await;
        assert_eq!(slots.busy(2), 2);
        drop((a, b));
        assert_eq!(slots.busy(2), 0);
    }
}
