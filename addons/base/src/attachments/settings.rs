// SPDX-License-Identifier: MIT OR Apache-2.0
//! Bounded database policy. Missing parameters use declared defaults; malformed values refuse.
use erp_sdk::{Error, Result, Transaction};

// Technical policy lives in System Parameters; storage credentials and node capacity stay in TOML.
pub(crate) const PARAMETERS: [(&str, i64, i64, i64); 10] = [
    ("attachments.usage_delta_read_limit", 10_000, 1, 10_000),
    ("attachments.usage_compaction_batch", 1_000, 1, 10_000),
    ("attachments.usage_compaction_interval_secs", 30, 1, 3_600),
    ("attachments.upload_idle_timeout_secs", 30, 1, 300),
    ("attachments.upload_receive_timeout_secs", 600, 10, 86_400),
    ("attachments.gc_interval_secs", 900, 1, 86_400),
    ("attachments.cleanup_interval_secs", 900, 1, 86_400),
    ("attachments.reconcile_interval_secs", 21_600, 1, 604_800),
    ("attachments.maintenance_batch", 1000, 1, 1000),
    // The floor stays above the longest expected compose time: the cleaner deletes a draft's
    // placeholder attachments older than this (review S5).
    (
        "attachments.composer_retention_secs",
        86_400,
        3_600,
        31_536_000,
    ),
];
pub(crate) fn contains(key: &str) -> bool {
    PARAMETERS.iter().any(|p| p.0 == key)
}
pub(crate) fn validate(key: &str, value: Option<&str>) -> Result<()> {
    if let Some(p) = PARAMETERS.iter().find(|p| p.0 == key) {
        parse(p, value)?;
    }
    Ok(())
}
fn parse(p: &(&str, i64, i64, i64), value: Option<&str>) -> Result<i64> {
    let invalid = || {
        Error::validation(format!(
            "{} must be an integer from {} to {}.",
            p.0, p.2, p.3
        ))
    };
    // Absent/empty is the system parameter API's documented reset-to-default behavior.
    let n = match value {
        None | Some("") => p.1,
        Some(v) => v.parse::<i64>().map_err(|_| invalid())?,
    };
    if !(p.2..=p.3).contains(&n) {
        return Err(invalid());
    }
    Ok(n)
}
#[derive(Clone, Copy)]
pub(super) struct Policy {
    pub delta_read: i64,
    pub compact_batch: i64,
    pub compact_interval: u64,
    pub idle: Option<u64>,
    pub receive: Option<u64>,
    pub gc_interval: u64,
    pub cleanup_interval: u64,
    pub reconcile_interval: u64,
    pub batch: i64,
    pub composer_retention: u64,
}
impl Policy {
    pub async fn load(tx: &mut Transaction) -> Result<Self> {
        let keys: Vec<&str> = PARAMETERS.iter().map(|p| p.0).collect();
        let rows = tx
            .query(
                "SELECT key,value FROM ir_config_parameter WHERE key=ANY($1::text[])",
                &[&keys],
            )
            .await?;
        let mut values = PARAMETERS.map(|p| p.1);
        let mut present = [false; 10];
        for row in rows {
            let key: String = row
                .try_get(0)
                .map_err(|e| Error::db(format!("attachment parameter key: {e}")))?;
            let value: Option<String> = row
                .try_get(1)
                .map_err(|e| Error::db(format!("attachment parameter value: {e}")))?;
            let index = PARAMETERS
                .iter()
                .position(|p| p.0 == key)
                .ok_or_else(|| Error::programming("Unexpected attachment parameter key."))?;
            values[index] = parse(&PARAMETERS[index], value.as_deref())?;
            present[index] = true;
        }
        Ok(Self {
            delta_read: values[0],
            compact_batch: values[1],
            compact_interval: values[2] as u64,
            idle: present[3].then_some(values[3] as u64),
            receive: present[4].then_some(values[4] as u64),
            gc_interval: values[5] as u64,
            cleanup_interval: values[6] as u64,
            reconcile_interval: values[7] as u64,
            batch: values[8],
            composer_retention: values[9] as u64,
        })
    }
}
