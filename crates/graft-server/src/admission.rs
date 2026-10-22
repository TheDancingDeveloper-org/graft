//! The free-space admission floor (`docs/design.md` §14, "Disk full").
//!
//! Below the floor on either the object volume or the metadata volume, every
//! write is refused with `Unavailable` before it touches a store; reads
//! continue. The check is one `statvfs` per volume, at most once a second.

use std::path::PathBuf;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use graft_proto::ApiError;
use graft_telemetry::Metrics;

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

/// The floor and the volumes it watches, each with its metric label.
#[derive(Debug)]
pub(crate) struct Admission {
    volumes: Vec<(&'static str, PathBuf)>,
    min_fraction: f64,
    min_bytes: u64,
    last: Mutex<Option<(Instant, bool)>>,
}

impl Admission {
    /// A floor over the volumes holding each of `volumes`, labelled by name.
    pub(crate) fn new(
        volumes: Vec<(&'static str, PathBuf)>,
        min_fraction: f64,
        min_bytes: u64,
    ) -> Self {
        Self {
            volumes,
            min_fraction,
            min_bytes,
            last: Mutex::new(None),
        }
    }

    /// `Ok` when writes may proceed. Reads the volumes at most once a
    /// second; see [`Admission::sample`].
    pub(crate) fn check_write(&self, metrics: &Metrics) -> Result<(), ApiError> {
        let fresh = match *self.last.lock().unwrap_or_else(|e| e.into_inner()) {
            Some((at, ok)) if at.elapsed() < RECHECK => Some(ok),
            _ => None,
        };
        let ok = match fresh {
            Some(ok) => ok,
            None => self.sample(metrics),
        };
        if ok {
            Ok(())
        } else {
            Err(ApiError::Unavailable {
                retry_after_ms: FULL_RETRY_MS,
            })
        }
    }

    /// Reads every volume's space now, records it in the disk gauges, and
    /// returns whether every volume is above the floor. This is the one
    /// place the server reads free space: writes use it through
    /// [`Admission::check_write`], and `/metrics` calls it on each scrape so
    /// the gauges are current even when no writes arrive.
    pub(crate) fn sample(&self, metrics: &Metrics) -> bool {
        // Every volume is read and recorded, even after one is found below
        // the floor: the other volume's gauges matter most just then.
        let ok = self
            .volumes
            .iter()
            .map(|(volume, dir)| match space(dir) {
                Some(s) => {
                    metrics
                        .disk_free_bytes
                        .with_label_values(&[volume])
                        .set(saturating_i64(s.free));
                    metrics
                        .disk_total_bytes
                        .with_label_values(&[volume])
                        .set(saturating_i64(s.total));
                    above_floor(s, self.min_fraction, self.min_bytes)
                }
                // An unreadable volume is not evidence that it is full.
                None => true,
            })
            .fold(true, |all, ok| all & ok);
        *self.last.lock().unwrap_or_else(|e| e.into_inner()) = Some((Instant::now(), ok));
        ok
    }
}

/// A byte count as a gauge value.
fn saturating_i64(n: u64) -> i64 {
    i64::try_from(n).unwrap_or(i64::MAX)
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
        let (_registry, metrics) = graft_telemetry::registry().unwrap();
        let volumes = || vec![("test", std::env::temp_dir())];
        let a = Admission::new(volumes(), 1.0, 0);
        assert_eq!(
            a.check_write(&metrics),
            Err(ApiError::Unavailable {
                retry_after_ms: FULL_RETRY_MS
            })
        );
        let a = Admission::new(volumes(), 0.0, 0);
        assert_eq!(a.check_write(&metrics), Ok(()));
    }

    #[cfg(unix)]
    #[test]
    fn sampling_records_free_and_total_bytes_per_volume() {
        let (registry, metrics) = graft_telemetry::registry().unwrap();
        let a = Admission::new(vec![("objects", std::env::temp_dir())], 0.0, 0);
        assert!(a.sample(&metrics));
        let text = graft_telemetry::encode(&registry);
        assert!(
            text.contains("graft_disk_free_bytes{volume=\"objects\"} "),
            "{text}"
        );
        let total = metrics
            .disk_total_bytes
            .with_label_values(&["objects"])
            .get();
        assert!(total > 0, "{text}");
    }

    #[cfg(unix)]
    #[test]
    fn a_volume_below_the_floor_does_not_stop_the_others_being_recorded() {
        let (_registry, metrics) = graft_telemetry::registry().unwrap();
        // Both volumes are below a floor of 100%, so the first one refuses.
        let volumes = vec![
            ("objects", std::env::temp_dir()),
            ("meta", std::env::temp_dir()),
        ];
        let a = Admission::new(volumes, 1.0, 0);
        assert!(!a.sample(&metrics));
        for volume in ["objects", "meta"] {
            let total = metrics.disk_total_bytes.with_label_values(&[volume]).get();
            assert!(total > 0, "{volume} not recorded");
        }
    }
}
