// SPDX-License-Identifier: MIT OR Apache-2.0
//! Generic installed-model upkeep on a separate owner pool.
use std::time::Duration;

use deadpool_postgres::Pool;
use erp_addon_base::partition_upkeep_settings as settings;
use erp_fields::{DateTime, Error, Result};
use erp_orm::{Orm, PartitionAction, PartitionReport};

/// The explicit base-free default does not hide a broken installed base schema.
pub(crate) async fn policy(orm: &Orm, owner: &Pool) -> Result<Duration> {
    if !orm
        .registry()
        .addons()
        .iter()
        .any(|addon| addon.name == "base")
    {
        return settings::interval(None);
    }
    let bytes = i32::try_from(settings::MAX_VALUE_BYTES)
        .map_err(|_| Error::programming("partition upkeep parameter text bound exceeds i32"))?;
    let client = owner.get().await.map_err(|error| {
        Error::db("could not read generic partition upkeep policy").with_source(error)
    })?;
    let rows = client.query(
        "SELECT CASE WHEN value IS NULL OR octet_length(value)<=$2 THEN value ELSE 'invalid' END FROM ir_config_parameter WHERE key=$1 LIMIT 2",
        &[&settings::KEY, &bytes],
    ).await.map_err(|error| Error::db("could not read generic partition upkeep policy").with_source(error))?;
    let value: Option<String> = match rows.as_slice() {
        [] => None, // an absent row selects the declared default.
        [row] => row.try_get(0).map_err(|error| {
            Error::db("invalid generic partition upkeep policy column").with_source(error)
        })?,
        _ => {
            return Err(Error::programming(
                "duplicate generic partition upkeep policy rows",
            ));
        }
    };
    settings::interval(value.as_deref())
}

/// Shared by startup and the periodic tick. `orm` supplies the current installed registry;
/// only `owner` supplies DDL credentials. Preliminary connections are released before upkeep.
pub(crate) async fn maintain(orm: &Orm, owner: &Pool) -> Result<PartitionReport> {
    let now: DateTime = {
        let client = owner.get().await.map_err(|error| {
            Error::db("could not read the generic partition upkeep clock").with_source(error)
        })?;
        client
            .query_one("SELECT (now() AT TIME ZONE 'UTC')", &[])
            .await
            .map_err(|error| {
                Error::db("could not read the generic partition upkeep clock").with_source(error)
            })?
            .try_get(0)
            .map_err(|error| {
                Error::db("invalid generic partition upkeep clock").with_source(error)
            })?
    };
    let report = orm.maintain_partitions(owner, now).await?;
    for entry in report.entries() {
        match entry.action() {
            PartitionAction::Created
            | PartitionAction::Finalized
            | PartitionAction::Detached
            | PartitionAction::Dropped => {
                tracing::info!(model=entry.model(), partition=entry.partition(), action=%entry.action(), "generic partition upkeep action");
            }
            PartitionAction::Existed => {
                tracing::debug!(
                    model = entry.model(),
                    partition = entry.partition(),
                    "generic partition window already present"
                );
            }
            PartitionAction::DropDeferred { .. } => {
                tracing::warn!(
                    model = entry.model(),
                    partition = entry.partition(),
                    reason = entry.reason(),
                    "generic partition drop deferred until the next run"
                );
            }
            PartitionAction::Failed { .. } => {
                tracing::error!(model=entry.model(), partition=entry.partition(), step=?entry.step(), kind=?entry.error_kind(), sqlstate=entry.sqlstate(), reason=entry.reason(), "generic partition upkeep step failed");
            }
            other => {
                return Err(Error::programming(format!(
                    "unknown generic partition upkeep outcome: {other:?}"
                )));
            }
        }
    }
    // An Ok report can contain failed steps. Never turn those into a healthy tick.
    report.into_result()
}

/// Startup has already validated policy and maintained the window. No detached task or
/// catch-up queue: dropping this serving future cancels later work and active owner sessions.
pub(crate) async fn supervise(
    initial: Duration,
    read_policy: impl AsyncFn() -> Result<Duration>,
    tick: impl AsyncFn() -> Result<()>,
) -> std::io::Result<()> {
    let mut period = initial;
    loop {
        tokio::time::sleep(period).await;
        match read_policy().await {
            Ok(next) => period = next,
            Err(error) => {
                tracing::error!(%error, kind=?error.kind(), cause=%crate::server_runtime::sanitized_cause(&error), "generic partition upkeep policy is invalid; maintenance skipped and retried at the last validated cadence");
                continue;
            }
        }
        if let Err(error) = tick().await {
            tracing::error!(%error, kind=?error.kind(), cause=%crate::server_runtime::sanitized_cause(&error), "scheduled generic partition upkeep failed; it runs again at the next tick");
        }
    }
}
