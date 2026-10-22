//! A controllable clock.

use std::sync::atomic::{AtomicI64, Ordering};

use graft_model::{Clock, Timestamp};

/// A clock that only moves when told to.
#[derive(Debug, Default)]
pub struct FakeClock {
    ms: AtomicI64,
}

impl FakeClock {
    /// A clock at `ms`.
    pub fn at(ms: i64) -> Self {
        Self {
            ms: AtomicI64::new(ms),
        }
    }

    /// Moves the clock forward.
    pub fn advance(&self, ms: i64) {
        self.ms.fetch_add(ms, Ordering::SeqCst);
    }
}

impl Clock for FakeClock {
    fn now(&self) -> Timestamp {
        Timestamp {
            ms: self.ms.load(Ordering::SeqCst),
            offset_min: 0,
        }
    }
}
