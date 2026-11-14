// SPDX-License-Identifier: MIT OR Apache-2.0
//! Lock-first lifecycle opening. Private connection handling, controlled public seam.
use super::*;

impl Transaction {
    /// Reopen read-only lifecycle preparation with table locks before the first snapshot read.
    ///
    /// The matching module controller must already hold `operation` and `schema` transaction
    /// advisory locks, in that order. Their session counterparts bridge the rollback on this
    /// same connection. No hook, schema/data mutation or staged write may precede this call.
    /// Tables are schema-qualified identifiers, never SQL. The resulting connection is tainted
    /// and closed on drop, including cancellation, so neither session locks nor settings leak.
    ///
    /// # Errors
    /// Programming for foreign control, wrong state, prior mutations, scopes, empty tables or
    /// unbounded timeouts. Database and lock errors retain their kinds and SQLSTATE.
    #[doc(hidden)]
    pub async fn operation_reopen_locked(
        &mut self,
        control: &OperationControl,
        tables: &[(String, String)],
        operation: (i32, i32),
        schema: i64,
        limits: (Duration, Duration),
    ) -> Result<()> {
        self.check_control(control, "Transaction::operation_reopen_locked")?;
        self.require_system_full("Transaction::operation_reopen_locked")?;
        self.refuse_in_hook_scope("Transaction::operation_reopen_locked")?;
        if self.state != State::Open
            || self.isolation != Isolation::RepeatableRead
            || self.route != crate::route::Route::Primary
            || !matches!(self.intent, TxIntent::Write)
            || self.mutations != 0
            || self.write_gens.anything_since(0)
            || !self.savepoints.is_empty()
            || !self.isolated.is_empty()
            || !self.after_commit.is_empty()
            || self.poison.is_some()
            || self.schema_changed
            || tables.is_empty()
        {
            return Err(Error::programming(
                "Lock-first lifecycle opening requires unmutated preparation, ordinary primary isolation, tables and bounded waits",
            ));
        }
        for (name, limit) in [("lock_timeout", limits.0), ("statement_timeout", limits.1)] {
            if !i32::try_from(limit.as_millis()).is_ok_and(|ms| ms > 0) {
                return Err(Error::programming(format!(
                    "Lock-first lifecycle {name} must fit PostgreSQL's positive millisecond range"
                )));
            }
        }
        let mut tables = tables.to_vec();
        tables.sort();
        tables.dedup();
        for (schema, table) in &tables {
            if schema.is_empty()
                || table.is_empty()
                || schema.len() > 63
                || table.len() > 63
                || schema.contains('\0')
                || table.contains('\0')
            {
                return Err(Error::programming(
                    "Lock-first lifecycle table identity is invalid",
                ));
            }
        }
        let quoted: Vec<_> = tables
            .iter()
            .map(|(s, t)| format!("{}.{}", erp_sql::quote(s), erp_sql::quote(t)))
            .collect();
        // Set before any await that could acquire a session lock. Drop closes the physical
        // connection even while Idle, including cancellation between ROLLBACK and BEGIN.
        self.connection_tainted = true;
        let result = async {
            self.query("SELECT pg_catalog.pg_advisory_lock($1,$2)", &[&operation.0,&operation.1]).await?;
            self.query("SELECT pg_catalog.pg_advisory_lock($1)", &[&schema]).await?;
            self.rollback_inner().await?;
            let settings = format!("SET lock_timeout = '{}ms'; SET statement_timeout = '{}ms'", limits.0.as_millis(), limits.1.as_millis());
            let set = self.conn()?.batch_execute(&settings).await;
            set.map_err(|e| self.statement_failed(e))?;
            self.initial_locks = Some(format!("LOCK TABLE {} IN ACCESS EXCLUSIVE MODE; ", quoted.join(", ")));
            self.ensure_open().await?;
            self.query("SELECT pg_catalog.pg_advisory_xact_lock($1,$2)", &[&operation.0,&operation.1]).await?;
            self.query("SELECT pg_catalog.pg_advisory_xact_lock($1)", &[&schema]).await?;
            let released = self.query("SELECT pg_catalog.pg_advisory_unlock($1,$2), pg_catalog.pg_advisory_unlock($3)", &[&operation.0,&operation.1,&schema]).await?;
            let row = released.first().ok_or_else(|| Error::programming("Lifecycle advisory unlock returned no row"))?;
            let operation_unlocked: bool = row.try_get(0).map_err(|e| Error::db("Cannot decode lifecycle unlock").with_source(e))?;
            let schema_unlocked: bool = row.try_get(1).map_err(|e| Error::db("Cannot decode schema unlock").with_source(e))?;
            if !operation_unlocked || !schema_unlocked { return Err(Error::programming("Lifecycle session-lock handoff was not intact")); }
            Ok(())
        }.await;
        if result.is_err() {
            self.release();
        }
        result
    }
}
