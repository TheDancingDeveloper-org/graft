//! The free-space admission floor (`docs/design.md` §14, "Disk full").
//!
//! Below the floor on either the object volume or the metadata volume, every
//! write is refused with `Unavailable` before it touches a store; reads
//! continue. The check is one `statvfs` per volume, at most once a second.

use std::path::PathBuf;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use graft_proto::ApiError;

/// How long one free-space reading is trusted.
const RECHECK: Duration = Duration::from_secs(1);

/// The retry hint for a refused write: space comes back on an operator's
/// time scale, not a client's.
pub(crate) const FULL_RETRY_MS: u64 = 60_000;

/// Free and total bytes of a volume.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Space {
    /// Bytes available to the server.
    pub(crate) free: u64,
    /// Volume size.
    pub(crate) total: u64,
}

/// The floor and the volumes it watches.
#[derive(Debug)]
pub(crate) struct Admission {
    dirs: Vec<PathBuf>,
    min_fraction: f64,
    min_bytes: u64,
    last: Mutex<Option<(Instant, bool)>>,
}

impl Admission {
    /// A floor over the volumes holding `dirs`.
    pub(crate) fn new(dirs: Vec<PathBuf>, min_fraction: f64, min_bytes: u64) -> Self {
        Self {
            dirs,
            min_fraction,
            min_bytes,
            last: Mutex::new(None),
        }
    }

    /// `Ok` when writes may proceed.
    pub(crate) fn check_write(&self) -> Result<(), ApiError> {
        let mut last = self.last.lock().unwrap_or_else(|e| e.into_inner());
        let ok = match *last {
            Some((at, ok)) if at.elapsed() < RECHECK => ok,
            _ => {
                let ok = self.dirs.iter().all(|d| match space(d) {
                    Some(s) => above_floor(s, self.min_fraction, self.min_bytes),
                    // An unreadable volume is not evidence that it is full.
                    None => true,
                });
                *last = Some((Instant::now(), ok));
                ok
            }
        };
        if ok {
            Ok(())
        } else {
            Err(ApiError::Unavailable {
                retry_after_ms: FULL_RETRY_MS,
            })
        }
    }
}

/// Whether `s` is above the floor.
pub(crate) fn above_floor(s: Space, min_fraction: f64, min_bytes: u64) -> bool {
    if s.free < min_bytes {
        return false;
    }
    if s.total == 0 {
        return true;
    }
    // A floor of 1.0 refuses every write, even on an empty volume.
    min_fraction < 1.0 && s.free as f64 / s.total as f64 >= min_fraction
}

/// The space on the volume holding `dir`.
#[cfg(unix)]
pub(crate) fn space(dir: &std::path::Path) -> Option<Space> {
    let v = rustix::fs::statvfs(dir).ok()?;
    Some(Space {
        free: v.f_bavail.saturating_mul(v.f_frsize),
        total: v.f_blocks.saturating_mul(v.f_frsize),
    })
}

/// Unknown off Unix, so admission never refuses.
#[cfg(not(unix))]
pub(crate) fn space(_dir: &std::path::Path) -> Option<Space> {
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn floor_by_fraction_and_bytes() {
        let s = Space {
            free: 10,
            total: 100,
        };
        assert!(above_floor(s, 0.05, 0));
        assert!(above_floor(s, 0.10, 10));
        assert!(!above_floor(s, 0.11, 0));
        assert!(!above_floor(s, 0.0, 11));
        assert!(!above_floor(
            Space {
                free: 100,
                total: 100
            },
            1.0,
            0
        ));
    }

    #[cfg(unix)]
    #[test]
    fn a_full_floor_refuses_with_unavailable() {
        let a = Admission::new(vec![std::env::temp_dir()], 1.0, 0);
        assert_eq!(
            a.check_write(),
            Err(ApiError::Unavailable {
                retry_after_ms: FULL_RETRY_MS
            })
        );
        let a = Admission::new(vec![std::env::temp_dir()], 0.0, 0);
        assert_eq!(a.check_write(), Ok(()));
    }
}
