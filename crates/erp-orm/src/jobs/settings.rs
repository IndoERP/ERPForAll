//! The job queue's settings (design §9): every setting has a default, a floor and
//! a hard bound; a value outside them fails naming the setting, zero never means unlimited, and
//! nothing is clamped.
//!
//! The scope of each setting follows: capacity of this process
//! (`jobs.workers`, `jobs.workers_per_db`, `jobs.claim_batch`, `jobs.run_in_app`) is a node
//! setting; everything that changes how a job behaves is shared, so a process with another value
//! is a configuration mismatch the node registry reports.

use std::time::Duration;

use crate::node::{NodeError, SectionName, SettingsWriter, SharedSettings};

/// Where a setting's value must agree.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Scope {
    Shared,
    Node,
}

struct Spec {
    name: &'static str,
    default: u64,
    floor: u64,
    hard: u64,
    scope: Scope,
}

macro_rules! settings {
    ($($variant:ident => $name:literal, $default:expr, $floor:expr, $hard:expr, $scope:ident;)+) => {
        /// One numeric job setting; use [`JobSettings::get`] or the typed getters.
        #[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
        #[non_exhaustive]
        pub enum Setting {
            $(
                #[doc = concat!("`", $name, "`")]
                $variant,
            )+
        }

        impl Setting {
            /// Every numeric setting, in table order.
            pub const ALL: &'static [Setting] = &[$(Setting::$variant),+];

            fn spec(self) -> &'static Spec {
                match self {
                    $(Setting::$variant => &Spec {
                        name: $name,
                        default: $default,
                        floor: $floor,
                        hard: $hard,
                        scope: Scope::$scope,
                    },)+
                }
            }

            /// The setting's name, such as `jobs.poll_interval_secs`.
            pub fn name(self) -> &'static str {
                self.spec().name
            }

            /// The default value.
            pub fn default_value(self) -> u64 {
                self.spec().default
            }

            /// The `(floor, hard bound)` an operator value must lie in, both inclusive.
            pub fn bounds(self) -> (u64, u64) {
                let spec = self.spec();
                (spec.floor, spec.hard)
            }

            /// The setting named `name`.
            pub fn parse(name: &str) -> Option<Setting> {
                Setting::ALL.iter().copied().find(|s| s.name() == name)
            }
        }
    };
}

settings! {
    Workers => "jobs.workers", 4, 1, 256, Node;
    WorkersPerDb => "jobs.workers_per_db", 2, 1, 256, Node;
    MaxRunningPerDb => "jobs.max_running_per_db", 16, 1, 4_096, Shared;
    ClaimBatch => "jobs.claim_batch", 8, 1, 256, Node;
    LeaseSecs => "jobs.lease_secs", 90, 15, 3_600, Shared;
    ReapIntervalSecs => "jobs.reap_interval_secs", 15, 1, 300, Shared;
    PollIntervalSecs => "jobs.poll_interval_secs", 5, 1, 300, Shared;
    NotifyIntervalMs => "jobs.notify_interval_ms", 1_000, 50, 60_000, Shared;
    WakeSlackMs => "jobs.wake_slack_ms", 1_000, 0, 60_000, Shared;
    MaxChunksPerRun => "jobs.max_chunks_per_run", 10, 1, 10_000, Shared;
    RunBudgetSecs => "jobs.run_budget_secs", 120, 1, 3_600, Shared;
    JobDeadlineSecs => "jobs.job_deadline_secs", 900, 10, 86_400, Shared;
    ChunkDefaultLockMs => "jobs.chunk.default.lock_ms", 5_000, 100, 60_000, Shared;
    ChunkDefaultStatementMs => "jobs.chunk.default.statement_ms", 30_000, 1_000, 3_600_000, Shared;
    ChunkDefaultTransactionMs => "jobs.chunk.default.transaction_ms", 60_000, 2_000, 3_600_000, Shared;
    ChunkMaintenanceLockMs => "jobs.chunk.maintenance.lock_ms", 5_000, 100, 60_000, Shared;
    ChunkMaintenanceStatementMs => "jobs.chunk.maintenance.statement_ms", 300_000, 1_000, 3_600_000, Shared;
    ChunkMaintenanceTransactionMs => "jobs.chunk.maintenance.transaction_ms", 600_000, 2_000, 3_600_000, Shared;
    BackoffBaseSecs => "jobs.backoff_base_secs", 10, 1, 3_600, Shared;
    BackoffCapSecs => "jobs.backoff_cap_secs", 3_600, 1, 86_400, Shared;
    MaxAttempts => "jobs.max_attempts", 10, 1, 100, Shared;
    PayloadMaxBytes => "jobs.payload_max_bytes", 65_536, 1_024, 65_536, Shared;
    ProgressMaxBytes => "jobs.progress_max_bytes", 1_048_576, 4_096, 1_048_576, Shared;
    MaxItemFailuresPerJob => "jobs.max_item_failures_per_job", 10_000, 1, 100_000, Shared;
    HistoryDays => "jobs.history_days", 7, 1, 3_650, Shared;
    DeadRetentionDays => "jobs.dead_retention_days", 30, 1, 3_650, Shared;
}

const RUN_IN_APP: &str = "jobs.run_in_app";

/// The queue's settings, checked.
///
/// ```
/// use erp_orm::jobs::{JobSettings, Setting};
/// let settings = JobSettings::builder().set(Setting::PollIntervalSecs, 30).build()?;
/// assert_eq!(settings.poll_interval().as_secs(), 30);
/// let refused = JobSettings::builder().set(Setting::PollIntervalSecs, 0).build().unwrap_err();
/// assert_eq!(refused.to_string(), "jobs.poll_interval_secs = 0 is outside 1..=300");
/// # Ok::<(), erp_orm::node::NodeError>(())
/// ```
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct JobSettings {
    values: Vec<u64>,
    run_in_app: bool,
    /// Test seam: a lease shorter than the operator floor (feature `test-faults`).
    #[cfg(feature = "test-faults")]
    lease_for_tests: Option<Duration>,
}

impl Default for JobSettings {
    /// Every default, `jobs.run_in_app` on (the single-node profile).
    fn default() -> Self {
        Self {
            values: Setting::ALL.iter().map(|s| s.default_value()).collect(),
            run_in_app: true,
            #[cfg(feature = "test-faults")]
            lease_for_tests: None,
        }
    }
}

/// Builds [`JobSettings`]; nothing is checked until [`build`](Self::build).
#[derive(Debug, Clone)]
#[must_use]
pub struct JobSettingsBuilder {
    settings: JobSettings,
}

impl JobSettings {
    /// Start from the defaults.
    pub fn builder() -> JobSettingsBuilder {
        JobSettingsBuilder {
            settings: JobSettings::default(),
        }
    }

    /// The value of `setting`.
    pub fn get(&self, setting: Setting) -> u64 {
        // `values` always holds one entry per setting (the only constructors fill all of them)
        self.values
            .get(index(setting))
            .copied()
            .unwrap_or_else(|| setting.default_value())
    }

    fn secs(&self, setting: Setting) -> Duration {
        Duration::from_secs(self.get(setting))
    }

    fn millis(&self, setting: Setting) -> Duration {
        Duration::from_millis(self.get(setting))
    }

    fn narrow(&self, setting: Setting) -> u32 {
        // every bound of a setting read through here is far below `u32::MAX`
        u32::try_from(self.get(setting)).unwrap_or(u32::MAX)
    }

    /// `jobs.run_in_app`: whether an `app` node also runs jobs.
    pub fn run_in_app(&self) -> bool {
        self.run_in_app
    }

    /// `jobs.workers`.
    pub fn workers(&self) -> u32 {
        self.narrow(Setting::Workers)
    }

    /// `jobs.workers_per_db`.
    pub fn workers_per_db(&self) -> u32 {
        self.narrow(Setting::WorkersPerDb)
    }

    /// `jobs.max_running_per_db`.
    pub fn max_running_per_db(&self) -> u32 {
        self.narrow(Setting::MaxRunningPerDb)
    }

    /// `jobs.claim_batch`.
    pub fn claim_batch(&self) -> u32 {
        self.narrow(Setting::ClaimBatch)
    }

    /// `jobs.lease_secs`.
    pub fn lease(&self) -> Duration {
        self.secs(Setting::LeaseSecs)
    }

    /// `jobs.reap_interval_secs`.
    pub fn reap_interval(&self) -> Duration {
        self.secs(Setting::ReapIntervalSecs)
    }

    /// `jobs.poll_interval_secs`.
    pub fn poll_interval(&self) -> Duration {
        self.secs(Setting::PollIntervalSecs)
    }

    /// `jobs.notify_interval_ms`.
    pub fn notify_interval(&self) -> Duration {
        self.millis(Setting::NotifyIntervalMs)
    }

    /// `jobs.wake_slack_ms`.
    pub fn wake_slack(&self) -> Duration {
        self.millis(Setting::WakeSlackMs)
    }

    /// `jobs.max_chunks_per_run`.
    pub fn max_chunks_per_run(&self) -> u32 {
        self.narrow(Setting::MaxChunksPerRun)
    }

    /// `jobs.run_budget_secs`.
    pub fn run_budget(&self) -> Duration {
        self.secs(Setting::RunBudgetSecs)
    }

    /// `jobs.job_deadline_secs`.
    pub fn job_deadline(&self) -> Duration {
        self.secs(Setting::JobDeadlineSecs)
    }

    /// `jobs.backoff_base_secs`.
    pub fn backoff_base(&self) -> Duration {
        self.secs(Setting::BackoffBaseSecs)
    }

    /// `jobs.backoff_cap_secs`.
    pub fn backoff_cap(&self) -> Duration {
        self.secs(Setting::BackoffCapSecs)
    }

    /// `jobs.max_attempts`.
    pub fn max_attempts(&self) -> u32 {
        self.narrow(Setting::MaxAttempts)
    }

    /// `jobs.payload_max_bytes`.
    pub fn payload_max_bytes(&self) -> usize {
        usize::try_from(self.get(Setting::PayloadMaxBytes)).unwrap_or(usize::MAX)
    }

    /// `jobs.progress_max_bytes`.
    pub fn progress_max_bytes(&self) -> usize {
        usize::try_from(self.get(Setting::ProgressMaxBytes)).unwrap_or(usize::MAX)
    }

    /// `jobs.max_item_failures_per_job`.
    pub fn max_item_failures_per_job(&self) -> u32 {
        self.narrow(Setting::MaxItemFailuresPerJob)
    }

    /// `jobs.history_days`.
    pub fn history_days(&self) -> u32 {
        self.narrow(Setting::HistoryDays)
    }

    /// `jobs.dead_retention_days`.
    pub fn dead_retention_days(&self) -> u32 {
        self.narrow(Setting::DeadRetentionDays)
    }

    /// The `(lock, statement, transaction)` limits of a chunk of `class`.
    pub fn chunk_timeouts(&self, class: super::TimeoutClass) -> (Duration, Duration, Duration) {
        match class {
            super::TimeoutClass::Maintenance => (
                self.millis(Setting::ChunkMaintenanceLockMs),
                self.millis(Setting::ChunkMaintenanceStatementMs),
                self.millis(Setting::ChunkMaintenanceTransactionMs),
            ),
            super::TimeoutClass::Default => (
                self.millis(Setting::ChunkDefaultLockMs),
                self.millis(Setting::ChunkDefaultStatementMs),
                self.millis(Setting::ChunkDefaultTransactionMs),
            ),
        }
    }

    /// The lease a job of `class` is granted: `max(jobs.lease_secs, transaction timeout + 10 s)`,
    /// so a chunk that holds its row cannot outlive its lease unnoticed (2).
    pub fn lease_for(&self, class: super::TimeoutClass) -> Duration {
        #[cfg(feature = "test-faults")]
        if let Some(lease) = self.lease_for_tests {
            return lease;
        }
        let (_, _, transaction) = self.chunk_timeouts(class);
        self.lease()
            .max(transaction.saturating_add(Duration::from_secs(10)))
    }

    /// The share of `jobs.max_running_per_db` one of `live_workers` job-running processes may
    /// claim: `ceil(max / live)`, at least 1 (4).
    pub fn per_process_share(&self, live_workers: u32) -> u32 {
        let live = live_workers.max(1);
        self.max_running_per_db().div_ceil(live).max(1)
    }
}

fn index(setting: Setting) -> usize {
    setting as usize
}

impl JobSettingsBuilder {
    /// Set `setting` to `value` (checked by [`build`](Self::build)).
    pub fn set(mut self, setting: Setting, value: u64) -> Self {
        if let Some(slot) = self.settings.values.get_mut(index(setting)) {
            *slot = value;
        }
        self
    }

    /// Set a numeric setting by name, for configuration files and environment variables.
    ///
    /// # Errors
    ///
    /// [`NodeError::InvalidSetting`] for a name that is not a job setting.
    pub fn set_named(self, name: &str, value: u64) -> Result<Self, NodeError> {
        match Setting::parse(name) {
            Some(setting) => Ok(self.set(setting, value)),
            None => Err(NodeError::invalid_setting(
                name,
                format!("{name} is not a job setting"),
            )),
        }
    }

    /// Test seam (feature `test-faults`): every job gets a lease of `lease`, below the operator
    /// floor and ignoring the transaction-timeout rule, so tests of expiry and renewal run in
    /// milliseconds. Never in a production build.
    #[cfg(feature = "test-faults")]
    #[doc(hidden)]
    pub fn lease_for_tests(mut self, lease: Duration) -> Self {
        self.settings.lease_for_tests = Some(lease);
        self
    }

    /// Set `jobs.run_in_app`.
    pub fn run_in_app(mut self, on: bool) -> Self {
        self.settings.run_in_app = on;
        self
    }

    /// Check every value against its bounds, then the relations between settings.
    ///
    /// # Errors
    ///
    /// [`NodeError::InvalidSetting`] naming the setting: a value outside `floor..=hard`
    /// (`jobs.poll_interval_secs = 0 is outside 1..=300`), `jobs.workers_per_db` above
    /// `jobs.workers`, a backoff cap below its base, or a chunk statement timeout not below its
    /// transaction timeout.
    pub fn build(self) -> Result<JobSettings, NodeError> {
        let settings = self.settings;
        for setting in Setting::ALL.iter().copied() {
            let (floor, hard) = setting.bounds();
            let value = settings.get(setting);
            if !(floor..=hard).contains(&value) {
                return Err(NodeError::invalid_setting(
                    setting.name(),
                    format!("{} = {value} is outside {floor}..={hard}", setting.name()),
                ));
            }
        }
        if settings.get(Setting::WorkersPerDb) > settings.get(Setting::Workers) {
            return Err(NodeError::invalid_setting(
                Setting::WorkersPerDb.name(),
                format!(
                    "jobs.workers_per_db = {} is outside 1..={} (jobs.workers)",
                    settings.get(Setting::WorkersPerDb),
                    settings.get(Setting::Workers)
                ),
            ));
        }
        if settings.get(Setting::BackoffCapSecs) < settings.get(Setting::BackoffBaseSecs) {
            return Err(NodeError::invalid_setting(
                Setting::BackoffCapSecs.name(),
                format!(
                    "jobs.backoff_cap_secs = {} is outside {}..=86400 (jobs.backoff_base_secs)",
                    settings.get(Setting::BackoffCapSecs),
                    settings.get(Setting::BackoffBaseSecs)
                ),
            ));
        }
        for (lock, statement) in [
            (
                Setting::ChunkDefaultLockMs,
                Setting::ChunkDefaultStatementMs,
            ),
            (
                Setting::ChunkMaintenanceLockMs,
                Setting::ChunkMaintenanceStatementMs,
            ),
        ] {
            if settings.get(lock) >= settings.get(statement) {
                return Err(NodeError::invalid_setting(
                    lock.name(),
                    format!(
                        "{} = {} must be below {} = {}",
                        lock.name(),
                        settings.get(lock),
                        statement.name(),
                        settings.get(statement)
                    ),
                ));
            }
        }
        for (statement, transaction) in [
            (
                Setting::ChunkDefaultStatementMs,
                Setting::ChunkDefaultTransactionMs,
            ),
            (
                Setting::ChunkMaintenanceStatementMs,
                Setting::ChunkMaintenanceTransactionMs,
            ),
        ] {
            if settings.get(statement) >= settings.get(transaction) {
                return Err(NodeError::invalid_setting(
                    statement.name(),
                    format!(
                        "{} = {} must be below {} = {}",
                        statement.name(),
                        settings.get(statement),
                        transaction.name(),
                        settings.get(transaction)
                    ),
                ));
            }
        }
        Ok(settings)
    }
}

impl SharedSettings for JobSettings {
    fn section(&self) -> SectionName {
        SectionName::new("jobs")
    }

    fn describe(&self, out: &mut SettingsWriter<'_>) -> Result<(), NodeError> {
        let Self {
            values: _,
            run_in_app: _,
            #[cfg(feature = "test-faults")]
                lease_for_tests: _,
        } = self;
        for setting in Setting::ALL.iter().copied() {
            match setting.spec().scope {
                Scope::Shared => out.shared_u64(setting.name(), self.get(setting))?,
                Scope::Node => out.node(setting.name())?,
            }
        }
        out.node(RUN_IN_APP)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_default_lies_inside_its_bounds_and_builds() {
        let settings = JobSettings::builder().build().unwrap();
        for setting in Setting::ALL.iter().copied() {
            let (floor, hard) = setting.bounds();
            assert!(
                (floor..=hard).contains(&setting.default_value()),
                "{setting:?}"
            );
            assert_eq!(settings.get(setting), setting.default_value());
        }
        assert!(settings.run_in_app());
    }

    #[test]
    fn a_value_below_the_floor_or_above_the_hard_bound_is_refused_naming_the_setting() {
        for setting in Setting::ALL.iter().copied() {
            let (floor, hard) = setting.bounds();
            let name = setting.name();
            if floor > 0 {
                let error = JobSettings::builder()
                    .set(setting, floor - 1)
                    .build()
                    .unwrap_err();
                assert_eq!(
                    error.to_string(),
                    format!("{name} = {} is outside {floor}..={hard}", floor - 1)
                );
            }
            let error = JobSettings::builder()
                .set(setting, hard + 1)
                .build()
                .unwrap_err();
            assert_eq!(
                error.to_string(),
                format!("{name} = {} is outside {floor}..={hard}", hard + 1)
            );
        }
    }

    #[test]
    fn zero_is_never_unlimited() {
        for setting in Setting::ALL.iter().copied() {
            if setting == Setting::WakeSlackMs {
                // a threshold, not a limit: only exactly-due jobs wake a runner
                assert!(JobSettings::builder().set(setting, 0).build().is_ok());
            } else {
                assert!(
                    JobSettings::builder().set(setting, 0).build().is_err(),
                    "{setting:?}"
                );
            }
        }
        assert_eq!(
            JobSettings::builder()
                .set(Setting::PollIntervalSecs, 0)
                .build()
                .unwrap_err()
                .to_string(),
            "jobs.poll_interval_secs = 0 is outside 1..=300"
        );
    }

    #[test]
    fn relations_between_settings_are_checked() {
        let error = JobSettings::builder()
            .set(Setting::Workers, 2)
            .set(Setting::WorkersPerDb, 3)
            .build()
            .unwrap_err();
        assert_eq!(
            error.to_string(),
            "jobs.workers_per_db = 3 is outside 1..=2 (jobs.workers)"
        );
        let error = JobSettings::builder()
            .set(Setting::BackoffBaseSecs, 100)
            .set(Setting::BackoffCapSecs, 50)
            .build()
            .unwrap_err();
        assert_eq!(
            error.to_string(),
            "jobs.backoff_cap_secs = 50 is outside 100..=86400 (jobs.backoff_base_secs)"
        );
        let error = JobSettings::builder()
            .set(Setting::ChunkDefaultStatementMs, 60_000)
            .build()
            .unwrap_err();
        assert_eq!(
            error.to_string(),
            "jobs.chunk.default.statement_ms = 60000 must be below \
             jobs.chunk.default.transaction_ms = 60000"
        );
    }

    #[test]
    fn a_chunk_lock_timeout_must_be_below_its_statement_timeout() {
        let error = JobSettings::builder()
            .set(Setting::ChunkDefaultStatementMs, 1_000)
            .set(Setting::ChunkDefaultTransactionMs, 5_000)
            .build()
            .unwrap_err();
        assert_eq!(
            error.to_string(),
            "jobs.chunk.default.lock_ms = 5000 must be below \
             jobs.chunk.default.statement_ms = 1000"
        );
    }

    #[test]
    fn an_unknown_name_is_refused() {
        let error = JobSettings::builder()
            .set_named("jobs.nope", 1)
            .unwrap_err();
        assert_eq!(error.to_string(), "jobs.nope is not a job setting");
        let settings = JobSettings::builder()
            .set_named("jobs.history_days", 30)
            .unwrap()
            .build()
            .unwrap();
        assert_eq!(settings.history_days(), 30);
    }

    #[test]
    fn the_per_process_share_divides_the_database_cap_by_live_workers() {
        let settings = JobSettings::builder()
            .set(Setting::MaxRunningPerDb, 4)
            .build()
            .unwrap();
        assert_eq!(settings.per_process_share(1), 4);
        assert_eq!(settings.per_process_share(2), 2);
        assert_eq!(settings.per_process_share(3), 2);
        assert_eq!(settings.per_process_share(0), 4);
        assert_eq!(settings.per_process_share(100), 1);
    }

    #[test]
    fn a_maintenance_lease_covers_its_transaction_timeout_plus_ten_seconds() {
        let settings = JobSettings::default();
        assert_eq!(
            settings.lease_for(super::super::TimeoutClass::Default),
            Duration::from_secs(90)
        );
        assert_eq!(
            settings.lease_for(super::super::TimeoutClass::Maintenance),
            Duration::from_secs(610)
        );
    }

    #[test]
    fn the_description_declares_every_setting_once_with_its_scope() {
        let digest = crate::node::SectionDigest::of(&JobSettings::default()).unwrap();
        // every numeric setting plus jobs.run_in_app
        assert_eq!(digest.settings().len(), Setting::ALL.len() + 1);
    }
}
