//! Store readiness: "the required sink". There is no recording, in-memory
//! or no-op sink: a recording call checks the cached state first and refuses before any
//! statement when it is not ready.

use std::sync::Mutex;

use erp_fields::Result;

use super::schema::AUDIT_TABLE;
use crate::event_schema::{EventSchemaProblem, SCHEMA};
use crate::{Orm, PrincipalHint};

/// Whether the audit table can take rows, as last checked ([`Orm::check_audit_store`]).
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum AuditStoreState {
    /// [`Orm::check_audit_store`] has not run on this ORM (every recording call refuses).
    Unchecked,
    /// The table has its shape, today's partition and at least two days ahead, and the connected
    /// role may insert every column the writer supplies.
    Ready,
    /// `erp_event.ir_audit_entry` does not exist.
    NotInstalled,
    /// The table differs from the fixed definition.
    ShapeMismatch(EventSchemaProblem),
    /// Fewer than two days of partitions ahead of today. With one day or more left the store
    /// still accepts rows (the server logs an error); with none it refuses.
    PartitionWindowShort {
        /// Days ahead of today that have a partition (0 when today has none).
        days_left: u32,
    },
    /// The connected role lacks `INSERT` on a column the writer supplies.
    NoInsertRight,
}

impl AuditStoreState {
    /// Whether a recording call may proceed.
    pub fn allows_writes(&self) -> bool {
        matches!(
            self,
            Self::Ready | Self::PartitionWindowShort { days_left: 1.. }
        )
    }

    /// Whether startup may proceed: the same test as [`allows_writes`](Self::allows_writes). It
    /// is **not** a health verdict: a window of one day left
    /// ([`PartitionWindowShort`](Self::PartitionWindowShort)) passes it while the next UTC
    /// midnight may stop every recording call, so read [`describe`](Self::describe) and
    /// log the short window.
    pub fn is_ready(&self) -> bool {
        self.allows_writes()
    }

    /// A short name for error texts and logs.
    pub fn describe(&self) -> String {
        match self {
            Self::Unchecked => "the store was not checked yet".to_owned(),
            Self::Ready => "ready".to_owned(),
            Self::NotInstalled => "erp_event.ir_audit_entry is not installed".to_owned(),
            Self::ShapeMismatch(problem) => {
                format!(
                    "the table differs from its definition: {}",
                    problem.detail()
                )
            }
            Self::PartitionWindowShort { days_left } => {
                format!("only {days_left} day(s) of partitions ahead of today")
            }
            Self::NoInsertRight => "the connected role may not insert the audit columns".to_owned(),
        }
    }
}

/// The cached state of one database, shared by every ORM of its lineage.
#[derive(Debug)]
pub(crate) struct StoreCell(Mutex<AuditStoreState>);

impl StoreCell {
    pub(crate) fn new() -> Self {
        Self(Mutex::new(AuditStoreState::Unchecked))
    }

    pub(crate) fn state(&self) -> AuditStoreState {
        self.0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }

    fn set(&self, state: AuditStoreState) {
        *self
            .0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = state;
    }
}

/// How many days from today the readiness check looks ahead for consecutive partitions.
const WINDOW_HORIZON_DAYS: i32 = 64;

impl Orm {
    /// Check the audit store as the connected role: the table, its columns and types, today's
    /// partition and the days ahead, and the `INSERT` right on every supplied column. Caches the
    /// result in this database's lineage (rebuildable, never authoritative) and returns it.
    ///
    /// # Errors
    ///
    /// `Database` when a catalog read fails.
    pub async fn check_audit_store(&self) -> Result<AuditStoreState> {
        let state = self.read_audit_store().await?;
        self.shared.audit_store().set(state.clone());
        Ok(state)
    }

    /// The cached state ([`AuditStoreState::Unchecked`] until [`check_audit_store`] ran).
    ///
    /// [`check_audit_store`]: Self::check_audit_store
    pub fn audit_store_state(&self) -> AuditStoreState {
        self.shared.audit_store().state()
    }

    async fn read_audit_store(&self) -> Result<AuditStoreState> {
        let mut tx = self.begin(PrincipalHint::system()).await?;
        let result = Self::read_audit_store_in(&mut tx).await;
        let _ = tx.rollback().await;
        result
    }

    async fn read_audit_store_in(tx: &mut crate::Transaction) -> Result<AuditStoreState> {
        let table = AUDIT_TABLE.table;
        // Catalog reads without a name lookup: `to_regclass` itself fails without schema USAGE.
        let found = tx
            .query(
                "SELECT c.oid::int8, c.relkind::text FROM pg_class c \
                 JOIN pg_namespace n ON n.oid = c.relnamespace \
                 WHERE n.nspname = $1 AND c.relname = $2",
                &[&SCHEMA, &table],
            )
            .await?;
        let Some(row) = found.first() else {
            return Ok(AuditStoreState::NotInstalled);
        };
        let (oid, kind): (i64, String) = (row.get(0), row.get(1));
        if kind != "p" {
            return Ok(AuditStoreState::ShapeMismatch(
                EventSchemaProblem::ForgedRelation(format!(
                    "\"{SCHEMA}\".\"{table}\" is not a partitioned table (relkind {kind})"
                )),
            ));
        }
        let columns = tx
            .query(
                "SELECT a.attname::text, format_type(a.atttypid, a.atttypmod), a.attnotnull, \
                 a.atthasdef, has_column_privilege(a.attrelid, a.attnum, 'INSERT') \
                 FROM pg_attribute a WHERE a.attrelid = $1::int8::oid AND a.attnum > 0 \
                 AND NOT a.attisdropped ORDER BY a.attnum",
                &[&oid],
            )
            .await?;
        let found: Vec<(String, String, bool, bool, bool)> = columns
            .iter()
            .map(|r| (r.get(0), r.get(1), r.get(2), r.get(3), r.get(4)))
            .collect();
        for (name, ..) in &found {
            if !AUDIT_TABLE.columns.iter().any(|c| c.name == name) {
                return Ok(AuditStoreState::ShapeMismatch(
                    EventSchemaProblem::ForgedColumn(format!(
                        "\"{table}\" has an unexpected column \"{name}\""
                    )),
                ));
            }
        }
        let mut insertable = true;
        for want in AUDIT_TABLE.columns {
            let Some((_, ty, not_null, has_default, may_insert)) =
                found.iter().find(|(n, ..)| n == want.name)
            else {
                return Ok(AuditStoreState::ShapeMismatch(
                    EventSchemaProblem::ForgedColumn(format!(
                        "\"{table}\" lacks column \"{}\"",
                        want.name
                    )),
                ));
            };
            if ty != want.ty || *not_null != want.not_null || *has_default != want.default.is_some()
            {
                return Ok(AuditStoreState::ShapeMismatch(
                    EventSchemaProblem::ForgedType(format!(
                        "column \"{}\" of \"{table}\" is {ty} (not null: {not_null}, default: \
                         {has_default}), expected {} (not null: {}, default: {})",
                        want.name,
                        want.ty,
                        want.not_null,
                        want.default.is_some()
                    )),
                ));
            }
            if want.default.is_none() && !may_insert {
                insertable = false;
            }
        }
        if !insertable {
            return Ok(AuditStoreState::NoInsertRight);
        }
        // consecutive daily partitions from today (UTC): the first missing day ends the window,
        // whatever lies beyond the gap
        let window = tx
            .query(
                &format!(
                    "SELECT COALESCE(min(g.n) FILTER (WHERE p.d IS NULL), {horizon})::int4 \
                     FROM generate_series(0, {horizon} - 1) AS g(n) \
                     LEFT JOIN (SELECT to_date(substr(c.relname, {from}), 'YYYYMMDD') AS d \
                       FROM pg_inherits i JOIN pg_class c ON c.oid = i.inhrelid \
                       WHERE i.inhparent = $1::int8::oid \
                       AND c.relname ~ '^{table}_p[0-9]{{8}}$') p \
                       ON p.d = (now() AT TIME ZONE 'UTC')::date + g.n",
                    from = table.len() + 3,
                    horizon = WINDOW_HORIZON_DAYS
                ),
                &[&oid],
            )
            .await?;
        // the number of consecutive days present from today: today alone is 1, today missing is 0
        let present: i32 = window[0].get(0);
        let days_left = u32::try_from(present.saturating_sub(1)).unwrap_or(0);
        if days_left < 2 {
            return Ok(AuditStoreState::PartitionWindowShort { days_left });
        }
        Ok(AuditStoreState::Ready)
    }
}
