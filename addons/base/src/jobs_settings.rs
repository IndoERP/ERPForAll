// SPDX-License-Identifier: MIT OR Apache-2.0
//! Declared database-global jobs policy. Native composition reads committed policy at job admission.
use erp_sdk::jobs::{JobSettings, Setting};
use erp_sdk::settings::{ScopeKind, SettingDecl, SettingGroupDecl, SettingsRegistry};
use erp_sdk::{Error, Result, Value};
use std::collections::BTreeMap;

const GROUP: &str = "base.jobs";
const ADMIN: &str = "base.group_system";

pub(crate) fn shared() -> impl Iterator<Item = Setting> {
    Setting::ALL
        .iter()
        .copied()
        .filter(|setting| setting.is_shared())
}
pub(crate) fn key(setting: Setting) -> String {
    format!("base.{}", setting.name())
}
fn presentation(setting: Setting) -> Result<(&'static str, &'static str)> {
    match setting {
        Setting::MaxRunningPerDb => Ok(("Maximum running jobs", "jobs")),
        Setting::LeaseSecs => Ok(("Lease", "seconds")),
        Setting::ReapIntervalSecs => Ok(("Reap interval", "seconds")),
        Setting::PollIntervalSecs => Ok(("Poll interval", "seconds")),
        Setting::NotifyIntervalMs => Ok(("Notification interval", "milliseconds")),
        Setting::WakeSlackMs => Ok(("Wake threshold", "milliseconds")),
        Setting::MaxChunksPerRun => Ok(("Maximum chunks per run", "chunks")),
        Setting::RunBudgetSecs => Ok(("Run budget", "seconds")),
        Setting::JobDeadlineSecs => Ok(("Job deadline", "seconds")),
        Setting::ChunkDefaultLockMs => Ok(("Default chunk lock timeout", "milliseconds")),
        Setting::ChunkDefaultStatementMs => Ok(("Default chunk statement timeout", "milliseconds")),
        Setting::ChunkDefaultTransactionMs => {
            Ok(("Default chunk transaction timeout", "milliseconds"))
        }
        Setting::ChunkMaintenanceLockMs => Ok(("Maintenance chunk lock timeout", "milliseconds")),
        Setting::ChunkMaintenanceStatementMs => {
            Ok(("Maintenance chunk statement timeout", "milliseconds"))
        }
        Setting::ChunkMaintenanceTransactionMs => {
            Ok(("Maintenance chunk transaction timeout", "milliseconds"))
        }
        Setting::BackoffBaseSecs => Ok(("Retry backoff base", "seconds")),
        Setting::BackoffCapSecs => Ok(("Retry backoff cap", "seconds")),
        Setting::MaxAttempts => Ok(("Maximum attempts", "attempts")),
        Setting::PayloadMaxBytes => Ok(("Maximum payload", "bytes")),
        Setting::ProgressMaxBytes => Ok(("Maximum progress", "bytes")),
        Setting::MaxItemFailuresPerJob => Ok(("Maximum item failures", "failures")),
        Setting::HistoryDays => Ok(("Job history retention", "days")),
        Setting::DeadRetentionDays => Ok(("Dead job retention", "days")),
        _ => Err(Error::programming(format!(
            "Missing shared jobs setting presentation for {}.",
            setting.name()
        ))),
    }
}
fn declarations() -> Result<Vec<SettingDecl>> {
    shared()
        .map(|setting| {
            let (label, unit) = presentation(setting)?;
            let (floor, hard) = setting.bounds();
            let convert = |value| {
                i64::try_from(value).map_err(|_| {
                    Error::programming(format!(
                        "Job setting {} does not fit the Settings integer type.",
                        setting.name()
                    ))
                })
            };
            let help = format!(
                "Default: {} {unit}. Allowed: {floor} to {hard} {unit}.",
                setting.default_value()
            );
            SettingDecl::integer(
                &key(setting),
                &format!("{label} ({unit})"),
                ScopeKind::Global,
                convert(setting.default_value())?,
                (convert(floor)?, convert(hard)?),
            )?
            .block("Background Jobs")?
            .help(&help)?
            .access(&[ADMIN], &[ADMIN])
        })
        .collect()
}
pub(crate) fn settings(values: &BTreeMap<String, Value>) -> Result<JobSettings> {
    let mut builder = JobSettings::builder();
    for setting in shared() {
        let external = key(setting);
        let Some(Value::Int(value)) = values.get(&external) else {
            return Err(Error::validation(format!("{external} must be an integer.")));
        };
        let value = u64::try_from(*value)
            .map_err(|_| Error::validation(format!("{external} must be nonnegative.")))?;
        builder = builder.set(setting, value);
    }
    builder.build().map_err(|error| {
        Error::validation(error.to_string().replace("jobs.", "base.jobs.")).with_source(error)
    })
}
fn validate(values: &BTreeMap<String, Value>) -> Result<()> {
    settings(values).map(|_| ())
}
fn groups() -> Result<Vec<SettingGroupDecl>> {
    let keys: Vec<_> = shared().map(key).collect();
    let refs: Vec<_> = keys.iter().map(String::as_str).collect();
    Ok(vec![SettingGroupDecl::new(GROUP, &refs, validate)?])
}
/// Required-registration guard: a missing link-time contribution is an error, never a partial page.
pub(crate) fn require(registry: &SettingsRegistry) -> Result<()> {
    for setting in shared() {
        let key = key(setting);
        if registry.get(&key).is_none() {
            return Err(Error::programming(format!(
                "The settings declaration {key} of erp-addon-base is not linked into this binary."
            )));
        }
    }
    if registry.group(GROUP).is_none() {
        return Err(Error::programming(
            "The settings group base.jobs of erp-addon-base is not linked into this binary.",
        ));
    }
    Ok(())
}
erp_sdk::setting_declarations!(declarations);
erp_sdk::setting_groups!(groups);
