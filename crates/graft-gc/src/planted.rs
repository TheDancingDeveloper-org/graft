//! Test-only planted bugs for the adversarial harness
//! (docs/notes/adversarial-harness.md §3.4).
//!
//! Compiled only with the `planted-bugs` feature, which nothing but
//! `graft-harness`'s dev-dependencies enables. Even then each bug is off
//! until a test plants it on a [`PlantedBugs`] handle shared with the
//! engine and the collector.

use std::sync::Arc;
use std::sync::atomic::{AtomicU8, Ordering};

/// A bug the harness must catch.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PlantedBug {
    /// A land candidate's metadata commits before its object writes are
    /// awaited: the objects are put only after the commit.
    MetadataBeforeObjects,
    /// `Land` skips the `lands[(head, line)]` idempotency check, so a
    /// retry lands again instead of returning the earlier `Landed`.
    SkipIdempotency,
    /// A lease end is reached one tick early: the engine's workspace lapse
    /// compares with `<=` instead of `<`, and the sweep treats an expiry
    /// one ms in the future as passed.
    LeaseBoundary,
    /// The sweep ignores the land-attempt holder: it collects a land
    /// candidate before its lease ends.
    IgnoreLandHolder,
}

impl PlantedBug {
    const fn bit(self) -> u8 {
        1 << self as u8
    }
}

/// Which bugs are planted. Clones share one set; all are off at first.
#[derive(Clone, Debug, Default)]
pub struct PlantedBugs(Arc<AtomicU8>);

impl PlantedBugs {
    /// Switches `bug` on.
    pub fn plant(&self, bug: PlantedBug) {
        self.0.fetch_or(bug.bit(), Ordering::SeqCst);
    }

    /// Switches `bug` off.
    pub fn clear(&self, bug: PlantedBug) {
        self.0.fetch_and(!bug.bit(), Ordering::SeqCst);
    }

    /// Whether `bug` is on.
    pub fn is_planted(&self, bug: PlantedBug) -> bool {
        self.0.load(Ordering::SeqCst) & bug.bit() != 0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn off_until_planted_and_shared_by_clones() {
        let bugs = PlantedBugs::default();
        let other = bugs.clone();
        assert!(!bugs.is_planted(PlantedBug::LeaseBoundary));
        other.plant(PlantedBug::LeaseBoundary);
        assert!(bugs.is_planted(PlantedBug::LeaseBoundary));
        assert!(!bugs.is_planted(PlantedBug::IgnoreLandHolder));
        bugs.clear(PlantedBug::LeaseBoundary);
        assert!(!other.is_planted(PlantedBug::LeaseBoundary));
    }
}
