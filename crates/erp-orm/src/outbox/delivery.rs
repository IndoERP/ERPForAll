//! Owner-side replay. A cursor advances through the fully resolved prefix of PostgreSQL xids.
//! A sequence id alone could skip a transaction that commits after a later id is visible.

use crate::{BeginOptions, BoxFuture, Isolation, Orm, SUPERUSER_ID, Transaction};
use erp_fields::{Error, Result};

const MAX_BATCH: u32 = 128;
const LOCK: &str = "erp_event.outbox_delivery";
const TABLES: &str = "\
CREATE TABLE IF NOT EXISTS erp_event.ir_outbox_consumer (
    name text PRIMARY KEY,
    last_xid pg_catalog.xid8 NOT NULL,
    last_id bigint NOT NULL CHECK (last_id >= 0),
    updated_at timestamptz NOT NULL DEFAULT pg_catalog.clock_timestamp()
);
CREATE TABLE IF NOT EXISTS erp_event.ir_outbox_retention (
    singleton boolean PRIMARY KEY CHECK (singleton),
    floor_xid pg_catalog.xid8 NOT NULL,
    floor_id bigint NOT NULL CHECK (floor_id >= 0),
    held boolean NOT NULL DEFAULT false
);
INSERT INTO erp_event.ir_outbox_retention (singleton, floor_xid, floor_id)
VALUES (true, '0'::pg_catalog.xid8, 0) ON CONFLICT DO NOTHING;
REVOKE ALL ON erp_event.ir_outbox_consumer, erp_event.ir_outbox_retention FROM PUBLIC";

/// An inclusive acknowledgement boundary. The initial cursor is `(0, 0)`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, PartialOrd, Ord)]
pub struct OutboxCursor {
    pub xid: u64,
    pub id: i64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct OutboxStatus {
    pub floor: OutboxCursor,
    pub held: bool,
    pub acknowledgement: Option<OutboxCursor>,
    /// Time since registration, progress or restore.
    pub acknowledgement_age_seconds: Option<i64>,
    /// Time since capture of the next committed row in cursor order.
    pub next_event_age_seconds: Option<i64>,
}

/// Names and ids only. This envelope is restricted to trusted owner-side consumers.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct OutboxEvent {
    pub event_id: String,
    pub kind: String,
    pub model: String,
    pub record_ids: Vec<i64>,
    pub field_names: Vec<String>,
    pub cascade_models: Vec<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OutboxReplay {
    NotInstalled,
    Busy,
    Idle,
    Delivered { count: usize, cursor: OutboxCursor },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OutboxRetention {
    NotInstalled,
    Busy,
    Held,
    Purged { count: i64, floor: OutboxCursor },
}

impl Orm {
    /// Read the committed retention floor and one named consumer's acknowledgement.
    pub async fn outbox_status(&self, consumer: &str) -> Result<Option<OutboxStatus>> {
        check_name(consumer)?;
        let mut tx = self.outbox_transaction().await?;
        let work = async {
            let row = tx.query("SELECT pg_catalog.to_regclass('erp_event.ir_outbox_retention') IS NOT NULL", &[]).await?;
            if !column::<bool>(&row[0], 0)? {
                return Ok(None);
            }
            let rows = tx.query("SELECT r.floor_xid::text, r.floor_id, r.held, c.last_xid::text, c.last_id, \
                        CASE WHEN c.updated_at IS NOT NULL THEN GREATEST(0, EXTRACT(EPOCH FROM pg_catalog.clock_timestamp() - c.updated_at))::bigint END, \
                        CASE WHEN next.create_date IS NOT NULL THEN GREATEST(0, EXTRACT(EPOCH FROM pg_catalog.clock_timestamp() - next.create_date))::bigint END \
                FROM erp_event.ir_outbox_retention r \
                LEFT JOIN erp_event.ir_outbox_consumer c ON c.name = $1 \
                LEFT JOIN LATERAL (SELECT e.create_date FROM erp_event.ir_event_outbox e \
                    WHERE (e.xid, e.id) > (COALESCE(c.last_xid, r.floor_xid), COALESCE(c.last_id, r.floor_id)) \
                    ORDER BY e.xid,e.id LIMIT 1) next ON true \
                WHERE r.singleton", &[&consumer]).await?;
            let row = rows.first().ok_or_else(|| Error::internal("outbox: missing retention state"))?;
            let acknowledgement = match (column::<Option<&str>>(row, 3)?, column::<Option<i64>>(row, 4)?) {
                (Some(xid), Some(id)) => Some(OutboxCursor { xid: parse_xid(xid)?, id }),
                (None, None) => None,
                _ => return Err(Error::internal("outbox: incomplete acknowledgement")),
            };
            Ok(Some(OutboxStatus {
                floor: decode_cursor(row)?,
                held: column(row, 2)?,
                acknowledgement,
                acknowledgement_age_seconds: column(row, 5)?,
                next_event_age_seconds: column(row, 6)?,
            }))
        }.await;
        finish(tx, work).await
    }

    /// Replay at most `limit` events, ordered by transaction xid and row id. The consumer's SQL
    /// effects and cursor advance commit together. Its callback must keep all effects in `tx`.
    /// Two processes replaying the same consumer serialize on its cursor row. Only an owner
    /// connection may use this API, and `consumer` is a stable internal name across restarts.
    pub async fn replay_outbox(
        &self,
        consumer: &str,
        limit: u32,
        apply: impl for<'a> FnOnce(&'a mut Transaction, Vec<OutboxEvent>) -> BoxFuture<'a, Result<()>>,
    ) -> Result<OutboxReplay> {
        check_name(consumer)?;
        check_limit(limit)?;
        let mut tx = self.outbox_transaction().await?;
        let work = async {
            if !prepare(&mut tx).await? {
                return Ok(OutboxReplay::NotInstalled);
            }
            if !delivery_lock(&mut tx, true).await? {
                return Ok(OutboxReplay::Busy);
            }
            // Registration races across nodes. Take a per-name try lock before inserting, so a
            // losing node also returns Busy while the first-ever batch is still being processed.
            if !consumer_lock(&mut tx, consumer).await? {
                return Ok(OutboxReplay::Busy);
            }
            register(&mut tx, consumer).await?;
            let cursor = current(&mut tx, consumer).await?;
            let (events, next) = read(&mut tx, cursor, limit).await?;
            if events.is_empty() {
                return Ok(OutboxReplay::Idle);
            }
            let count = events.len();
            apply(&mut tx, events).await?;
            set_cursor(&mut tx, consumer, next).await?;
            Ok(OutboxReplay::Delivered {
                count,
                cursor: next,
            })
        }
        .await;
        finish(tx, work).await
    }

    /// Enable or release the owner retention hold. Replay can continue while held.
    pub async fn hold_outbox_retention(&self, held: bool) -> Result<()> {
        let mut tx = self.outbox_transaction().await?;
        let work = async {
            require_installed(&mut tx).await?;
            require_maintenance_lock(&mut tx).await?;
            tx.execute(
                "UPDATE erp_event.ir_outbox_retention SET held = $1 WHERE singleton",
                &[&held],
            )
            .await?;
            Ok(())
        }
        .await;
        finish(tx, work).await
    }

    /// Rewind an existing consumer during a retention hold. The target must be a retained event
    /// or the retention floor. Replayed effects after the target may be delivered again.
    pub async fn restore_outbox_cursor(&self, consumer: &str, target: OutboxCursor) -> Result<()> {
        check_name(consumer)?;
        let mut tx = self.outbox_transaction().await?;
        let work = async {
            require_installed(&mut tx).await?;
            require_maintenance_lock(&mut tx).await?;
            let (floor, held) = state(&mut tx).await?;
            if !held || target < floor || target > current(&mut tx, consumer).await? {
                return Err(Error::validation("outbox: restore requires a retention hold and a cursor between the retained floor and the current acknowledgement"));
            }
            if target != floor && tx.query(
                "SELECT 1 FROM erp_event.ir_event_outbox WHERE (xid,id)=($1::text::pg_catalog.xid8,$2) LIMIT 1",
                &[&target.xid.to_string(), &target.id],
            ).await?.is_empty() {
                return Err(Error::validation("outbox: restore target does not identify a retained event"));
            }
            set_cursor(&mut tx, consumer, target).await
        }.await;
        finish(tx, work).await
    }

    /// Remove up to `limit` events in the consumed prefix older than `days`. With no consumers,
    /// nothing is retired. A hold blocks deletion, and a new consumer starts at the retained floor.
    pub async fn retain_outbox(&self, days: u32, limit: u32) -> Result<OutboxRetention> {
        check_limit(limit)?;
        let days = i32::try_from(days)
            .ok()
            .filter(|days| (1..=36500).contains(days))
            .ok_or_else(|| {
                Error::validation("outbox: retention days must be between 1 and 36500")
            })?;
        let mut tx = self.outbox_transaction().await?;
        let work = async {
            if !prepare(&mut tx).await? {
                return Ok(OutboxRetention::NotInstalled);
            }
            if !delivery_lock(&mut tx, false).await? {
                return Ok(OutboxRetention::Busy);
            }
            let (old_floor, held) = state(&mut tx).await?;
            if held {
                return Ok(OutboxRetention::Held);
            }
            let rows = tx.query(RETIRE, &[&days, &i64::from(limit), &old_floor.xid.to_string(), &old_floor.id]).await?;
            let Some(row) = rows.first() else {
                return Ok(OutboxRetention::Purged { count: 0, floor: old_floor });
            };
            let floor = decode_cursor(row)?;
            tx.execute(
                "UPDATE erp_event.ir_outbox_retention SET floor_xid = $1::text::pg_catalog.xid8, floor_id = $2 WHERE singleton",
                &[&floor.xid.to_string(), &floor.id],
            ).await?;
            Ok(OutboxRetention::Purged { count: column(row, 2)?, floor })
        }.await;
        finish(tx, work).await
    }

    async fn outbox_transaction(&self) -> Result<Transaction> {
        let tx = self
            .begin_opts(
                BeginOptions::new(SUPERUSER_ID)
                    .isolation(Isolation::ReadCommitted)
                    .on_primary(),
            )
            .await?;
        tx.require_system_full("owner outbox delivery")?;
        Ok(tx)
    }
}

const READ: &str = "\
WITH prefix AS MATERIALIZED (
    SELECT e.* FROM erp_event.ir_event_outbox e
    WHERE (e.xid,e.id) > ($1::text::pg_catalog.xid8,$2)
      AND e.xid < pg_catalog.pg_snapshot_xmin(pg_catalog.pg_current_snapshot())
    ORDER BY e.xid,e.id LIMIT ($3::bigint + 1)
), payloads AS (
    SELECT e.xid,e.id,e.schema_version,pg_catalog.row_to_json(p)::text AS payload
    FROM prefix e CROSS JOIN LATERAL (
        SELECT e.event_id,e.kind,e.model,e.record_ids,e.field_names,e.cascade_models
    ) p
) SELECT e.xid::text,e.id,e.schema_version,
         CASE WHEN pg_catalog.octet_length(e.payload) <= $4::int4 THEN e.payload END
  FROM payloads e ORDER BY e.xid,e.id";

// Age is not monotone in xid order (create_date uses transaction start), so remove only the
// contiguous old prefix. Looking past a young event could push the floor over retained events.
const RETIRE: &str = "\
WITH low AS (
    SELECT last_xid,last_id FROM erp_event.ir_outbox_consumer ORDER BY last_xid,last_id LIMIT 1
), candidates AS MATERIALIZED (
    SELECT e.tableoid,e.ctid,e.xid,e.id,e.create_date
    FROM erp_event.ir_event_outbox e, low
    WHERE (e.xid,e.id) > ($3::text::pg_catalog.xid8,$4)
      AND (e.xid,e.id) <= (low.last_xid,low.last_id)
    ORDER BY e.xid,e.id LIMIT $2
), bounded AS (
    SELECT c.*, bool_or(c.create_date >= pg_catalog.statement_timestamp() - pg_catalog.make_interval(days => $1))
      OVER (ORDER BY c.xid,c.id ROWS UNBOUNDED PRECEDING) AS keep
    FROM candidates c
), deleted AS (
    DELETE FROM erp_event.ir_event_outbox e USING bounded c
    WHERE NOT c.keep AND e.tableoid = c.tableoid AND e.ctid = c.ctid
    RETURNING e.xid,e.id
) SELECT d.xid::text,d.id,count(*) OVER () FROM deleted d ORDER BY d.xid DESC,d.id DESC LIMIT 1";

async fn prepare(tx: &mut Transaction) -> Result<bool> {
    let setup = "SELECT pg_catalog.to_regclass('erp_event.ir_event_outbox') IS NOT NULL,
        pg_catalog.to_regclass('erp_event.ir_outbox_retention') IS NOT NULL";
    let rows = tx.query(setup, &[]).await?;
    if !column::<bool>(&rows[0], 0)? {
        return Ok(false);
    }
    if !column::<bool>(&rows[0], 1)? {
        tx.query(
            "SELECT pg_catalog.pg_advisory_xact_lock(pg_catalog.hashtextextended($1,0))",
            &[&LOCK],
        )
        .await?;
        if !column::<bool>(&tx.query(setup, &[]).await?[0], 1)? {
            tx.batch(TABLES).await?;
        }
    }
    Ok(true)
}

async fn delivery_lock(tx: &mut Transaction, shared: bool) -> Result<bool> {
    let sql = if shared {
        "SELECT pg_catalog.pg_try_advisory_xact_lock_shared(pg_catalog.hashtextextended($1,0))"
    } else {
        "SELECT pg_catalog.pg_try_advisory_xact_lock(pg_catalog.hashtextextended($1,0))"
    };
    column(&tx.query(sql, &[&LOCK]).await?[0], 0)
}

async fn consumer_lock(tx: &mut Transaction, name: &str) -> Result<bool> {
    column(&tx.query("SELECT pg_catalog.pg_try_advisory_xact_lock(pg_catalog.hashtext($1), pg_catalog.hashtext($2))", &[&LOCK, &name]).await?[0], 0)
}

async fn require_maintenance_lock(tx: &mut Transaction) -> Result<()> {
    if !delivery_lock(tx, false).await? {
        return Err(Error::new(
            erp_fields::ErrorKind::Concurrency,
            "outbox: delivery or retention in progress",
        ));
    }
    Ok(())
}

async fn require_installed(tx: &mut Transaction) -> Result<()> {
    if !prepare(tx).await? {
        return Err(Error::validation("outbox: event table is not installed"));
    }
    Ok(())
}

async fn register(tx: &mut Transaction, name: &str) -> Result<()> {
    tx.execute(
        "INSERT INTO erp_event.ir_outbox_consumer (name,last_xid,last_id)
        SELECT $1,floor_xid,floor_id FROM erp_event.ir_outbox_retention WHERE singleton
        ON CONFLICT (name) DO NOTHING",
        &[&name],
    )
    .await?;
    Ok(())
}

async fn current(tx: &mut Transaction, name: &str) -> Result<OutboxCursor> {
    let rows = tx.query("SELECT last_xid::text,last_id FROM erp_event.ir_outbox_consumer WHERE name=$1 FOR UPDATE", &[&name]).await?;
    decode_cursor(
        rows.first()
            .ok_or_else(|| Error::validation("outbox: unknown consumer"))?,
    )
}

async fn state(tx: &mut Transaction) -> Result<(OutboxCursor, bool)> {
    let rows = tx.query("SELECT floor_xid::text,floor_id,held FROM erp_event.ir_outbox_retention WHERE singleton", &[]).await?;
    let row = rows
        .first()
        .ok_or_else(|| Error::internal("outbox: missing retention state"))?;
    Ok((decode_cursor(row)?, column(row, 2)?))
}

async fn set_cursor(tx: &mut Transaction, consumer: &str, cursor: OutboxCursor) -> Result<()> {
    let count = tx.execute("UPDATE erp_event.ir_outbox_consumer SET last_xid=$2::text::pg_catalog.xid8, last_id=$3,
        updated_at=pg_catalog.clock_timestamp() WHERE name=$1", &[&consumer, &cursor.xid.to_string(), &cursor.id]).await?;
    if count != 1 {
        return Err(Error::internal(
            "outbox: cursor update did not affect exactly one consumer",
        ));
    }
    Ok(())
}

async fn read(
    tx: &mut Transaction,
    cursor: OutboxCursor,
    limit: u32,
) -> Result<(Vec<OutboxEvent>, OutboxCursor)> {
    let limit = i64::from(limit);
    let max_bytes = super::MAX_ENVELOPE_BYTES as i32;
    let rows = tx
        .query(
            READ,
            &[&cursor.xid.to_string(), &cursor.id, &limit, &max_bytes],
        )
        .await?;
    let mut next = cursor;
    let mut events = Vec::with_capacity(rows.len());
    for (index, row) in rows.iter().enumerate() {
        let position = decode_cursor(row)?;
        if position <= next {
            return Err(Error::validation("outbox: duplicate cursor"));
        }
        if index as i64 == limit {
            break;
        }
        if column::<i16>(row, 2)? != super::SCHEMA_VERSION {
            return Err(Error::validation("outbox: unsupported envelope version"));
        }
        let payload = column::<Option<&str>>(row, 3)?.ok_or_else(|| {
            Error::validation("outbox: stored envelope exceeds replay size limit")
        })?;
        let event: OutboxEvent = serde_json::from_str(payload).map_err(|error| {
            Error::validation("outbox: stored envelope could not be decoded").with_source(error)
        })?;
        if event.record_ids.is_empty()
            || event.record_ids.len() > super::MAX_ID_ENTRIES
            || event.record_ids.iter().any(|id| *id < 1)
        {
            return Err(Error::validation(
                "outbox: stored envelope has invalid record ids",
            ));
        }
        events.push(event);
        next = position;
    }
    Ok((events, next))
}

fn column<'a, T: tokio_postgres::types::FromSql<'a>>(
    row: &'a tokio_postgres::Row,
    index: usize,
) -> Result<T> {
    row.try_get(index).map_err(|error| {
        Error::db("outbox: could not decode the stored delivery state").with_source(error)
    })
}

fn decode_cursor(row: &tokio_postgres::Row) -> Result<OutboxCursor> {
    let xid = parse_xid(column::<&str>(row, 0)?)?;
    Ok(OutboxCursor {
        xid,
        id: column(row, 1)?,
    })
}

fn parse_xid(value: &str) -> Result<u64> {
    value
        .parse()
        .map_err(|_| Error::internal("outbox: invalid xid"))
}

fn check_name(name: &str) -> Result<()> {
    if name.is_empty()
        || name.len() > 128
        || !name
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b"_.-".contains(&b))
    {
        return Err(Error::validation(
            "outbox: consumer name must be 1 to 128 lowercase ASCII letters, digits, dots, hyphens or underscores",
        ));
    }
    Ok(())
}

fn check_limit(limit: u32) -> Result<()> {
    if !(1..=MAX_BATCH).contains(&limit) {
        return Err(Error::validation(format!(
            "outbox: batch limit must be between 1 and {MAX_BATCH}"
        )));
    }
    Ok(())
}

async fn finish<T>(mut tx: Transaction, work: Result<T>) -> Result<T> {
    match work {
        Ok(value) => {
            tx.commit().await?;
            Ok(value)
        }
        Err(error) => match tx.rollback().await {
            Ok(()) => Err(error),
            Err(rollback) => Err(error.and_also(&rollback)),
        },
    }
}
