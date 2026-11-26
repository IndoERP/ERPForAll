// SPDX-License-Identifier: MIT OR Apache-2.0
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use erp_fields::{Date, Error};
use erp_orm::audit::{
    AuditRetention,
    seal::{AnchorInventory, WitnessEvidence},
};
use erp_server::config::ServerConfig;
use tokio::time::{Instant, MissedTickBehavior};

use super::FileWitness;
use crate::args::AuditAction;
use crate::error::Result;
use crate::human::{config, failed, owner_maintenance_orm, runtime};

fn date(raw: &str) -> Result<Date> {
    Date::parse_odoo(raw)
        .filter(|day| day.to_string() == raw)
        .ok_or_else(|| failed("Audit retention day must be a UTC date in YYYY-MM-DD form"))
}

fn witness(config: &ServerConfig) -> erp_fields::Result<WitnessEvidence> {
    let Some(path) = config.audit_witness_file() else {
        return Ok(WitnessEvidence::NotConfigured);
    };
    let read = || AnchorInventory::new(FileWitness::new(path)?.inventory()?);
    read().map(WitnessEvidence::Inventory).map_err(|error| {
        Error::validation("Could not read the configured audit witness inventory")
            .with_source(error)
    })
}

fn outcome(result: AuditRetention) -> Result<String> {
    let text = match result {
        AuditRetention::KeepAll => "Audit retention keeps every partition.".to_owned(),
        AuditRetention::Busy => "Audit retention owner lease is already held.".to_owned(),
        AuditRetention::NothingEligible => {
            "No audit partition is eligible for archival.".to_owned()
        }
        AuditRetention::Archived { day, rows } => {
            format!("Archived audit partition {day} with {rows} rows.")
        }
        _ => return Err(failed("Unrecognized audit retention result.")),
    };
    Ok(text)
}

pub(crate) fn retention(action: &AuditAction) -> Result<String> {
    runtime()?.block_on(async {
        match action {
            AuditAction::Retain(args) => {
                let config = config(&args.config)?;
                let orm = owner_maintenance_orm(&config)?;
                let days = config.audit_retention_days();
                let witness = if days.is_some() {
                    witness(&config)?
                } else {
                    WitnessEvidence::NotConfigured
                };
                outcome(orm.retain_audit(days, false, &witness).await?)
            }
            AuditAction::Hold {
                server,
                day,
                release,
            } => {
                let day = date(day)?;
                let config = config(&server.config)?;
                let orm = owner_maintenance_orm(&config)?;
                let changed = orm.set_audit_hold(day, !release).await?;
                match (changed, release) {
                    (true, false) => Ok(format!("Placed an audit hold on {day}.")),
                    (true, true) => Ok(format!("Released the audit hold on {day}.")),
                    (false, false) => Ok(format!("Audit hold on {day} already exists.")),
                    (false, true) => Ok(format!("Audit hold on {day} is already absent.")),
                }
            }
            AuditAction::Restore { server, day } => {
                let day = date(day)?;
                let config = config(&server.config)?;
                let orm = owner_maintenance_orm(&config)?;
                let rows = orm.restore_audit_partition(day, &witness(&config)?).await?;
                Ok(format!(
                    "Restored audit partition {day} with {rows} rows and placed it on hold."
                ))
            }
            AuditAction::Maintain(_) => {
                Err(failed("Partition premake is a separate owner operation."))
            }
        }
    })
}

pub(crate) async fn supervise_retention(
    orm: &erp_orm::Orm,
    config: &ServerConfig,
) -> std::io::Result<()> {
    let Some(days) = config.audit_retention_days() else {
        return std::future::pending().await;
    };
    const DAY: u64 = 86_400;
    let seconds = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(std::io::Error::other)?
        .as_secs();
    let start = Instant::now() + Duration::from_secs((DAY + 7200 - seconds % DAY) % DAY);
    let mut ticks = tokio::time::interval_at(start, Duration::from_secs(DAY));
    ticks.set_missed_tick_behavior(MissedTickBehavior::Skip);
    loop {
        ticks.tick().await;
        for attempt in 1..=3 {
            let result =
                async { orm.retain_audit(Some(days), true, &witness(config)?).await }.await;
            match result {
                Ok(AuditRetention::Archived { day, rows }) => {
                    tracing::info!(%day, rows, job = "core.audit.retention", "archived audit partition");
                    break;
                }
                Ok(_) => break,
                Err(error) if attempt < 3 => {
                    tracing::warn!(kind = ?error.kind(), cause = %crate::server_runtime::sanitized_cause(&error),
                        job = "core.audit.retention", attempt, "audit archival will retry");
                    tokio::time::sleep(Duration::from_secs(30 * attempt)).await;
                }
                Err(error) => {
                    tracing::error!(kind = ?error.kind(), cause = %crate::server_runtime::sanitized_cause(&error),
                    job = "core.audit.retention", "audit archival blocked until the next scheduled pass")
                }
            }
        }
    }
}
