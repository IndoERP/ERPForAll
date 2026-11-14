// SPDX-License-Identifier: MIT OR Apache-2.0
//! Native adapter for committed, database-global jobs policy (/D5).
use erp_sdk::jobs::{JobSettingsCompatibility, JobSettingsSnapshot, JobSettingsSource};
use erp_sdk::{BeginOptions, BoxFuture, Error, Isolation, Orm, Result, SUPERUSER_ID};

/// A read-only policy reader on a separately bounded primary pool.
/// Native composition supplies a minimal ORM with no package hooks or write privileges added.
#[derive(Clone)]
#[non_exhaustive]
pub struct DatabaseJobSettingsSource {
    reader: Orm,
    compatibility: JobSettingsCompatibility,
}
impl DatabaseJobSettingsSource {
    /// Bind a minimal reader. The caller must use a separate pool so enqueue cannot exhaust it.
    /// # Errors
    /// Missing Settings models or invalid native compatibility metadata.
    pub fn new(reader: Orm) -> Result<Self> {
        for model in [
            "ir.config.setting",
            "ir.config.setting.scope",
            "ir.config.setting.value",
        ] {
            if reader.registry().model(model).is_none() {
                return Err(Error::programming(format!(
                    "Database job policy requires {model}."
                )));
            }
        }
        Ok(Self {
            reader,
            compatibility: Self::compatibility_identity()?,
        })
    }
    /// Stable provider/schema/group-validator protocol, included in served node admission.
    /// Version changes accompany incompatible storage or group-validation changes.
    /// # Errors
    /// Invalid compiled compatibility metadata.
    pub fn compatibility_identity() -> Result<JobSettingsCompatibility> {
        JobSettingsCompatibility::new("base.declared-settings", 1)
    }
}
impl JobSettingsSource for DatabaseJobSettingsSource {
    fn compatibility(&self) -> JobSettingsCompatibility {
        self.compatibility.clone()
    }
    fn read(&self) -> BoxFuture<'_, Result<JobSettingsSnapshot>> {
        Box::pin(async {
            let mut tx = self
                .reader
                .begin_opts(
                    BeginOptions::new(SUPERUSER_ID)
                        .isolation(Isolation::ReadCommitted)
                        .read_only("jobs.policy"),
                )
                .await?;
            let (revision, values) = crate::ir_config_setting::read_job_policy(&mut tx).await?;
            let settings = crate::jobs_settings::settings(&values)?;
            tx.rollback().await?;
            JobSettingsSnapshot::new(revision, settings)
        })
    }
}
