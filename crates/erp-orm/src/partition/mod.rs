//! Leaf partitioning at run time: the upkeep calls
//! that keep the partition window ([`Orm::ensure_partitions`], [`Orm::retire_partitions`],
//! [`Orm::maintain_partitions`]), their [`PartitionReport`], the window health check
//! ([`Orm::partition_health`]), the schema-sync side (creating, adopting and refusing to convert
//! partitioned tables) and the errors of the insert path (a missing partition, a caller value for
//! a server-set field).
//!
//! The upkeep runs on the **owner** pool (the request role has no DDL right), outside
//! any request transaction, on a connection it takes out of the pool and closes afterwards, so
//! its session settings and its advisory lock can never leak into a pooled connection. It bumps
//! no generation: attaching or detaching a partition changes no result shape, and PostgreSQL
//! re-plans the statements that depend on the parent itself (design §7.9).

mod catalog;
pub(crate) mod sync;
mod upkeep;

use std::fmt;

use erp_fields::{DateTime, Error, ErrorKind, Result};
use erp_registry::{FieldSpec, ModelSpec, PartitionInterval, Registry};

/// How far ahead [`Orm::ensure_partitions`](crate::Orm::ensure_partitions) creates partitions.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum Horizon {
    /// The declared window: the current interval and `premake` intervals after it.
    Declared,
    /// Every interval from the current one through the one holding this instant (UTC).
    Through(DateTime),
}

/// Which partitions [`Orm::retire_partitions`](crate::Orm::retire_partitions) retires: those whose
/// whole range ends at or before the cutoff.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum KeepAfter {
    /// The declared retention: keep the current interval and `retain` intervals before it.
    Declared,
    /// Keep the partitions that end after this instant (UTC, not later than now).
    Time(DateTime),
}

/// The step of the upkeep a failure happened in.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum PartitionStep {
    /// Creating and attaching a partition of the window.
    Create,
    /// Finishing a detach that was interrupted (`DETACH PARTITION … FINALIZE`).
    Finalize,
    /// Marking and detaching a retired partition (`DETACH PARTITION … CONCURRENTLY`).
    Detach,
    /// Dropping a detached partition.
    Drop,
    /// Starting the upkeep of a model in [`Orm::maintain_partitions`](crate::Orm::maintain_partitions):
    /// taking its upkeep lock, reading its partitions, checking its window (the entry names the
    /// model's table; the other models are maintained all the same).
    Prepare,
    /// Releasing the model's upkeep lock after its work: the work is done and in the report, but
    /// the lock was not released cleanly (it is released when the upkeep's connection closes).
    Unlock,
}

impl PartitionStep {
    fn name(self) -> &'static str {
        match self {
            Self::Create => "create",
            Self::Finalize => "finalize",
            Self::Detach => "detach",
            Self::Drop => "drop",
            Self::Prepare => "prepare",
            Self::Unlock => "unlock",
        }
    }
}

impl fmt::Display for PartitionStep {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.name())
    }
}

/// What the upkeep did with one partition.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum PartitionAction {
    /// Created and attached (a partition of the window was missing).
    Created,
    /// A partition of the window that was attached already.
    Existed,
    /// An interrupted detach completed (`FINALIZE`).
    Finalized,
    /// Detached concurrently (retired).
    Detached,
    /// Dropped after its detach (retired), or left detached by an interrupted retirement and
    /// dropped now.
    Dropped,
    /// The drop of a detached partition gave up waiting for a lock within
    /// [`PARTITION_SHORT_LOCK_TIMEOUT`]: dropping the table takes `ACCESS EXCLUSIVE` on the tables
    /// its foreign keys point to, and the readers of those tables queue behind a waiting drop. The
    /// table stays detached and marked as retired; the next retirement or maintenance drops it.
    /// Not a failure: nothing is lost and nothing is wrong, the drop is only postponed.
    #[non_exhaustive]
    DropDeferred {
        /// The database's message and SQLSTATE of the lock wait that was cut.
        reason: String,
    },
    /// A step failed; the partition is as the failed statement left it (a failed detach may leave
    /// it pending, which the next call finalizes). Read the fields through
    /// [`PartitionEntry::step`], [`reason`](PartitionEntry::reason),
    /// [`sqlstate`](PartitionEntry::sqlstate) and [`error_kind`](PartitionEntry::error_kind); a
    /// scheduler tells a wait cut by the lock timeout ([`PartitionEntry::is_lock_timeout`], retry
    /// at the next run) from a fault by them.
    #[non_exhaustive]
    Failed {
        /// The step that failed.
        step: PartitionStep,
        /// The database's message and SQLSTATE, or the upkeep's own text.
        reason: String,
        /// The SQLSTATE of the failed statement (`55P03` for a lock wait cut by `lock_timeout`),
        /// when the database raised it.
        sqlstate: Option<String>,
        /// The kind of the error behind the failure, when it was one (`Lock` for `55P03`).
        kind: Option<ErrorKind>,
    },
}

impl PartitionAction {
    /// A failed step with its structured cause.
    pub(crate) fn failed(
        step: PartitionStep,
        reason: String,
        sqlstate: Option<String>,
        kind: Option<ErrorKind>,
    ) -> Self {
        Self::Failed {
            step,
            reason,
            sqlstate,
            kind,
        }
    }

    /// A step that failed with the upkeep's own text, no database error behind it.
    pub(crate) fn failed_text(step: PartitionStep, reason: String) -> Self {
        Self::failed(step, reason, None, None)
    }

    /// A deferred drop.
    pub(crate) fn drop_deferred(reason: String) -> Self {
        Self::DropDeferred { reason }
    }
}

/// One action of a [`PartitionReport`].
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct PartitionEntry {
    model: &'static str,
    partition: String,
    action: PartitionAction,
}

impl PartitionEntry {
    /// The partitioned model.
    pub fn model(&self) -> &'static str {
        self.model
    }

    /// The partition's table name.
    pub fn partition(&self) -> &str {
        &self.partition
    }

    /// What happened to it.
    pub fn action(&self) -> &PartitionAction {
        &self.action
    }

    /// The step of a failed entry (`None` for any other action).
    pub fn step(&self) -> Option<PartitionStep> {
        match &self.action {
            PartitionAction::Failed { step, .. } => Some(*step),
            _ => None,
        }
    }

    /// The reason of a failed or deferred entry (`None` for any other action).
    pub fn reason(&self) -> Option<&str> {
        match &self.action {
            PartitionAction::Failed { reason, .. } | PartitionAction::DropDeferred { reason } => {
                Some(reason)
            }
            _ => None,
        }
    }

    /// The SQLSTATE of a failed entry, when the database raised one.
    pub fn sqlstate(&self) -> Option<&str> {
        match &self.action {
            PartitionAction::Failed { sqlstate, .. } => sqlstate.as_deref(),
            _ => None,
        }
    }

    /// The error kind behind a failed entry, when there was one.
    pub fn error_kind(&self) -> Option<ErrorKind> {
        match &self.action {
            PartitionAction::Failed { kind, .. } => *kind,
            _ => None,
        }
    }

    /// True for a failed entry whose wait for a lock was cut by `lock_timeout` (SQLSTATE
    /// `55P03`): nothing is wrong, the step is retried by the next upkeep run (a cut detach leaves
    /// the partition pending, which that run finalizes).
    pub fn is_lock_timeout(&self) -> bool {
        self.sqlstate() == Some(LOCK_TIMEOUT_SQLSTATE)
    }
}

/// SQLSTATE of a lock wait cut by `lock_timeout`.
const LOCK_TIMEOUT_SQLSTATE: &str = "55P03";

/// Every action of an upkeep call, in the order they happened: nothing is skipped silently. A
/// failed step is an entry with its reason, and the call goes on with the next partition; check
/// [`PartitionReport::is_complete`] or turn failures into an error with
/// [`PartitionReport::into_result`].
#[must_use = "a partition report lists failed steps; check `is_complete()` or call `into_result()`"]
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PartitionReport {
    entries: Vec<PartitionEntry>,
}

impl PartitionReport {
    pub(crate) fn push(&mut self, model: &'static str, partition: &str, action: PartitionAction) {
        self.entries.push(PartitionEntry {
            model,
            partition: partition.to_owned(),
            action,
        });
    }

    /// Append the entries of `other` (e.g. the reports of several models).
    pub fn merge(&mut self, other: PartitionReport) {
        self.entries.extend(other.entries);
    }

    /// All entries, in order.
    pub fn entries(&self) -> &[PartitionEntry] {
        &self.entries
    }

    fn names(&self, wanted: &PartitionAction) -> Vec<&str> {
        self.entries
            .iter()
            .filter(|e| &e.action == wanted)
            .map(|e| e.partition.as_str())
            .collect()
    }

    /// Partitions created, in order.
    pub fn created(&self) -> Vec<&str> {
        self.names(&PartitionAction::Created)
    }

    /// Partitions of the window that existed already.
    pub fn existed(&self) -> Vec<&str> {
        self.names(&PartitionAction::Existed)
    }

    /// Interrupted detaches completed.
    pub fn finalized(&self) -> Vec<&str> {
        self.names(&PartitionAction::Finalized)
    }

    /// Partitions detached.
    pub fn detached(&self) -> Vec<&str> {
        self.names(&PartitionAction::Detached)
    }

    /// Partitions dropped.
    pub fn dropped(&self) -> Vec<&str> {
        self.names(&PartitionAction::Dropped)
    }

    /// Partitions whose drop was postponed because a lock was not free within
    /// [`PARTITION_SHORT_LOCK_TIMEOUT`] (still detached, dropped by the next retirement). Not
    /// failures, but work left: a job may log them.
    pub fn drops_deferred(&self) -> Vec<&str> {
        self.entries
            .iter()
            .filter(|e| matches!(e.action, PartitionAction::DropDeferred { .. }))
            .map(|e| e.partition.as_str())
            .collect()
    }

    /// The failed steps.
    pub fn failures(&self) -> impl Iterator<Item = &PartitionEntry> {
        self.entries
            .iter()
            .filter(|e| matches!(e.action, PartitionAction::Failed { .. }))
    }

    /// True when no step failed.
    pub fn is_complete(&self) -> bool {
        self.failures().next().is_none()
    }

    /// The report when no step failed, else a `Database` error listing every failure.
    ///
    /// # Errors
    ///
    /// `Database`: `partition upkeep failed: <model> <partition>: <step>: <reason>; …`.
    pub fn into_result(self) -> Result<Self> {
        let failures: Vec<String> = self
            .entries
            .iter()
            .filter_map(|e| match &e.action {
                PartitionAction::Failed { step, reason, .. } => {
                    Some(format!("{} {}: {step}: {reason}", e.model, e.partition))
                }
                PartitionAction::Created
                | PartitionAction::Existed
                | PartitionAction::Finalized
                | PartitionAction::Detached
                | PartitionAction::Dropped
                | PartitionAction::DropDeferred { .. } => None,
            })
            .collect();
        if failures.is_empty() {
            Ok(self)
        } else {
            Err(Error::new(
                ErrorKind::Database,
                format!("partition upkeep failed: {}", failures.join("; ")),
            ))
        }
    }
}

impl fmt::Display for PartitionAction {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Created => f.write_str("created"),
            Self::Existed => f.write_str("existed"),
            Self::Finalized => f.write_str("finalized"),
            Self::Detached => f.write_str("detached"),
            Self::Dropped => f.write_str("dropped"),
            Self::DropDeferred { reason } => write!(f, "drop deferred: {reason}"),
            Self::Failed { step, reason, .. } => write!(f, "failed to {step}: {reason}"),
        }
    }
}

/// The partition window of one model, as [`Orm::partition_health`](crate::Orm::partition_health)
/// finds it.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct PartitionWindow {
    model: &'static str,
    covered: u32,
    premake: u32,
    interval: PartitionInterval,
    unreadable: u32,
    pending_detaches: u32,
    oldest_pending_end: Option<DateTime>,
}

impl PartitionWindow {
    /// The partitioned model.
    pub fn model(&self) -> &'static str {
        self.model
    }

    /// Consecutive intervals with an attached partition, from the current one on (0 when the
    /// current interval has none: inserts fail).
    pub fn covered(&self) -> u32 {
        self.covered
    }

    /// The declared number of intervals created ahead.
    pub fn premake(&self) -> u32 {
        self.premake
    }

    /// The model's interval.
    pub fn interval(&self) -> PartitionInterval {
        self.interval
    }

    /// Partitions the check cannot classify: attached, but their bounds are not a range of two
    /// finite timestamps (a `MINVALUE`/`MAXVALUE`/`infinity` bound, a default partition). They are
    /// never counted as covering an interval, and every retirement lists them as failed until
    /// someone detaches or fixes them by hand. Anything above 0 wants a look.
    pub fn unreadable_bounds(&self) -> u32 {
        self.unreadable
    }

    /// Partitions whose `DETACH … CONCURRENTLY` is pending: it waits for the transactions that
    /// started before it, database-wide, and the upkeep's short lock timeout cuts that wait, so
    /// one long transaction leaves the detach pending every run. Finalizing is retried at each
    /// upkeep call; a count that stays above 0 means retention is stuck (look for a long
    /// transaction or a prepared transaction).
    pub fn pending_detaches(&self) -> u32 {
        self.pending_detaches
    }

    /// The end of the oldest pending partition's range (UTC), the age marker of stuck retention:
    /// the longer ago, the longer the partition has been retired and not detached. `None` when
    /// nothing is pending (or no pending bound is readable).
    pub fn oldest_pending_end(&self) -> Option<DateTime> {
        self.oldest_pending_end
    }

    /// True when fewer than 2 intervals are covered (the current one and the next): the upkeep is
    /// late and inserts fail at the next interval boundary or already do (design §7.5).
    pub fn is_short(&self) -> bool {
        self.covered < 2
    }
}

/// The `lock_timeout` of the `DROP TABLE` of a detached partition (design §7.10). The foreign keys
/// a partition inherits from its parent (`create_uid`, `write_uid`, and any many2one) become
/// standalone after the detach, and dropping the table removes their triggers from the tables
/// they point to, which takes `ACCESS EXCLUSIVE` there: while the drop waits for a long reader of
/// `res_users`, every new reader of `res_users` queues behind it. The wait is therefore cut after
/// this long, not after [`crate::OrmConfig::partition_lock_timeout`]; the partition stays detached
/// and marked, and the next retirement drops it ([`PartitionAction::DropDeferred`]).
pub const PARTITION_SHORT_LOCK_TIMEOUT: std::time::Duration = std::time::Duration::from_millis(300);

/// Bounds of the upkeep's `lock_timeout` ([`crate::OrmConfig::partition_lock_timeout`]): never 0
/// (which would mean "wait forever"), at most an hour.
pub const PARTITION_LOCK_TIMEOUT_BOUNDS: (std::time::Duration, std::time::Duration) = (
    std::time::Duration::from_millis(100),
    std::time::Duration::from_secs(3_600),
);

/// Most partitions of one window ([`Orm::ensure_partitions`](crate::Orm::ensure_partitions), schema
/// sync): the current interval plus the largest `premake`. A horizon further ahead is refused
/// before anything runs.
pub const MAX_PARTITIONS_PER_CALL: usize = erp_registry::MAX_PARTITION_INTERVALS as usize + 1;

/// The model `name` of `registry` and its partitioning; it must be partitioned.
fn partitioned<'r>(
    registry: &'r Registry,
    name: &str,
    call: &str,
) -> Result<(&'r ModelSpec, erp_registry::Partition)> {
    let spec = registry
        .model(name)
        .ok_or_else(|| Error::programming(format!("Model '{name}' does not exist in registry.")))?;
    let partition = spec.partition.ok_or_else(|| {
        Error::programming(format!("{call} on '{name}': the model is not partitioned"))
    })?;
    Ok((spec, partition))
}

/// The partitioned models of `registry`, in registry order, with their partitioning.
fn partitioned_models(registry: &Registry) -> Vec<(&ModelSpec, erp_registry::Partition)> {
    registry
        .models()
        .iter()
        // selects the partitioned models; every other model has nothing to maintain
        .filter_map(|m| m.partition.map(|p| (m, p)))
        .collect()
}

/// `field` is set by the server: a caller value for it is refused (design §7.1, §10). The one
/// source of the text is `erp_fields::convert::server_set_error`.
pub(crate) fn server_set_error(spec: &ModelSpec, field: &FieldSpec) -> Error {
    erp_fields::convert::server_set_error(erp_fields::convert::FieldRef {
        model: spec.name,
        field: field.name,
        label: &field.string,
    })
}

/// Whether a caller value for `field` of a model is refused as set by the server (design §7.1,
/// §10): a `server_set` field, or `create_date` of a partitioned model with log access (the
/// default partition key, whichever column the model names as its key).
pub(crate) fn refuses_caller_value(spec: &ModelSpec, field: &FieldSpec) -> bool {
    let audit = field.name == "id"
        || (spec.log_access && erp_registry::MAGIC_FIELD_NAMES.contains(&field.name));
    if audit {
        spec.partition.is_some() && spec.log_access && field.name == "create_date"
    } else {
        field.server_set
    }
}

/// True when `field` is the partition key of `spec` (the column is `NOT NULL` through the primary
/// key, whatever the field says).
pub(crate) fn is_key(spec: &ModelSpec, field: erp_registry::FieldId) -> bool {
    spec.partition.is_some_and(|p| p.key() == field)
}

/// The message of an insert no partition accepts (design §7.6), when `db` is that error on the
/// table of a partitioned `model`: SQLSTATE `23514` without a constraint name, on the parent table.
/// Recognised through the registry, never through the (localised) server text; the key's value is
/// taken from the error detail, where PostgreSQL writes `(<key>) = (<value>)` without translation.
pub(crate) fn missing_partition_message(
    model: Option<&ModelSpec>,
    db: &tokio_postgres::error::DbError,
) -> Option<String> {
    if *db.code() != tokio_postgres::error::SqlState::CHECK_VIOLATION || db.constraint().is_some() {
        return None;
    }
    let model = model?;
    let partition = model.partition?;
    let key = partition.key_name();
    let value = db
        .detail()
        .and_then(|detail| key_value(detail, key))
        // the server omits the key description when the role may not read the key column; the
        // error is raised all the same, only less precise
        .unwrap_or("(not reported by the server)");
    Some(format!(
        "no partition of '{}' accepts {key} = {value}: partition upkeep has not created it yet \
         (window: {}; see the health check)",
        model.name,
        partition.interval().count(partition.premake())
    ))
}

/// The value in `… (<key>) = (<value>).` of a partition-routing error detail.
fn key_value<'a>(detail: &'a str, key: &str) -> Option<&'a str> {
    let marker = format!("({key}) = (");
    let start = detail.find(&marker)?.checked_add(marker.len())?;
    let rest = detail.get(start..)?;
    let end = rest.rfind(')')?;
    rest.get(..end)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn key_values_come_from_the_untranslated_part_of_the_detail() {
        assert_eq!(
            key_value(
                "Partition key of the failing row contains (create_date) = (2026-10-18 \
                 10:00:00.123456).",
                "create_date"
            ),
            Some("2026-10-18 10:00:00.123456")
        );
        // a translated sentence around the same description
        assert_eq!(
            key_value(
                "La clé de partitionnement de la ligne en échec contient (recorded_at) = \
                 (2026-10-18 10:00:00).",
                "recorded_at"
            ),
            Some("2026-10-18 10:00:00")
        );
        assert_eq!(key_value("no description", "create_date"), None);
        assert_eq!(key_value("(create_date) = (", "create_date"), None);
    }

    #[test]
    fn reports_list_and_fail_loudly() {
        let mut report = PartitionReport::default();
        report.push(
            "scale.event",
            "scale_event_p20261016",
            PartitionAction::Existed,
        );
        report.push(
            "scale.event",
            "scale_event_p20261017",
            PartitionAction::Created,
        );
        assert!(report.is_complete());
        assert_eq!(report.created(), ["scale_event_p20261017"]);
        assert_eq!(report.existed(), ["scale_event_p20261016"]);
        let mut other = PartitionReport::default();
        other.push(
            "scale.event",
            "scale_event_p20261010",
            PartitionAction::failed(
                PartitionStep::Detach,
                "canceling statement due to lock timeout (SQLSTATE 55P03)".to_owned(),
                Some("55P03".to_owned()),
                Some(ErrorKind::Lock),
            ),
        );
        report.merge(other);
        assert!(!report.is_complete());
        assert_eq!(report.entries().len(), 3);
        let failed = report.failures().next().unwrap();
        assert_eq!(failed.step(), Some(PartitionStep::Detach));
        assert_eq!(failed.sqlstate(), Some("55P03"));
        assert_eq!(failed.error_kind(), Some(ErrorKind::Lock));
        assert!(failed.is_lock_timeout());
        assert_eq!(
            failed.action().to_string(),
            "failed to detach: canceling statement due to lock timeout (SQLSTATE 55P03)"
        );
        assert_eq!(PartitionStep::Unlock.to_string(), "unlock");
        assert!(!report.entries()[0].is_lock_timeout());
        let error = report.into_result().unwrap_err();
        assert_eq!(error.kind(), ErrorKind::Database);
        assert_eq!(
            error.message(),
            "partition upkeep failed: scale.event scale_event_p20261010: detach: canceling \
             statement due to lock timeout (SQLSTATE 55P03)"
        );
    }
}
