// SPDX-License-Identifier: MIT OR Apache-2.0
use super::{
    decode, integrity, limit,
    types::{AuditQuery, EntryKey, MAX_BYTES},
    unavailable,
};
use deadpool_postgres::{Object, Pool};
use erp_fields::Result;
use serde_json::{Value, json};
use std::sync::LazyLock;
use tokio_postgres::Row;

// Each guarded value is checked on the server before its bytes enter the driver's buffers.
// Hidden payloads are deliberately absent. This query is fixed SQL; only values are parameters.
const SIZE: &str = "octet_length(target_ids::text)::bigint + coalesce(octet_length(model)::bigint,0) + coalesce(octet_length(method)::bigint,0) + coalesce(octet_length(effect)::bigint,0) + octet_length(entry_kind)::bigint + octet_length(outcome)::bigint + 128";
const COLUMNS:&str="entry_id::text, to_char(create_date AT TIME ZONE 'UTC','YYYY-MM-DD\"T\"HH24:MI:SS.US\"Z\"'), schema_version,
 CASE WHEN octet_length(entry_kind)<=32 THEN entry_kind END,
 CASE WHEN octet_length(outcome)<=32 THEN outcome END,
 CASE WHEN octet_length(model)<=256 THEN model END,
 CASE WHEN octet_length(method)<=256 THEN method END,
 CASE WHEN octet_length(effect)<=32 THEN effect END,
 CASE WHEN octet_length(target_ids::text)<=1048576 AND sum({size}) OVER (ORDER BY create_date,entry_id ROWS UNBOUNDED PRECEDING)<={budget} THEN target_ids::text END,
 octet_length(target_ids::text)::bigint + coalesce(octet_length(model)::bigint,0) + coalesce(octet_length(method)::bigint,0) + coalesce(octet_length(effect)::bigint,0) + octet_length(entry_kind)::bigint + octet_length(outcome)::bigint + 128,
 (model IS NOT NULL AND octet_length(model)>256) OR (method IS NOT NULL AND octet_length(method)>256) OR (effect IS NOT NULL AND octet_length(effect)>32)";

pub(super) struct Candidate {
    pub key: EntryKey,
    pub version: i16,
    pub kind: String,
    pub outcome: String,
    pub model: Option<String>,
    pub method: Option<String>,
    pub effect: Option<String>,
    pub targets: Vec<i64>,
    pub bytes: usize,
}
impl Candidate {
    fn from_row(row: Row) -> Result<Self> {
        let get_error = |_| integrity();
        let key = EntryKey::stored(
            row.try_get::<_, &str>(0).map_err(get_error)?,
            row.try_get::<_, &str>(1).map_err(get_error)?,
        )?;
        let version: i16 = row.try_get(2).map_err(get_error)?;
        let bytes: i64 = row.try_get(9).map_err(get_error)?;
        let bytes = usize::try_from(bytes).map_err(|_| integrity())?;
        if bytes > MAX_BYTES || row.try_get::<_, bool>(10).map_err(get_error)? {
            return Err(limit());
        }
        // Unsupported schemas are concealed without interpreting their historical grammar.
        if version != 1 {
            return Ok(Self {
                key,
                version,
                kind: String::new(),
                outcome: String::new(),
                model: None,
                method: None,
                effect: None,
                targets: Vec::new(),
                bytes,
            });
        }
        let kind: Option<String> = row.try_get(3).map_err(get_error)?;
        let outcome: Option<String> = row.try_get(4).map_err(get_error)?;
        let kind = kind
            .filter(|k| matches!(k.as_str(), "change" | "attempt"))
            .ok_or_else(integrity)?;
        let outcome = outcome
            .filter(|k| {
                matches!(
                    k.as_str(),
                    "committed" | "denied" | "failed" | "replayed" | "commit_unknown" | "read_ok"
                )
            })
            .ok_or_else(integrity)?;
        let effect: Option<String> = row.try_get(7).map_err(get_error)?;
        if effect
            .as_ref()
            .is_some_and(|e| !matches!(e.as_str(), "read" | "write" | "destructive" | "external"))
        {
            return Err(integrity());
        }
        let target_text: Option<&str> = row.try_get(8).map_err(get_error)?;
        let targets = decode::targets(target_text.ok_or_else(limit)?)?;
        Ok(Self {
            key,
            version,
            kind,
            outcome,
            model: row.try_get(5).map_err(get_error)?,
            method: row.try_get(6).map_err(get_error)?,
            effect,
            targets,
            bytes,
        })
    }
    pub(super) fn project(&self, fields: &[String]) -> Result<Value> {
        let mut map = serde_json::Map::new();
        for field in fields {
            let value = match field.as_str() {
                "entry_id" => json!(self.key.id.to_string()),
                "create_date" => json!(self.key.time.to_string()),
                "entry_kind" => json!(self.kind),
                "outcome" => json!(self.outcome),
                "model" => json!(self.model),
                "method" => json!(self.method),
                "effect" => json!(self.effect),
                "target_ids" => json!(self.targets),
                _ => return Err(integrity()),
            };
            map.insert(field.clone(), value);
        }
        Ok(Value::Object(map))
    }
}

// A dropped or failed operation detaches its connection rather than returning an open or
// partially consumed transaction to the pool. A successful ROLLBACK makes recycling safe.
pub(super) struct Connection {
    conn: Option<Object>,
    active: bool,
    pub database: String,
}
impl Drop for Connection {
    fn drop(&mut self) {
        if self.active
            && let Some(c) = self.conn.take()
        {
            drop(Object::take(c));
        }
    }
}
impl Connection {
    pub async fn open(pool: &Pool) -> Result<Self> {
        let conn = pool.get().await.map_err(|_| super::unavailable())?;
        let mut this = Self {
            conn: Some(conn),
            active: true,
            database: String::new(),
        };
        this.client()?.batch_execute("BEGIN ISOLATION LEVEL REPEATABLE READ READ ONLY; SET LOCAL statement_timeout='5s'; SET LOCAL transaction_timeout='30s'").await.map_err(|_|unavailable())?;
        let r = this
            .client()?
            .query_one("SELECT current_database()", &[])
            .await
            .map_err(database_error)?;
        this.database = r.try_get(0).map_err(|_| integrity())?;
        Ok(this)
    }
    fn client(&self) -> Result<&Object> {
        self.conn.as_ref().ok_or_else(unavailable)
    }
    pub async fn finish(&mut self) -> Result<()> {
        self.client()?
            .batch_execute("ROLLBACK")
            .await
            .map_err(database_error)?;
        self.active = false;
        Ok(())
    }
    pub async fn search(
        &self,
        q: &AuditQuery,
        seek: Option<&EntryKey>,
        count: i64,
        remaining: usize,
    ) -> Result<Vec<Candidate>> {
        let from = q.from.to_string();
        let until = q.until.to_string();
        let seek_time = seek.map(|k| k.time.to_string());
        let seek_id = seek.map(|k| k.id.to_string());
        let remaining = i64::try_from(remaining).map_err(|_| limit())?;
        self.client()?
            .query(
                SEARCH_SQL.as_str(),
                &[
                    &q.model, &from, &until, &q.kind, &q.outcome, &seek_time, &seek_id, &count,
                    &remaining,
                ],
            )
            .await
            .map_err(database_error)?
            .into_iter()
            .map(Candidate::from_row)
            .collect()
    }
    pub async fn read(&self, keys: &[EntryKey], remaining: usize) -> Result<Vec<Candidate>> {
        let ids: Vec<String> = keys.iter().map(|k| k.id.to_string()).collect();
        let dates: Vec<String> = keys.iter().map(|k| k.time.to_string()).collect();
        let remaining = i64::try_from(remaining).map_err(|_| limit())?;
        self.client()?
            .query(READ_SQL.as_str(), &[&ids, &dates, &remaining])
            .await
            .map_err(database_error)?
            .into_iter()
            .map(Candidate::from_row)
            .collect()
    }
}

fn database_error(e: tokio_postgres::Error) -> erp_fields::Error {
    // The public cause is deliberately sanitized; database errors can carry row context.
    drop(e);
    unavailable()
}

static SEARCH_SQL: LazyLock<String> = LazyLock::new(|| {
    let columns = COLUMNS
        .replace("{size}", SIZE)
        .replace("{budget}", "$9::bigint");
    format!(
        "SELECT {columns} FROM (SELECT * FROM erp_event.ir_audit_entry WHERE model=$1 AND create_date >= $2::text::timestamptz AND create_date < $3::text::timestamptz AND ($4::text IS NULL OR entry_kind=$4) AND ($5::text IS NULL OR outcome=$5) AND ($6::text IS NULL OR (create_date,entry_id)>($6::text::timestamptz,$7::text::uuid)) ORDER BY create_date,entry_id LIMIT $8) AS candidates ORDER BY create_date,entry_id"
    )
});

static READ_SQL: LazyLock<String> = LazyLock::new(|| {
    let columns = COLUMNS
        .replace("{size}", SIZE)
        .replace("{budget}", "$3::bigint");
    format!(
        "SELECT {columns} FROM (SELECT * FROM erp_event.ir_audit_entry WHERE (entry_id,create_date) IN (SELECT id::uuid,date::timestamptz FROM unnest($1::text[],$2::text[]) AS wanted(id,date)) ORDER BY create_date,entry_id LIMIT 17) AS candidates ORDER BY create_date,entry_id"
    )
});
