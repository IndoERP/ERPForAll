// SPDX-License-Identifier: MIT OR Apache-2.0
//! Schema synchronization of encrypted fields: a field that becomes encrypted on a table that
//! already holds its clear values.
//!
//! Before the DDL, the catalog says which encrypted fields still have a `varchar` or `text`
//! column. Schema sync changes such a column to `bytea` holding the UTF-8 bytes of each value
//! (`erp_sql::ddl::alter_column_to_utf8_bytea`), and [`Transaction::seal_clear_columns`] then seals
//! those bytes in batches, in the same transaction: the commit holds either the clear column or
//! the sealed one, never a mix. On a large table this runs inside the module operation (it is not
//! a post-commit schema step yet); its duration grows with the row count.
//!
//! The reverse (an encrypted field declared plain again) is refused by schema sync before any DDL.

use erp_fields::{Error, ErrorKind, Result};
use erp_registry::ModelSpec;

use super::{Sealed, keys_for};
use crate::Transaction;

/// Rows read and written per round trip while sealing a converted column.
const SEAL_BATCH: i64 = 1_000;

/// An encrypted field whose column still holds clear text.
pub(crate) struct ClearColumn {
    table: String,
    column: &'static str,
    sealed: Sealed,
}

fn db_error(what: &'static str) -> impl Fn(tokio_postgres::Error) -> Error {
    move |error| Error::new(ErrorKind::Database, what).with_source(error)
}

impl Transaction {
    /// The encrypted fields of `specs` whose existing column is still `varchar` or `text` (read
    /// from the catalog before schema sync converts them).
    ///
    /// # Errors
    ///
    /// A database error; a `Programming` error when such a column exists and this process has no
    /// field keys (nothing is converted then).
    pub(crate) async fn clear_encrypted_columns(
        &mut self,
        specs: &[&ModelSpec],
    ) -> Result<Vec<ClearColumn>> {
        let mut out = Vec::new();
        for spec in specs {
            let fields: Vec<(&'static str, Sealed)> = spec
                .fields
                .iter()
                .filter_map(|f| Sealed::of(spec, f).map(|sealed| (f.name, sealed)))
                .collect();
            if fields.is_empty() {
                continue;
            }
            let names: Vec<&str> = fields.iter().map(|(name, _)| *name).collect();
            let rows = self
                .query(
                    "SELECT a.attname::text FROM pg_attribute a \
                     JOIN pg_type t ON t.oid = a.atttypid \
                     WHERE a.attrelid = to_regclass($1) AND a.attname = ANY($2) \
                     AND a.attnum > 0 AND NOT a.attisdropped \
                     AND t.typname IN ('varchar', 'text', 'bpchar')",
                    &[&erp_sql::quote(&spec.table), &names],
                )
                .await?;
            for row in rows {
                let name: String = row.try_get(0).map_err(db_error(
                    "could not read the catalog of an encrypted column",
                ))?;
                let Some((column, sealed)) = fields.iter().find(|(n, _)| *n == name) else {
                    continue;
                };
                // refuse before any DDL: converting without a key would strand clear bytes
                keys_for(spec.name, column)?;
                out.push(ClearColumn {
                    table: spec.table.clone(),
                    column: *column,
                    sealed: *sealed,
                });
            }
        }
        Ok(out)
    }

    /// Seal the UTF-8 bytes schema sync left in each of `columns` (now `bytea`), in batches of
    /// [`SEAL_BATCH`] rows by ascending id.
    ///
    /// # Errors
    ///
    /// A database error; a `Database` error naming the field when a value is not UTF-8.
    pub(crate) async fn seal_clear_columns(&mut self, columns: &[ClearColumn]) -> Result<()> {
        for clear in columns {
            let table = erp_sql::quote(&clear.table);
            let column = erp_sql::quote(clear.column);
            let select = format!(
                "SELECT id, {column} FROM {table} WHERE {column} IS NOT NULL AND id > $1 \
                 ORDER BY id LIMIT $2"
            );
            let update = format!(
                "UPDATE {table} SET {column} = u.v FROM unnest($1::int8[], $2::bytea[]) \
                 AS u(id, v) WHERE {table}.id = u.id"
            );
            let mut after: i64 = 0;
            loop {
                let rows = self.query(&select, &[&after, &SEAL_BATCH]).await?;
                let Some(last) = rows.last() else {
                    break;
                };
                after = last
                    .try_get(0)
                    .map_err(db_error("could not read the id of a converted row"))?;
                let mut ids = Vec::with_capacity(rows.len());
                let mut envelopes = Vec::with_capacity(rows.len());
                for row in &rows {
                    let id: i64 = row
                        .try_get(0)
                        .map_err(db_error("could not read the id of a converted row"))?;
                    let bytes: Vec<u8> = row
                        .try_get(1)
                        .map_err(db_error("could not read a converted encrypted value"))?;
                    let text = std::str::from_utf8(&bytes).map_err(|_| {
                        Error::new(
                            ErrorKind::Database,
                            format!(
                                "{}.{}: a value converted for encryption is not UTF-8 text",
                                clear.sealed.model, clear.sealed.field
                            ),
                        )
                    })?;
                    let sealed = clear.sealed.seal(erp_fields::Cell::Str(text.into()))?;
                    let erp_fields::Cell::Bytes(envelope) = sealed else {
                        return Err(Error::invariant("sealing text did not produce bytes"));
                    };
                    ids.push(id);
                    envelopes.push(envelope.to_vec());
                }
                self.execute(&update, &[&ids, &envelopes]).await?;
            }
        }
        Ok(())
    }
}
