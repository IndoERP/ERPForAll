//! The snapshot descriptor of the running transaction (4).

use erp_fields::{Error, Result};
use erp_store_wire::{SnapshotDescriptor, Xid8};

use crate::route::Route;
use crate::transaction::{Isolation, Transaction};

/// One statement, so the snapshot and the own-xid read see the same transaction state.
const SNAPSHOT_SQL: &str =
    "SELECT pg_current_snapshot()::text, pg_current_xact_id_if_assigned()::text";

impl Transaction {
    /// The snapshot descriptor of this transaction: `{xmin, xmax, xip, own}` from PostgreSQL's
    /// `pg_current_snapshot()` and this transaction's own xid.
    ///
    /// Pending ORM writes are flushed first, so the descriptor covers what this transaction has
    /// written. The transaction must be REPEATABLE READ (assumption A1 of the messaging store
    /// spec: one snapshot for the whole transaction, fixed before its xid is assigned, so `own`
    /// is never below `xmax`); under READ COMMITTED `pg_current_snapshot()` is a fresh snapshot
    /// for every statement and does not describe the data earlier statements read, so the call is
    /// refused. `own` is `None` while the transaction has no xid (a read-only transaction, or one
    /// that has written nothing yet), and equals `pg_current_xact_id()` once it has one. Under
    /// REPEATABLE READ the first statement fixes the snapshot, so the descriptor taken first is
    /// the snapshot every later read of the transaction sees.
    ///
    /// # Errors
    /// `Programming` naming A1 when the transaction is not REPEATABLE READ and naming A2 when it ran
    /// on the read pool (both checked before anything is sent); the errors of [`Transaction::flush`] (including the refusal in a raw-statements-only
    /// scope) and of the statement; `Internal` carrying the wire error as its source when the
    /// database answer is not a valid descriptor (never a default descriptor).
    ///
    /// ```no_run
    /// # async fn demo(orm: &erp_orm::Orm) -> erp_fields::Result<()> {
    /// let mut tx = orm.begin(erp_orm::SUPERUSER_ID).await?;
    /// let asof = tx.store_snapshot().await?;
    /// assert!(asof.own().is_none());
    /// tx.rollback().await?;
    /// # Ok(()) }
    /// ```
    pub async fn store_snapshot(&mut self) -> Result<SnapshotDescriptor> {
        if self.isolation_level() != Isolation::RepeatableRead {
            return Err(Error::programming(
                "store_snapshot: assumption A1 (one snapshot for the whole transaction) needs a \
                 REPEATABLE READ transaction; this one was opened with \
                 BeginOptions::isolation(ReadCommitted)",
            ));
        }
        if self.route() != Route::Primary {
            return Err(Error::programming(
                "store_snapshot: store answers are read on the primary only (assumption A2: \
                 snapshots of a standby are not shown to nest with the primary's); this \
                 transaction ran on the read pool. Serve the call from the primary \
                 (messaging store decision D-4; declare the method in PRIMARY_READS)",
            ));
        }
        self.flush().await?;
        let rows = self.query(SNAPSHOT_SQL, &[]).await?;
        let [row] = rows.as_slice() else {
            return Err(Error::internal(format!(
                "store_snapshot: expected one row, the database returned {}",
                rows.len()
            )));
        };
        let text: String = row.try_get(0).map_err(|e| {
            Error::internal("store_snapshot: pg_current_snapshot() is not text").with_source(e)
        })?;
        let own: Option<String> = row
            .try_get(1)
            .map_err(|e| Error::internal("store_snapshot: own xid is not text").with_source(e))?;
        let own = own
            .map(|text| {
                Xid8::parse(&text).map_err(|e| {
                    Error::internal(format!("store_snapshot: own xid {text:?} is invalid"))
                        .with_source(e)
                })
            })
            .transpose()?;
        SnapshotDescriptor::parse_pg_snapshot(&text, own).map_err(|e| {
            Error::internal(format!("store_snapshot: snapshot {text:?} is invalid")).with_source(e)
        })
    }
}
