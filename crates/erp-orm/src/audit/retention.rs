// SPDX-License-Identifier: MIT OR Apache-2.0
//! Owner audit partition archival. Evidence and its manifest commit together.
use erp_fields::{Date, Error, Result};
use sha2::{Digest, Sha256};
use std::time::Duration;
use tokio_postgres::{Row, types::FromSql};

use super::seal::{VerifyOutcome, VerifyScope, WitnessEvidence};
use crate::{BeginOptions, OperationIdentity, Orm, SUPERUSER_ID, Timeouts, Transaction};

const LIVE: &str = "erp_event.ir_audit_entry";
const COLD: &str = "erp_audit_archive.entry";
const EVIDENCE: &str = "erp_audit_archive.evidence";
const MAX_ROWS: i64 = 1_000_000;

const INSTALL: &str = "
CREATE TABLE IF NOT EXISTS erp_audit_archive.entry
    (LIKE erp_event.ir_audit_entry INCLUDING ALL) PARTITION BY RANGE(create_date);
CREATE TABLE IF NOT EXISTS erp_audit_archive.hold (
    day date PRIMARY KEY, placed_at timestamptz NOT NULL DEFAULT now(), placed_by name NOT NULL DEFAULT current_user
);
CREATE TABLE IF NOT EXISTS erp_audit_archive.manifest (
    day date PRIMARY KEY, cutoff date NOT NULL, archive_relation text NOT NULL,
    row_count bigint NOT NULL CHECK (row_count >= 0),
    sha256 bytea NOT NULL CHECK (octet_length(sha256) = 32),
    archived_by name NOT NULL DEFAULT current_user, archived_at timestamptz NOT NULL DEFAULT now(),
    restored_at timestamptz
);
CREATE TABLE IF NOT EXISTS erp_audit_archive.schedule (
    singleton boolean PRIMARY KEY CHECK(singleton), day date NOT NULL
);
CREATE OR REPLACE VIEW erp_audit_archive.evidence AS
    SELECT a.*, a.tableoid AS source_relation, a.ctid AS source_tuple FROM erp_event.ir_audit_entry a
    UNION ALL
    SELECT a.*, a.tableoid AS source_relation, a.ctid AS source_tuple FROM erp_audit_archive.entry a;
REVOKE ALL ON ALL TABLES IN SCHEMA erp_audit_archive FROM PUBLIC;";

/// Result of one bounded owner retention pass.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum AuditRetention {
    /// No age policy is configured.
    KeepAll,
    /// Another owner or today's scheduled execution already acquired the lease.
    Busy,
    /// No whole partition older than the cutoff is eligible.
    NothingEligible,
    /// Evidence transferred to owner-only storage with its manifest.
    Archived {
        /// The archived UTC day.
        day: Date,
        /// The number of rows covered by the manifest.
        rows: i64,
    },
}

fn cell<'r, T: FromSql<'r>>(row: &'r Row, i: usize) -> Result<T> {
    row.try_get(i)
        .map_err(|error| Error::db("invalid audit archive metadata").with_source(error))
}

fn dates(day: Date) -> Result<(String, String, String)> {
    let (year, month, date) = day.ymd();
    if !(1..=9998).contains(&year) {
        return Err(Error::validation(
            "Audit partition day is outside the supported range",
        ));
    }
    Ok((
        format!("ir_audit_entry_p{year:04}{month:02}{date:02}"),
        day.to_string(),
        Date::from_days(day.days() + 1).to_string(),
    ))
}

async fn singleton(tx: &mut Transaction) -> Result<bool> {
    tx.require_system_full("audit partition retention")?;
    let rows = tx
        .query(
            "SELECT pg_try_advisory_xact_lock(hashtextextended('core.audit.retention',0))",
            &[],
        )
        .await?;
    cell(&rows[0], 0)
}

async fn storage(tx: &mut Transaction) -> Result<()> {
    let parent = tx
        .query(
            "SELECT relowner = (current_user::regrole)::oid AND relkind='p'
         FROM pg_class WHERE oid=to_regclass('erp_event.ir_audit_entry')",
            &[],
        )
        .await?;
    if parent.first().map(|row| cell::<bool>(row, 0)).transpose()? != Some(true) {
        return Err(Error::access(
            "Audit retention requires the audit table owner",
        ));
    }
    tx.batch("CREATE SCHEMA IF NOT EXISTS erp_audit_archive")
        .await?;
    let rows = tx.query(
        "SELECT nspowner = (current_user::regrole)::oid AND NOT EXISTS(
            SELECT FROM aclexplode(COALESCE(nspacl,acldefault('n',nspowner))) p WHERE p.grantee<>nspowner)
         FROM pg_namespace WHERE nspname='erp_audit_archive'",
        &[],
    ).await?;
    if !cell::<bool>(&rows[0], 0)? {
        return Err(Error::access(
            "Audit archive schema must be private and owned by the maintenance role",
        ));
    }
    tx.batch(INSTALL).await
}

/// Shared owner evidence source for chain verification and new seals.
pub(super) async fn evidence_source(tx: &mut Transaction) -> Result<&'static str> {
    let rows = tx
        .query(
            "SELECT to_regnamespace('erp_audit_archive') IS NOT NULL,
                to_regclass('erp_audit_archive.evidence') IS NOT NULL",
            &[],
        )
        .await?;
    match (cell(&rows[0], 0)?, cell(&rows[0], 1)?) {
        (true, true) => Ok(EVIDENCE),
        (false, false) => Ok(
            "(SELECT a.*,a.tableoid AS source_relation,a.ctid AS source_tuple FROM erp_event.ir_audit_entry a)",
        ),
        _ => Err(Error::programming("Audit archive evidence view is missing")),
    }
}

async fn eligible(tx: &mut Transaction, days: u16) -> Result<Option<(Date, Date)>> {
    let days = i32::from(days);
    let rows = tx
        .query(
            "WITH partitions AS (
            SELECT to_date(right(c.relname::text,8),'YYYYMMDD') AS day
            FROM pg_inherits i JOIN pg_class c ON i.inhrelid=c.oid
            JOIN pg_namespace n ON n.oid=c.relnamespace
            WHERE i.inhparent='erp_event.ir_audit_entry'::regclass AND n.nspname='erp_event'
              AND c.relname ~ '^ir_audit_entry_p[0-9]{8}$'
         ), policy AS (SELECT (now() AT TIME ZONE 'UTC')::date - $1::int AS cutoff)
         SELECT partitions.day, policy.cutoff FROM partitions CROSS JOIN policy
         WHERE partitions.day < policy.cutoff
           AND NOT EXISTS(SELECT FROM erp_audit_archive.hold h WHERE h.day=partitions.day)
         ORDER BY partitions.day LIMIT 1",
            &[&days],
        )
        .await?;
    rows.first()
        .map(|row| Ok((cell(row, 0)?, cell(row, 1)?)))
        .transpose()
}

async fn proof(tx: &mut Transaction, partition: &str, witness: &WitnessEvidence) -> Result<()> {
    let rows = tx
        .query(
            "SELECT to_regclass('erp_event.ir_audit_chain') IS NOT NULL",
            &[],
        )
        .await?;
    if !cell::<bool>(&rows[0], 0)? {
        return if matches!(witness, WitnessEvidence::NotConfigured) {
            Ok(())
        } else {
            Err(Error::validation(
                "Configured audit witness requires an initialized chain",
            ))
        };
    }
    // An installed but uninitialized chain has no integrity policy until an epoch is created.
    let rows = tx
        .query("SELECT EXISTS(SELECT FROM erp_event.ir_audit_chain)", &[])
        .await?;
    if !cell::<bool>(&rows[0], 0)? && matches!(witness, WitnessEvidence::NotConfigured) {
        return Ok(());
    }
    let WitnessEvidence::Inventory(anchors) = witness else {
        return Err(Error::validation(
            "Audit partition requires published chain anchors",
        ));
    };
    let VerifyOutcome::Verified(report) =
        tx.verify_audit_chain(&VerifyScope::full(), witness).await?
    else {
        return Err(Error::validation(
            "Audit chain verification failed before moving a partition",
        ));
    };
    if report.epochs() == 0 {
        return Err(Error::validation(
            "Audit partition requires published chain anchors",
        ));
    }
    let (epochs, batches): (Vec<_>, Vec<_>) = anchors
        .anchors()
        .iter()
        .map(|anchor| {
            (
                anchor.identity().epoch() as i64,
                anchor.identity().batch_no() as i64,
            )
        })
        .unzip();
    let rows = tx.query(&format!(
        "WITH covered AS MATERIALIZED (
            SELECT s.lower_xid,s.lower_id,s.upper_xid,s.upper_id FROM erp_event.ir_audit_seal s
            JOIN unnest($1::int8[],$2::int8[]) AS anchored(epoch,batch)
            ON s.chain_epoch=anchored.epoch AND s.batch_no=anchored.batch
        ) SELECT NOT EXISTS (SELECT FROM {partition} a WHERE NOT EXISTS(SELECT FROM covered s
            WHERE (s.lower_xid,s.lower_id)<(a.xid,a.id) AND (s.upper_xid,s.upper_id)>=(a.xid,a.id)))"
    ), &[&epochs,&batches]).await?;
    if !cell::<bool>(&rows[0], 0)? {
        return Err(Error::validation(
            "Audit partition contains rows without published chain coverage",
        ));
    }
    Ok(())
}

async fn fingerprint(tx: &mut Transaction, partition: &str) -> Result<(i64, [u8; 32])> {
    let mut count = 0_i64;
    let mut digest = Sha256::new();
    tx.query_each(
        &format!(
            "SELECT id, sha256(convert_to(row_to_json(a)::text, 'UTF8'))
            FROM {partition} a ORDER BY id,create_date LIMIT {}",
            MAX_ROWS + 1
        ),
        &[],
        |row| {
            count += 1;
            if count > MAX_ROWS {
                return Err(Error::validation(
                    "Audit partition exceeds the per-pass archive row limit",
                ));
            }
            digest.update(row.try_get::<i64>(0)?.to_be_bytes());
            digest.update(row.try_get::<&[u8]>(1)?);
            Ok(())
        },
    )
    .await?;
    Ok((count, digest.finalize().into()))
}

async fn begin(orm: &Orm) -> Result<Transaction> {
    orm.check_audit_store().await?;
    orm.begin_opts(
        BeginOptions::new(SUPERUSER_ID)
            .on_primary()
            .operation_identity(OperationIdentity::generate()?)
            .timeouts(Timeouts::new(
                Duration::from_millis(500),
                Duration::from_secs(20),
                Duration::from_secs(40),
            )?),
    )
    .await
}

fn audit(tx: &mut Transaction, method: &'static str, scheduled: bool) -> Result<()> {
    use super::{AuditCall, AuditOrigin, TargetIds};

    tx.audit_call(AuditCall::new(
        None,
        method.into(),
        crate::rpc::Effect::Write,
        TargetIds::none(),
        None,
    )?)?;
    tx.audit_origin(if scheduled {
        AuditOrigin::Job
    } else {
        AuditOrigin::Cli
    })
}

impl Orm {
    /// Archive one eligible UTC partition using the owner connection. No configured age keeps
    /// everything attached. Scheduled executions claim at most one pass per UTC day.
    /// # Errors
    /// Invalid age, catalog drift, insufficient owner authority or missing published anchors.
    pub async fn retain_audit(
        &self,
        days: Option<u16>,
        scheduled: bool,
        witness: &WitnessEvidence,
    ) -> Result<AuditRetention> {
        let Some(days) = days else {
            return Ok(AuditRetention::KeepAll);
        };
        if !(1..=3650).contains(&days) {
            return Err(Error::validation(
                "Audit retention days must be from 1 to 3650",
            ));
        }
        let mut tx = begin(self).await?;
        let result = async {
            if !singleton(&mut tx).await? { return Ok(AuditRetention::Busy); }
            tx.check_event_partitions(&super::AUDIT_TABLE).await?;
            storage(&mut tx).await?;
            if scheduled && tx.execute(
                "INSERT INTO erp_audit_archive.schedule VALUES (true,(now() AT TIME ZONE 'UTC')::date)
                 ON CONFLICT(singleton) DO UPDATE SET day=EXCLUDED.day WHERE schedule.day < EXCLUDED.day",
                &[],
            ).await? == 0 { return Ok(AuditRetention::Busy); }
            let Some((day, cutoff)) = eligible(&mut tx, days).await? else {
                return Ok(AuditRetention::NothingEligible);
            };
            let (name, first, next) = dates(day)?;
            let hot = format!("erp_event.{name}");
            let cold = format!("erp_audit_archive.{name}");
            tx.batch("SET LOCAL TIME ZONE 'UTC'; SET LOCAL DateStyle = 'ISO, YMD'; SET LOCAL bytea_output = 'hex'").await?;
            tx.batch(&format!("LOCK TABLE {hot} IN ACCESS EXCLUSIVE MODE")).await?;
            proof(&mut tx, &hot, witness).await?;
            let (rows, digest) = fingerprint(&mut tx, &hot).await?;
            move_day(&mut tx, LIVE, &hot, "erp_audit_archive", COLD, &cold, &first, &next).await?;
            if tx.execute(
                "INSERT INTO erp_audit_archive.manifest (day,cutoff,archive_relation,row_count,sha256)
                 VALUES($1,$2,$3,$4,$5)
                 ON CONFLICT(day) DO UPDATE SET cutoff=EXCLUDED.cutoff, archive_relation=EXCLUDED.archive_relation,
                    row_count=EXCLUDED.row_count, sha256=EXCLUDED.sha256, archived_by=current_user,
                    archived_at=now(), restored_at=NULL WHERE manifest.restored_at IS NOT NULL",
                &[&day, &cutoff, &cold, &rows, &digest.as_slice()],
            ).await? != 1 { return Err(Error::programming("Live audit archive manifest already exists")); }
            audit(&mut tx, "audit.retention.archive", scheduled)?;
            Ok(AuditRetention::Archived { day, rows })
        }.await;
        finish(&mut tx, result).await
    }

    /// Set an owner-only legal hold for a UTC day. The return value indicates a change.
    /// # Errors
    /// Invalid day, an active competing owner operation, or insufficient privileges.
    pub async fn set_audit_hold(&self, day: Date, held: bool) -> Result<bool> {
        dates(day)?;
        let mut tx = begin(self).await?;
        let result = async {
            if !singleton(&mut tx).await? {
                return Err(Error::validation("Audit retention is busy"));
            }
            storage(&mut tx).await?;
            let statement = if held {
                "INSERT INTO erp_audit_archive.hold(day) VALUES($1) ON CONFLICT DO NOTHING"
            } else {
                "DELETE FROM erp_audit_archive.hold WHERE day=$1"
            };
            let changed = tx.execute(statement, &[&day]).await? != 0;
            if changed {
                audit(
                    &mut tx,
                    if held {
                        "audit.hold.add"
                    } else {
                        "audit.hold.remove"
                    },
                    false,
                )?;
            }
            Ok(changed)
        }
        .await;
        finish(&mut tx, result).await
    }

    /// Check the stored fingerprint, restore a partition to live storage and place a hold.
    /// # Errors
    /// Lost or changed archive evidence, active competing operation, or insufficient privileges.
    pub async fn restore_audit_partition(
        &self,
        day: Date,
        witness: &WitnessEvidence,
    ) -> Result<i64> {
        let (name, first, next) = dates(day)?;
        let mut tx = begin(self).await?;
        let result = async {
            if !singleton(&mut tx).await? { return Err(Error::validation("Audit retention is busy")); }
            storage(&mut tx).await?;
            let cold = format!("erp_audit_archive.{name}");
            let hot = format!("erp_event.{name}");
            let rows = tx.query(
                "SELECT row_count,sha256 FROM erp_audit_archive.manifest WHERE day=$1 AND archive_relation=$2 AND restored_at IS NULL",
                &[&day,&cold],
            ).await?;
            let row = rows.first().ok_or_else(|| Error::validation("Audit day has no live archive manifest"))?;
            let expected = (cell::<i64>(row,0)?, cell::<Vec<u8>>(row,1)?);
            tx.batch("SET LOCAL TIME ZONE 'UTC'; SET LOCAL DateStyle = 'ISO, YMD'; SET LOCAL bytea_output = 'hex'").await?;
            tx.batch(&format!("LOCK TABLE {cold} IN ACCESS EXCLUSIVE MODE")).await?;
            let (count, digest) = fingerprint(&mut tx, &cold).await?;
            if count != expected.0 || digest.as_slice() != expected.1 {
                return Err(Error::validation("Audit archive does not match its manifest"));
            }
            proof(&mut tx, &cold, witness).await?;
            move_day(&mut tx, COLD, &cold, "erp_event", LIVE, &hot, &first, &next).await?;
            tx.execute("UPDATE erp_audit_archive.manifest SET restored_at=now() WHERE day=$1", &[&day]).await?;
            tx.execute("INSERT INTO erp_audit_archive.hold(day) VALUES($1) ON CONFLICT DO NOTHING", &[&day]).await?;
            audit(&mut tx, "audit.retention.restore", false)?;
            Ok(count)
        }.await;
        finish(&mut tx, result).await
    }
}

#[allow(clippy::too_many_arguments)]
async fn move_day(
    tx: &mut Transaction,
    source: &str,
    child: &str,
    target_schema: &str,
    destination: &str,
    new_child: &str,
    first: &str,
    next: &str,
) -> Result<()> {
    tx.batch(&format!(
        "ALTER TABLE {source} DETACH PARTITION {child};
        ALTER TABLE {child} SET SCHEMA {target_schema};
        ALTER TABLE {destination} ATTACH PARTITION {new_child}
        FOR VALUES FROM ('{first} 00:00:00+00') TO ('{next} 00:00:00+00')"
    ))
    .await
}

async fn finish<T>(tx: &mut Transaction, result: Result<T>) -> Result<T> {
    match result {
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
