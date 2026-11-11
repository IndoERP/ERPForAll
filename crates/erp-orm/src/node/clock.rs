//! The membership clock (design §6.4): monotonic, and it keeps counting while the host is
//! suspended, so a node that wakes from a suspend finds its self-fence deadline passed instead of
//! admitting work on a lease its peers have already counted out.
//!
//! `std::time::Instant` does not count suspend: it reads `CLOCK_MONOTONIC` on Linux and
//! `CLOCK_UPTIME_RAW` on Apple platforms. This clock reads `CLOCK_BOOTTIME` on Linux and
//! `CLOCK_MONOTONIC` on Apple platforms (which, unlike `CLOCK_UPTIME_RAW`, continues while the
//! system sleeps), through `rustix` (no `unsafe` here; on Linux a vDSO call, no system call).

use std::time::Duration;

#[cfg(any(target_os = "linux", target_os = "android"))]
pub(super) const SOURCE: rustix::time::ClockId = rustix::time::ClockId::Boottime;
#[cfg(target_vendor = "apple")]
pub(super) const SOURCE: rustix::time::ClockId = rustix::time::ClockId::Monotonic;
#[cfg(not(any(target_os = "linux", target_os = "android", target_vendor = "apple")))]
compile_error!(
    "the node registry needs a monotonic clock that counts suspend; it is known for Linux and \
     Apple platforms only (crates/erp-orm/src/node/clock.rs)"
);

/// A point on the membership clock: nanoseconds since its origin (the boot).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) struct Tick(u64);

/// Now, on the membership clock.
pub(crate) fn now() -> Tick {
    let t = rustix::time::clock_gettime(SOURCE);
    // both fields are non-negative for these clocks; a negative one would be a kernel defect,
    // read as the origin (the deadline then lies in the past: the node fences, never the
    // reverse)
    let secs = u64::try_from(t.tv_sec).unwrap_or(0);
    let nanos = u64::try_from(t.tv_nsec).unwrap_or(0);
    Tick(secs.saturating_mul(1_000_000_000).saturating_add(nanos))
}

impl Tick {
    /// The tick `nanos` after the origin (an atomic's value).
    pub(crate) fn from_nanos(nanos: u64) -> Self {
        Self(nanos)
    }

    /// Nanoseconds since the origin.
    pub(crate) fn as_nanos(self) -> u64 {
        self.0
    }

    /// `self + d`, saturating.
    pub(crate) fn saturating_add(self, d: Duration) -> Self {
        Self(
            self.0
                .saturating_add(u64::try_from(d.as_nanos()).unwrap_or(u64::MAX)),
        )
    }

    /// `self - earlier`, zero when `earlier` is later.
    pub(crate) fn saturating_since(self, earlier: Self) -> Duration {
        Duration::from_nanos(self.0.saturating_sub(earlier.0))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// run in CI)
    #[test]
    fn the_membership_clock_counts_suspend() {
        #[cfg(any(target_os = "linux", target_os = "android"))]
        let source = rustix::time::ClockId::Boottime;
        #[cfg(target_vendor = "apple")]
        let source = rustix::time::ClockId::Monotonic;
        let ns = |t: rustix::time::Timespec| {
            u64::try_from(t.tv_sec).unwrap() * 1_000_000_000 + u64::try_from(t.tv_nsec).unwrap()
        };
        let before = ns(rustix::time::clock_gettime(source));
        let now = now();
        let after = ns(rustix::time::clock_gettime(source));
        assert!(before <= now.as_nanos() && now.as_nanos() <= after);
        assert_eq!(SOURCE, source);
        let later = now.saturating_add(std::time::Duration::from_secs(3));
        assert_eq!(
            later.saturating_since(now),
            std::time::Duration::from_secs(3)
        );
        assert_eq!(now.saturating_since(later), std::time::Duration::ZERO);
    }
}
