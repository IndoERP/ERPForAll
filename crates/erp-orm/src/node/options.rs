//! Heartbeat interval, lease and retention (design §6.1): shared settings, refused outside their
//! bounds, never clamped.

use std::time::Duration;

use super::NodeError;
use super::digest::{SectionName, SettingsWriter, SharedSettings};

const INTERVAL_SECS: (u64, u64, u64) = (10, 5, 60);
const LEASE_INTERVALS: (u32, u32, u32) = (3, 3, 10);
const RETENTION_HOURS: (u64, u64, u64) = (24, 1, 720);
/// Interval floor under the `test-faults` feature (debug builds only), so tests run in seconds.
#[cfg(feature = "test-faults")]
const TEST_INTERVAL_FLOOR: Duration = Duration::from_millis(100);
/// Retention floor under the `test-faults` feature.
#[cfg(feature = "test-faults")]
const TEST_RETENTION_FLOOR: Duration = Duration::from_secs(1);

/// The node registry's settings (section `node`, all shared).
///
/// ```
/// use erp_orm::node::NodeOptions;
/// let options = NodeOptions::builder().heartbeat_interval_secs(10).lease_intervals(3).build()?;
/// assert_eq!(options.lease().as_secs(), 30);
/// let err = NodeOptions::builder().heartbeat_interval_secs(4).build().unwrap_err();
/// assert_eq!(err.to_string(), "node.heartbeat_interval_secs = 4 is outside its bounds 5 – 60");
/// # Ok::<(), erp_orm::node::NodeError>(())
/// ```
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct NodeOptions {
    interval: Duration,
    lease_intervals: u32,
    retention: Duration,
}

impl Default for NodeOptions {
    fn default() -> Self {
        Self {
            interval: Duration::from_secs(INTERVAL_SECS.0),
            lease_intervals: LEASE_INTERVALS.0,
            retention: Duration::from_secs(RETENTION_HOURS.0 * 3600),
        }
    }
}

impl NodeOptions {
    /// Start from the defaults (10 s, 3 intervals, 24 h).
    pub fn builder() -> NodeOptionsBuilder {
        NodeOptionsBuilder {
            interval: Err(Duration::from_secs(INTERVAL_SECS.0)),
            lease_intervals: LEASE_INTERVALS.0,
            retention: Err(Duration::from_secs(RETENTION_HOURS.0 * 3600)),
            problem: None,
        }
    }

    /// `node.heartbeat_interval_secs`: one renewal per interval.
    pub fn interval(&self) -> Duration {
        self.interval
    }

    /// `node.lease_intervals`.
    pub fn lease_intervals(&self) -> u32 {
        self.lease_intervals
    }

    /// The lease: interval × lease intervals.
    pub fn lease(&self) -> Duration {
        self.interval.saturating_mul(self.lease_intervals)
    }

    /// `node.retention_hours`: how long a dead row is kept.
    pub fn retention(&self) -> Duration {
        self.retention
    }

    /// How long after a renewal sent at `s` the node may admit work: `lease - interval`.
    pub fn fence_after(&self) -> Duration {
        self.lease().saturating_sub(self.interval)
    }

    pub(crate) fn interval_ms(&self) -> i64 {
        i64::try_from(self.interval.as_millis()).unwrap_or(i64::MAX)
    }

    pub(crate) fn lease_ms(&self) -> i64 {
        i64::try_from(self.lease().as_millis()).unwrap_or(i64::MAX)
    }

    pub(crate) fn retention_ms(&self) -> i64 {
        i64::try_from(self.retention.as_millis()).unwrap_or(i64::MAX)
    }
}

impl SharedSettings for NodeOptions {
    fn section(&self) -> SectionName {
        SectionName::new("node")
    }

    fn describe(&self, out: &mut SettingsWriter<'_>) -> Result<(), NodeError> {
        let Self {
            interval,
            lease_intervals,
            retention,
        } = self;
        out.shared_duration("node.heartbeat_interval_secs", *interval)?;
        out.shared_u64("node.lease_intervals", u64::from(*lease_intervals))?;
        out.shared_duration("node.retention_hours", *retention)
    }
}

/// Builds [`NodeOptions`]; values are checked by [`build`](Self::build).
#[derive(Debug)]
pub struct NodeOptionsBuilder {
    /// `Err(d)`: the default or a duration set by a test seam (already checked); `Ok(secs)`: an
    /// operator value in seconds, checked at build.
    interval: Result<u64, Duration>,
    lease_intervals: u32,
    retention: Result<u64, Duration>,
    problem: Option<NodeError>,
}

impl NodeOptionsBuilder {
    /// `node.heartbeat_interval_secs` (default 10, 5 – 60).
    #[must_use]
    pub fn heartbeat_interval_secs(mut self, secs: u64) -> Self {
        self.interval = Ok(secs);
        self
    }

    /// `node.lease_intervals` (default 3, 3 – 10).
    #[must_use]
    pub fn lease_intervals(mut self, n: u32) -> Self {
        self.lease_intervals = n;
        self
    }

    /// `node.retention_hours` (default 24, 1 – 720).
    #[must_use]
    pub fn retention_hours(mut self, hours: u64) -> Self {
        self.retention = Ok(hours);
        self
    }

    /// Test seam: an interval below the operator floor (at least 100 ms, at most 60 s).
    #[cfg(feature = "test-faults")]
    #[doc(hidden)]
    #[must_use]
    pub fn heartbeat_interval_for_tests(mut self, interval: Duration) -> Self {
        if interval < TEST_INTERVAL_FLOOR || interval > Duration::from_secs(INTERVAL_SECS.2) {
            self.problem = Some(NodeError::invalid_setting(
                "node.heartbeat_interval_secs",
                format!("test interval {interval:?} is outside 100 ms – 60 s"),
            ));
        }
        self.interval = Err(interval);
        self
    }

    /// Test seam: a retention below the operator floor (at least 1 s).
    #[cfg(feature = "test-faults")]
    #[doc(hidden)]
    #[must_use]
    pub fn retention_for_tests(mut self, retention: Duration) -> Self {
        if retention < TEST_RETENTION_FLOOR {
            self.problem = Some(NodeError::invalid_setting(
                "node.retention_hours",
                format!("test retention {retention:?} is below 1 s"),
            ));
        }
        self.retention = Err(retention);
        self
    }

    /// Check and build.
    ///
    /// # Errors
    ///
    /// [`NodeError::InvalidSetting`] for a value outside its bounds:
    /// `<name> = <value> is outside its bounds <floor> – <max>`.
    pub fn build(self) -> Result<NodeOptions, NodeError> {
        if let Some(problem) = self.problem {
            return Err(problem);
        }
        let interval = match self.interval {
            Ok(secs) => {
                check("node.heartbeat_interval_secs", secs, INTERVAL_SECS)?;
                Duration::from_secs(secs)
            }
            Err(d) => d,
        };
        let (_, lo, hi) = LEASE_INTERVALS;
        if !(lo..=hi).contains(&self.lease_intervals) {
            return Err(NodeError::invalid_setting(
                "node.lease_intervals",
                format!(
                    "node.lease_intervals = {} is outside its bounds {lo} – {hi}",
                    self.lease_intervals
                ),
            ));
        }
        let retention = match self.retention {
            Ok(hours) => {
                check("node.retention_hours", hours, RETENTION_HOURS)?;
                Duration::from_secs(hours * 3600)
            }
            Err(d) => d,
        };
        Ok(NodeOptions {
            interval,
            lease_intervals: self.lease_intervals,
            retention,
        })
    }
}

fn check(name: &str, value: u64, (_, lo, hi): (u64, u64, u64)) -> Result<(), NodeError> {
    if (lo..=hi).contains(&value) {
        Ok(())
    } else {
        Err(NodeError::invalid_setting(
            name,
            format!("{name} = {value} is outside its bounds {lo} – {hi}"),
        ))
    }
}
