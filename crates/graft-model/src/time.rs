//! Time values and the clock port.

/// A point in time: milliseconds since the Unix epoch plus the author's UTC
/// offset in minutes. Client-supplied in changesets, so never server time.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct Timestamp {
    /// Milliseconds since the Unix epoch.
    pub ms: i64,
    /// UTC offset in minutes.
    pub offset_min: i16,
}

/// A source of the current time. The server uses it for leases only; tests
/// use a fake (graft-testkit `clock`).
pub trait Clock: Send + Sync {
    /// The current time, at UTC offset zero.
    fn now(&self) -> Timestamp;
}

/// The system clock.
#[derive(Clone, Copy, Debug, Default)]
pub struct SystemClock;

impl Clock for SystemClock {
    fn now(&self) -> Timestamp {
        let ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis() as i64)
            .unwrap_or(0);
        Timestamp { ms, offset_min: 0 }
    }
}
