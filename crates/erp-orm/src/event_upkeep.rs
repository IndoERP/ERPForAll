// SPDX-License-Identifier: MIT OR Apache-2.0
//! Partition upkeep of the two event tables, the outbox `erp_event.ir_event_outbox` and the audit
//! trail `erp_event.ir_audit_entry` (slice H1 of
//! ).
//!
//! The generic partition upkeep ([`Orm::maintain_partitions`]) never touches the closed event
//! namespace; these tables are kept here instead, at server start and by an hourly tick of every
//! node, on an ORM connected as the owner role. Each table is maintained in a transaction of its
//! own at `READ COMMITTED` on the primary, under the namespace advisory lock of the install
//! (`event_schema.rs`): the lock statement takes no snapshot, so a process that waited for another
//! one's upkeep sees the partitions it made and creates nothing. The transaction's `lock_timeout`
//! is [`crate::OrmConfig::partition_lock_timeout`]: `CREATE TABLE ... PARTITION OF` locks the
//! parent, so live inserts queue behind it for at most that long, and a wait cut by the timeout
//! is the reported outcome [`EventTableUpkeep::Busy`], retried at the next tick. A table that is
//! not installed is reported, not an error. Nothing is retired here: both tables keep every
//! partition until the retention slice.

use erp_fields::{Error, Result};

use crate::audit::AuditStoreState;
use crate::event_schema::{OUTBOX, TableDef, consecutive_partition_days, event_table_oid};
use crate::{
    BeginOptions, Isolation, Orm, PARTITION_LOCK_TIMEOUT_BOUNDS, PrincipalHint, SUPERUSER_ID,
};

/// The qualified name of the outbox table, as [`EventPartitionWindow::table`] names it.
pub const OUTBOX_TABLE_NAME: &str = "erp_event.ir_event_outbox";
/// The qualified name of the audit table, as [`EventPartitionWindow::table`] names it.
pub const AUDIT_TABLE_NAME: &str = "erp_event.ir_audit_entry";

/// What the event partition upkeep did to one event table.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum EventTableUpkeep {
    /// The table does not exist. Normal for the outbox until a model opts into events; for the
    /// audit table the server's audit admission refuses to serve.
    NotInstalled,
    /// Another session held a lock the upkeep needed (the parent table, or the namespace lock of
    /// another upkeep or install) longer than [`crate::OrmConfig::partition_lock_timeout`]. Nothing
    /// was changed; the next run tries again.
    Busy,
    /// The table passed the shape checks and every day of its window has a partition now.
    Maintained {
        /// Partitions created by this run, in date order (empty when the window was complete).
        created: Vec<String>,
    },
}

/// The remaining partition window of one event table: the health figure of the upkeep.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct EventPartitionWindow {
    /// The qualified table name ([`OUTBOX_TABLE_NAME`] or [`AUDIT_TABLE_NAME`]).
    pub table: &'static str,
    /// Whether the table exists.
    pub installed: bool,
    /// Whether today (UTC) has an attached partition. `false` means inserts fail now. `false` when
    /// not installed.
    pub today_covered: bool,
    /// Consecutive UTC days after today with an attached partition: 0 when only today has one (or
    /// today has none, see [`today_covered`](Self::today_covered)), 1 for today and tomorrow.
    /// Counted up to 63 days. The same convention as the `days_left` of
    /// [`AuditStoreState::PartitionWindowShort`]. 0 when not installed.
    pub days_left: u32,
}

impl EventPartitionWindow {
    /// The window of a table that is not installed.
    fn not_installed(table: &'static str) -> Self {
        Self {
            table,
            installed: false,
            today_covered: false,
            days_left: 0,
        }
    }

    /// The window of an installed table from the number of consecutive days with a partition,
    /// counted from today and including it ([`consecutive_partition_days`]).
    fn from_consecutive(table: &'static str, consecutive: u32) -> Self {
        Self {
            table,
            installed: true,
            today_covered: consecutive > 0,
            days_left: consecutive.saturating_sub(1),
        }
    }

    /// True for an installed table whose today has no partition or with fewer than 2 days after
    /// today: the upkeep is late, and inserts fail within two UTC midnights or already do. The
    /// audit store admission is ready from the same 2 days on.
    pub fn is_short(&self) -> bool {
        self.installed && (!self.today_covered || self.days_left < 2)
    }
}

/// What [`Orm::maintain_event_partitions`] did.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct EventUpkeepReport {
    /// The outbox table.
    pub outbox: EventTableUpkeep,
    /// The audit table.
    pub audit: EventTableUpkeep,
    /// The window of each table (outbox first), read after the work.
    pub health: Vec<EventPartitionWindow>,
}

/// One run of the scheduled upkeep ([`Orm::event_upkeep_tick`]).
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct EventUpkeepTick {
    /// What the upkeep did.
    pub report: EventUpkeepReport,
    /// The audit store state of the runtime ORM, checked again after the upkeep.
    pub audit_store: AuditStoreState,
}

/// SQLSTATE `lock_not_available`: a lock wait cut by `lock_timeout`.
const LOCK_NOT_AVAILABLE: &str = "55P03";

impl Orm {
    /// Keep the partition window of the outbox and the audit table: for each, under the namespace
    /// lock, the shape checks of the install, the missing days of its window (today plus
    /// [`crate::EVENT_PREMAKE_DAYS`] for the outbox, today plus [`crate::audit::AUDIT_PREMAKE_DAYS`]
    /// for the audit table) and the partition checks; then the window of both (see the module
    /// documentation). Safe to run from several processes at once: one of them creates a missing
    /// day, the others find it. This ORM must connect as the owner role.
    ///
    /// # Errors
    ///
    /// `Programming` for an out-of-bounds [`crate::OrmConfig::partition_lock_timeout`], or with an
    /// [`crate::EventSchemaProblem`] source when a table or partition differs from its definition;
    /// `Access` when the connected role may not run the System principal's transaction;
    /// `Database` when a statement fails for another reason than a lock timeout. A table that is
    /// not installed or whose lock wait timed out is an outcome of the report, not an error. The
    /// tables are maintained independently: when one fails, the other's work is still done and
    /// committed, and the error is the failing table's (both, joined with `and_also`, when both
    /// fail).
    pub async fn maintain_event_partitions(&self) -> Result<EventUpkeepReport> {
        let timeout = self.shared.config.partition_lock_timeout;
        let (floor, ceiling) = PARTITION_LOCK_TIMEOUT_BOUNDS;
        if !(floor..=ceiling).contains(&timeout) {
            return Err(Error::programming(format!(
                "OrmConfig::partition_lock_timeout is {timeout:?}; it must be from {floor:?} to \
                 {ceiling:?} (0 would wait forever)"
            )));
        }
        // each table independently: a broken outbox must not let the audit window run out
        let outbox = self.maintain_event_table(&OUTBOX, timeout).await;
        let audit = self
            .maintain_event_table(&crate::audit::AUDIT_TABLE, timeout)
            .await;
        let (outbox, audit) = match (outbox, audit) {
            (Ok(outbox), Ok(audit)) => (outbox, audit),
            (Err(error), Ok(_)) | (Ok(_), Err(error)) => return Err(error),
            (Err(outbox), Err(audit)) => return Err(outbox.and_also(&audit)),
        };
        let health = self.event_partition_health().await?;
        Ok(EventUpkeepReport {
            outbox,
            audit,
            health,
        })
    }

    /// The remaining window of the outbox and the audit table, outbox first (the upkeep's health
    /// figure, for logs and later metrics). A catalog read in a transaction of its own; any role
    /// that may read the catalog can run it.
    ///
    /// # Errors
    ///
    /// `Database` when the catalog cannot be read.
    pub async fn event_partition_health(&self) -> Result<Vec<EventPartitionWindow>> {
        let mut tx = self.begin(PrincipalHint::system()).await?;
        let mut windows = Vec::with_capacity(2);
        let mut read = Ok(());
        for (def, table) in [
            (&OUTBOX, OUTBOX_TABLE_NAME),
            (&crate::audit::AUDIT_TABLE, AUDIT_TABLE_NAME),
        ] {
            let window = match event_table_oid(&mut tx, def).await {
                Ok(None) => Ok(EventPartitionWindow::not_installed(table)),
                Ok(Some((oid, _))) => consecutive_partition_days(&mut tx, oid, def)
                    .await
                    .map(|days| EventPartitionWindow::from_consecutive(table, days)),
                Err(error) => Err(error),
            };
            match window {
                Ok(window) => windows.push(window),
                Err(error) => {
                    read = Err(error);
                    break;
                }
            }
        }
        match (read, tx.rollback().await) {
            (Ok(()), Ok(())) => Ok(windows),
            (Err(error), Ok(())) => Err(error),
            (Ok(()), Err(rollback)) => Err(rollback),
            (Err(error), Err(rollback)) => Err(error.and_also(&rollback)),
        }
    }

    /// One run of the scheduled upkeep: [`maintain_event_partitions`](Self::maintain_event_partitions)
    /// on this ORM (the owner role), then [`check_audit_store`](Self::check_audit_store) on
    /// `runtime`, the ORM requests run on. The store state is cached per ORM lineage, so a window
    /// found short before the upkeep would keep refusing audited writes until checked again.
    ///
    /// # Errors
    ///
    /// As [`maintain_event_partitions`](Self::maintain_event_partitions) and
    /// [`check_audit_store`](Self::check_audit_store); the store is checked again even when the
    /// upkeep failed (the cached state is then refreshed and the upkeep's error returned).
    pub async fn event_upkeep_tick(&self, runtime: &Orm) -> Result<EventUpkeepTick> {
        let report = self.maintain_event_partitions().await;
        // checked even after a failed upkeep: the other table's work may have refreshed the window
        let audit_store = runtime.check_audit_store().await;
        match (report, audit_store) {
            (Ok(report), Ok(audit_store)) => Ok(EventUpkeepTick {
                report,
                audit_store,
            }),
            (Err(error), Ok(_)) | (Ok(_), Err(error)) => Err(error),
            (Err(upkeep), Err(store)) => Err(upkeep.and_also(&store)),
        }
    }

    /// The upkeep of one table in a transaction of its own (see the module documentation).
    async fn maintain_event_table(
        &self,
        def: &TableDef,
        timeout: std::time::Duration,
    ) -> Result<EventTableUpkeep> {
        let mut tx = self
            .begin_opts(
                BeginOptions::new(SUPERUSER_ID)
                    .isolation(Isolation::ReadCommitted)
                    .on_primary(),
            )
            .await?;
        let work = async {
            tx.require_system_full("Orm::maintain_event_partitions")?;
            tx.batch(&format!(
                "SET LOCAL lock_timeout = '{}ms'",
                timeout.as_millis()
            ))
            .await?;
            tx.maintain_table_partitions(def).await
        }
        .await;
        match work {
            Ok(Some(created)) => {
                tx.commit().await?;
                Ok(EventTableUpkeep::Maintained { created })
            }
            Ok(None) => {
                tx.rollback().await?;
                Ok(EventTableUpkeep::NotInstalled)
            }
            Err(error) if error.sqlstate() == Some(LOCK_NOT_AVAILABLE) => {
                tx.rollback()
                    .await
                    .map_err(|rollback| error.and_also(&rollback))?;
                Ok(EventTableUpkeep::Busy)
            }
            Err(error) => match tx.rollback().await {
                Ok(()) => Err(error),
                Err(rollback) => Err(error.and_also(&rollback)),
            },
        }
    }
}
