// SPDX-License-Identifier: MIT OR Apache-2.0
//! Database authority is read at admission; claimed runs retain an immutable snapshot.
use super::{JobSettings, Setting};
use crate::BoxFuture;
use crate::node::{NodeError, SectionName, SettingsWriter, SharedSettings};
use erp_fields::{Error, Result};
use std::sync::Arc;

/// A native source of committed, database-global job policy.
///
/// Implementations read one consistent revision/value snapshot from the tenant primary, with
/// bounded connection acquisition and statement timeouts. A failed read refuses new admission.
/// The compatibility product describes immutable provider/schema/declaration compatibility, never mutable
/// database values. This interface grants no settings-write or elevated-handler authority.
pub trait JobSettingsSource: Send + Sync + 'static {
    /// Immutable provider/schema/validator contract used for node compatibility admission.
    fn compatibility(&self) -> JobSettingsCompatibility;
    /// Read committed policy. Missing or corrupt schema/declarations/values must return an error.
    fn read(&self) -> BoxFuture<'_, Result<JobSettingsSnapshot>>;
}

/// Validated policy captured for an admission batch or claimed run.
#[derive(Debug, Clone)]
#[non_exhaustive]
pub struct JobSettingsSnapshot {
    revision: u64,
    settings: JobSettings,
}
impl JobSettingsSnapshot {
    /// Construct database policy from validated settings; Node values must remain defaults.
    ///
    /// ```
    /// use erp_orm::jobs::{JobSettings, JobSettingsSnapshot};
    /// let snapshot = JobSettingsSnapshot::new(1, JobSettings::default())?;
    /// assert_eq!(snapshot.revision(), 1);
    /// # Ok::<(), erp_fields::Error>(())
    /// ```
    /// # Errors
    /// `Validation` naming any attempted Node override.
    pub fn new(revision: u64, settings: JobSettings) -> Result<Self> {
        for setting in Setting::ALL.iter().copied().filter(|s| !s.is_shared()) {
            if settings.get(setting) != setting.default_value() {
                return Err(Error::validation(format!(
                    "Database job policy cannot override {}.",
                    setting.name()
                )));
            }
        }
        if !settings.run_in_app() {
            return Err(Error::validation(
                "Database job policy cannot override jobs.run_in_app.",
            ));
        }
        Ok(Self { revision, settings })
    }
    /// Durable Settings scope revision. A revision is not an authorization token.
    pub fn revision(&self) -> u64 {
        self.revision
    }
    /// Effective immutable settings, including the admitting node's local capacity.
    pub fn settings(&self) -> &JobSettings {
        &self.settings
    }
    pub(crate) fn static_policy(settings: JobSettings) -> Arc<Self> {
        Arc::new(Self {
            revision: 0,
            settings,
        })
    }
    pub(crate) fn on_node(mut self, node: &JobSettings) -> Result<Arc<Self>> {
        self.settings = self.settings.with_node_settings(node)?;
        Ok(Arc::new(self))
    }
}

/// Immutable compatibility contract of a database policy provider, independent of stored values.
#[derive(Debug, Clone)]
#[non_exhaustive]
pub struct JobSettingsCompatibility {
    provider: &'static str,
    version: u64,
}
impl JobSettingsCompatibility {
    /// Name the native provider and its schema/group-validation protocol version.
    /// # Errors
    /// `Validation` for an invalid provider identifier or zero protocol version.
    pub fn new(provider: &'static str, version: u64) -> Result<Self> {
        if provider.is_empty()
            || provider.len() > 128
            || !provider
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b"._-".contains(&b))
            || version == 0
        {
            return Err(Error::validation(
                "Invalid job settings provider compatibility identity.",
            ));
        }
        Ok(Self { provider, version })
    }
}
impl SharedSettings for JobSettingsCompatibility {
    fn section(&self) -> SectionName {
        SectionName::new("jobs")
    }
    fn describe(&self, out: &mut SettingsWriter<'_>) -> std::result::Result<(), NodeError> {
        out.shared_str("jobs.provider", self.provider)?;
        out.shared_u64("jobs.provider_version", self.version)?;
        for setting in Setting::ALL.iter().copied() {
            if setting.is_shared() {
                let name = setting.name();
                out.shared_u64(&format!("{name}.default"), setting.default_value())?;
                out.shared_u64(&format!("{name}.floor"), setting.bounds().0)?;
                out.shared_u64(&format!("{name}.hard"), setting.bounds().1)?;
            } else {
                out.node(setting.name())?;
            }
        }
        out.node("jobs.run_in_app")
    }
}
