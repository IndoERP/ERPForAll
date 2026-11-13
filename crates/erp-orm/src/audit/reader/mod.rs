// SPDX-License-Identifier: MIT OR Apache-2.0
//! Bounded current-access audit reader. This engine is not a generic event ORM escape.
//! Production credential admission and serving integration belong to/INTEGRATE.
mod types;
use erp_fields::{Error, ErrorKind};
pub use types::{AuditPage, AuditQuery, EntryKey, ReadRequest};
fn invalid(message: &str) -> Error {
    Error::validation(format!("a later release: {message}"))
}
fn integrity() -> Error {
    Error::new(
        ErrorKind::Internal,
        "a later release: invalid stored audit data",
    )
}
fn limit() -> Error {
    Error::new(
        ErrorKind::ResourceExhausted,
        "a later release: reader safety limit exceeded",
    )
}
fn denied() -> Error {
    Error::access("a later release: audit access denied")
}
mod access;
mod cursor;
mod decode;
mod store;
use crate::Transaction;
use deadpool_postgres::Pool;
use erp_fields::Result;
use types::{MAX_BYTES, MAX_CANDIDATES};

fn unavailable() -> Error {
    Error::new(
        ErrorKind::Internal,
        "a later release: audit reader unavailable",
    )
}

/// Platform-owned read service. Raw credentials, connections and stored rows are private.
/// An unavailable reader never substitutes the request or owner pool.
///
/// Production construction is reserved for the admitted role composition in
/// /INTEGRATE; addons cannot construct an enabled reader.
#[non_exhaustive]
pub struct AuditReader {
    pool: Option<Pool>,
}
impl std::fmt::Debug for AuditReader {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AuditReader").finish_non_exhaustive()
    }
}
impl AuditReader {
    /// A reader without configured database authority.
    pub fn unavailable() -> Self {
        Self { pool: None }
    }
    /// Real restricted-role test fixture only. This is absent from shipped builds.
    #[cfg(feature = "test-faults")]
    pub fn for_tests(pool: Pool) -> Self {
        Self { pool: Some(pool) }
    }

    /// One bounded page, authorized with the transaction's original caller on the primary.
    /// # Errors
    /// Access for missing capability/ACL/fields, Validation for a mismatched cursor,
    /// ResourceExhausted on a safety bound, Internal on unavailable/malformed storage.
    /// Cancellation discards all buffered output and detaches the reader connection.
    pub async fn search(
        &self,
        tx: &mut Transaction,
        query: &AuditQuery,
        output_budget: usize,
    ) -> Result<AuditPage> {
        tokio::time::timeout(
            std::time::Duration::from_secs(30),
            self.page(tx, query, output_budget),
        )
        .await
        .map_err(|_| limit())?
    }
    /// Export is exactly one authorized page with the search continuation contract.
    /// # Errors
    /// The same failures as [`Self::search`], with no partial export on error.
    pub async fn export(
        &self,
        tx: &mut Transaction,
        query: &AuditQuery,
        output_budget: usize,
    ) -> Result<AuditPage> {
        self.search(tx, query, output_budget).await
    }
    /// Read bounded composite keys in request order, deduplicated by [`ReadRequest`].
    /// Missing and inaccessible entries are both omitted.
    /// # Errors
    /// The same access, integrity, availability and safety failures as [`Self::search`].
    pub async fn read(
        &self,
        tx: &mut Transaction,
        request: &ReadRequest,
        output_budget: usize,
    ) -> Result<AuditPage> {
        tokio::time::timeout(
            std::time::Duration::from_secs(30),
            self.direct(tx, request, output_budget),
        )
        .await
        .map_err(|_| limit())?
    }
    async fn connect(&self, tx: &mut Transaction) -> Result<store::Connection> {
        let pool = self.pool.as_ref().ok_or_else(unavailable)?;
        let connection = store::Connection::open(pool).await?;
        let row = tx
            .query("SELECT current_database()", &[])
            .await
            .map_err(access::sanitize)?;
        let [row] = row.as_slice() else {
            return Err(unavailable());
        };
        let database: &str = row.try_get(0).map_err(|_| unavailable())?;
        if database != connection.database {
            return Err(unavailable());
        }
        Ok(connection)
    }
    async fn page(
        &self,
        tx: &mut Transaction,
        q: &AuditQuery,
        output_budget: usize,
    ) -> Result<AuditPage> {
        let env = access::caller(tx, &q.fields)
            .await
            .map_err(access::sanitize)?;
        if !access::model(tx, &env, &q.model)
            .await
            .map_err(access::sanitize)?
        {
            return Err(denied());
        }
        let mut connection = self.connect(tx).await?;
        let binding = cursor::binding(tx, &env, q, &connection.database).await?;
        let mut seek = q
            .continuation
            .as_deref()
            .map(|c| cursor::decode(c, &binding))
            .transpose()?;
        if seek
            .as_ref()
            .is_some_and(|k| k.time < q.from || k.time >= q.until)
        {
            return Err(invalid("invalid continuation"));
        }
        let mut page = AuditPage {
            rows: Vec::new(),
            continuation: None,
        };
        let mut candidates = 0usize;
        let mut bytes = 0usize;
        loop {
            tx.check_not_cancelled()?;
            let count =
                i64::try_from((MAX_CANDIDATES - candidates + 1).min(16)).map_err(|_| limit())?;
            let rows = connection
                .search(q, seek.as_ref(), count, MAX_BYTES - bytes)
                .await?;
            if rows.is_empty() {
                break;
            }
            candidates = candidates.checked_add(rows.len()).ok_or_else(limit)?;
            if candidates > MAX_CANDIDATES {
                return Err(limit());
            }
            for row in &rows {
                bytes = bytes
                    .checked_add(row.bytes)
                    .filter(|n| *n <= MAX_BYTES)
                    .ok_or_else(limit)?;
            }
            let eligible = access::eligible(tx, &env, &rows)
                .await
                .map_err(access::sanitize)?;
            for (row, allowed) in rows.iter().zip(eligible) {
                seek = Some(row.key.clone());
                if allowed {
                    page.rows.push(row.project(&q.fields)?);
                    if page.rows.len() == q.limit {
                        page.continuation = Some(cursor::encode(&row.key, &binding));
                        check_output(&page, output_budget)?;
                        connection.finish().await?;
                        return Ok(page);
                    }
                    check_output(&page, output_budget)?;
                }
            }
        }
        check_output(&page, output_budget)?;
        connection.finish().await?;
        Ok(page)
    }
    async fn direct(
        &self,
        tx: &mut Transaction,
        request: &ReadRequest,
        output_budget: usize,
    ) -> Result<AuditPage> {
        let env = access::caller(tx, &request.fields)
            .await
            .map_err(access::sanitize)?;
        let mut connection = self.connect(tx).await?;
        let mut found = std::collections::HashMap::new();
        let mut bytes = 0usize;
        for keys in request.keys.chunks(16) {
            tx.check_not_cancelled()?;
            let rows = connection.read(keys, MAX_BYTES - bytes).await?;
            if rows.len() > keys.len() {
                return Err(integrity());
            }
            for row in &rows {
                bytes = bytes
                    .checked_add(row.bytes)
                    .filter(|n| *n <= MAX_BYTES)
                    .ok_or_else(limit)?;
            }
            let eligible = access::eligible(tx, &env, &rows)
                .await
                .map_err(access::sanitize)?;
            for (row, allowed) in rows.iter().zip(eligible) {
                if allowed
                    && found
                        .insert(row.key.clone(), row.project(&request.fields)?)
                        .is_some()
                {
                    return Err(integrity());
                }
            }
            // Bound accumulated output before processing another batch.
            let probe = AuditPage {
                rows: found.values().cloned().collect(),
                continuation: None,
            };
            check_output(&probe, output_budget)?;
        }
        let mut page = AuditPage {
            rows: Vec::new(),
            continuation: None,
        };
        for key in &request.keys {
            if let Some(row) = found.remove(key) {
                page.rows.push(row);
            }
        }
        check_output(&page, output_budget)?;
        connection.finish().await?;
        Ok(page)
    }
}
fn check_output(page: &AuditPage, budget: usize) -> Result<()> {
    if budget == 0 {
        return Err(limit());
    }
    // Includes the enclosing object, JSON escapes, separators and continuation encoding.
    // Values were constructed from bounded primitives; serialization cannot recurse unboundedly.
    let bytes = serde_json::to_vec(&page.to_json()).map_err(|_| integrity())?;
    if bytes.len() > budget.min(MAX_BYTES) {
        return Err(limit());
    }
    Ok(())
}
