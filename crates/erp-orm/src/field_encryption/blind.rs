// SPDX-License-Identifier: MIT OR Apache-2.0
//! Blind indexes of encrypted fields (`#[field(char, encrypted = true, blind_index = true)]`).
//!
//! An envelope is sealed with a random nonce, so two equal values never have equal bytes and
//! the database can neither compare nor index them. A blind-indexed field also keeps, in a
//! companion column `<field>__bidx` (`text COLLATE "C"`, not a field of the registry), the
//! keyed hash of its value:
//!
//! ```text
//! <key id>:<hex of HMAC-SHA256(K, value)>
//! K = HKDF-SHA256(ikm = key bytes, info = label | model | field)
//! ```
//!
//! The key is the last key of `field_key_file` ([`FieldKeys::index_id`]): a rotation puts the new
//! sealing key first and leaves the blind indexes as they are. Each field derives its own key, so
//! the same value in two fields (or two models) has two unrelated hashes. The value is hashed as
//! the ORM stores it, without normalisation: a blind-indexed `=` matches exactly what `=` matches
//! on a clear column, and an addon that wants `"1234 5678"` to equal `"12345678"` normalises the
//! value before it is written.
//!
//! What it allows: `=`, `!=`, `in` and `not in` conditions (the SQL compiler hashes the values
//! with [`hash_installed`] and compares them with the companion column, which the ORM indexes), and typed keys
//! ([`erp_registry::KeyDecl`]) naming the field, which are built on the companion column. What it
//! reveals: which rows hold equal values (a property of any deterministic index); not the values,
//! which the hash does not give back without the key.
//!
//! Schema sync adds the companion column and its index, then fills the rows whose hash is
//! missing, stale (another key id) or left over from a cleared value, in batches, in the
//! transaction of the module operation, before the keys of the table are built. A field that
//! stops being blind-indexed loses its companion column. An upsert writing a blind-indexed field
//! is refused for now: it would not maintain the hash.

use std::fmt::Write as _;

use erp_fields::{Cell, Error, ErrorKind, Result, Storage};
use erp_registry::{BLIND_INDEX_SUFFIX, FieldSpec, ModelSpec};
use hkdf::Hkdf;
use postgres_types::ToSql;
use sha2::Sha256;
use zeroize::Zeroizing;

use super::sync::ClearColumn;
use super::{FieldKeys, KEY_LEN, Sealed, keys_for};
use crate::Transaction;

/// Domain separation of the blind index keys.
pub const BLIND_INDEX_LABEL: &[u8] = b"erpforall.blind-index.v1";
/// Rows read and written per round trip while filling a companion column.
const FILL_BATCH: i64 = 1_000;
/// Longest Postgres identifier.
const MAX_IDENT_LEN: usize = 63;

impl FieldKeys {
    /// The blind index of `clear` for `model.field`: `<key id>:<64 hex digits>`, the HMAC-SHA256
    /// of the value under a key derived (HKDF-SHA256) from the index key
    /// ([`FieldKeys::index_id`]), the model and the field.
    ///
    /// # Errors
    ///
    /// `Internal` when the key derivation refuses (not reachable for 32-byte keys).
    pub fn blind_index(&self, model: &str, field: &str, clear: &[u8]) -> Result<String> {
        let key = self.index_key();
        let mut info = Vec::with_capacity(BLIND_INDEX_LABEL.len() + model.len() + field.len() + 24);
        for part in [BLIND_INDEX_LABEL, model.as_bytes(), field.as_bytes()] {
            info.extend_from_slice(&(part.len() as u64).to_le_bytes());
            info.extend_from_slice(part);
        }
        let mut derived = Zeroizing::new([0u8; KEY_LEN]);
        Hkdf::<Sha256>::new(None, &key.bytes[..])
            .expand(&info, &mut derived[..])
            .map_err(|_| Error::internal("could not derive a blind index key"))?;
        let mut mac = <hmac::Hmac<Sha256> as hmac::KeyInit>::new_from_slice(&derived[..])
            .map_err(|_| Error::internal("could not key a blind index"))?;
        hmac::Mac::update(&mut mac, clear);
        let digest = hmac::Mac::finalize(mac).into_bytes();
        let mut out = String::with_capacity(key.id.len() + 1 + 2 * digest.len());
        out.push_str(&key.id);
        out.push(':');
        for byte in digest {
            // writing to a String does not fail
            let _ = write!(out, "{byte:02x}");
        }
        Ok(out)
    }
}

/// A stored blind-indexed field: its model, field and companion column.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Blind {
    model: &'static str,
    field: &'static str,
    column: String,
}

impl Blind {
    /// `Some` for a field with a companion column (stored, encrypted and blind-indexed).
    pub(crate) fn of(spec: &ModelSpec, field: &FieldSpec) -> Option<Self> {
        field.blind_index_column().map(|column| Self {
            model: spec.name,
            field: field.name,
            column,
        })
    }

    /// The companion column.
    pub(crate) fn column(&self) -> &str {
        &self.column
    }

    /// The blind index of one clear text.
    fn hash(&self, keys: &FieldKeys, clear: &str) -> Result<String> {
        keys.blind_index(self.model, self.field, clear.as_bytes())
    }

    /// The companion cells of clear cache cells: text becomes its blind index, NULL stays NULL.
    pub(crate) fn hash_all(&self, cells: &[Cell]) -> Result<Vec<Cell>> {
        let keys = keys_for(self.model, self.field)?;
        cells
            .iter()
            .map(|cell| match cell {
                Cell::Null => Ok(Cell::Null),
                Cell::Str(text) => Ok(Cell::Str(self.hash(keys, text)?.into())),
                _ => Err(Error::invariant(format!(
                    "the cache value of the encrypted field {}.{} is not text",
                    self.model, self.field
                ))),
            })
            .collect()
    }

    /// The array parameter of the companion column for clear cache cells.
    pub(crate) fn param(&self, cells: &[Cell]) -> Result<Box<dyn ToSql + Sync + Send>> {
        crate::write::array_param(Storage::Str, self.hash_all(cells)?)
    }
}

/// The blind index of `clear` for `model.field` with the installed keys: the hasher the SQL
/// compiler calls for a condition on a blind-indexed field ([`erp_sql::blind`]).
pub(crate) fn hash_installed(model: &str, field: &str, clear: &str) -> Result<String> {
    keys_for(model, field)?.blind_index(model, field, clear.as_bytes())
}

/// The columns of a typed key over `fields`: a blind-indexed field is its companion column.
pub(crate) fn key_columns(spec: &ModelSpec, fields: &[&str]) -> Vec<String> {
    fields
        .iter()
        .map(|name| {
            spec.field(name)
                .and_then(FieldSpec::blind_index_column)
                .unwrap_or_else(|| (*name).to_owned())
        })
        .collect()
}

/// The name of the ORM's index on a companion column: `<table>_<column>_idx`, or a name with a
/// digest when that is longer than Postgres allows.
fn index_name(table: &str, column: &str) -> String {
    let name = format!("{table}_{column}_idx");
    if name.len() <= MAX_IDENT_LEN {
        return name;
    }
    let digest = <Sha256 as sha2::Digest>::digest(name.as_bytes());
    let mut hex = String::with_capacity(16);
    for byte in &digest[..8] {
        let _ = write!(hex, "{byte:02x}");
    }
    // `_` + 16 hex digits + `_bidx_idx` after the prefix
    let room = MAX_IDENT_LEN - 1 - hex.len() - "_bidx_idx".len();
    let mut prefix: String = table.chars().take(room).collect();
    while prefix.len() > room {
        prefix.pop();
    }
    format!("{prefix}_{hex}_bidx_idx")
}

fn db_error(what: &'static str) -> impl Fn(tokio_postgres::Error) -> Error {
    move |error| Error::new(ErrorKind::Database, what).with_source(error)
}

impl Transaction {
    /// Schema sync of the blind indexes of `specs`, after the columns are in place and before the
    /// keys are built: add the companion column and its index of each blind-indexed field and
    /// fill the rows whose hash is missing or stale; drop the companion column of an encrypted
    /// field that is no longer blind-indexed. `clear` lists the encrypted columns that still hold
    /// Add and backfill companion columns for blind-indexed encrypted fields before Phase 2 builds
    /// unique keys over them. Populates existing rows before building the B-tree index and skips
    /// view-backed models. Returns `true` if any DDL was executed.
    pub(crate) async fn sync_blind_indexes_add_and_fill(
        &mut self,
        specs: &[&ModelSpec],
        views: &rustc_hash::FxHashSet<String>,
        clear: &[ClearColumn],
    ) -> Result<bool> {
        let mut ddl_ran = false;
        for spec in specs {
            if views.contains(&spec.table) {
                continue;
            }
            let encrypted: Vec<&FieldSpec> = spec
                .fields
                .iter()
                .filter(|f| f.encrypted && f.has_column() && f.inherited.is_none())
                .collect();
            if encrypted.is_empty() {
                continue;
            }
            let companions: Vec<String> = encrypted
                .iter()
                .map(|f| format!("{}{BLIND_INDEX_SUFFIX}", f.name))
                .collect();
            let existing = self
                .existing_blind_columns(&spec.table, &companions)
                .await?;
            for field in encrypted {
                let Some(blind) = Blind::of(spec, field) else {
                    continue;
                };
                let exists = existing.iter().any(|c| c == blind.column());
                let in_clear = clear.iter().any(|c| c.is(&spec.table, field.name));
                if !exists {
                    let table = erp_sql::quote(&spec.table);
                    let column = erp_sql::quote(blind.column());
                    self.batch(&format!(
                        "ALTER TABLE {table} ADD COLUMN {column} text COLLATE \"C\""
                    ))
                    .await?;
                    ddl_ran = true;
                }
                // Fill existing rows BEFORE building the B-tree index to avoid page-split bloat.
                self.fill_blind_index(spec, field, &blind, in_clear).await?;
                let idx_name = index_name(&spec.table, blind.column());
                let has_valid_idx = self.has_valid_blind_index(&spec.table, &idx_name).await?;
                if !has_valid_idx {
                    let table = erp_sql::quote(&spec.table);
                    let column = erp_sql::quote(blind.column());
                    self.batch(&format!(
                        "CREATE INDEX IF NOT EXISTS {} ON {table} ({column})",
                        erp_sql::quote(&idx_name)
                    ))
                    .await?;
                    ddl_ran = true;
                }
            }
        }
        Ok(ddl_ran)
    }

    /// Drop companion columns of encrypted fields that no longer declare `blind_index = true`.
    /// Runs AFTER Phase 2 drops superseded unique keys so PostgreSQL does not fail with `2BP01`.
    pub(crate) async fn sync_blind_indexes_drop(
        &mut self,
        specs: &[&ModelSpec],
        views: &rustc_hash::FxHashSet<String>,
    ) -> Result<bool> {
        let mut ddl_ran = false;
        for spec in specs {
            if views.contains(&spec.table) {
                continue;
            }
            let encrypted: Vec<&FieldSpec> = spec
                .fields
                .iter()
                .filter(|f| f.encrypted && f.has_column() && f.inherited.is_none())
                .collect();
            if encrypted.is_empty() {
                continue;
            }
            let companions: Vec<String> = encrypted
                .iter()
                .map(|f| format!("{}{BLIND_INDEX_SUFFIX}", f.name))
                .collect();
            let existing = self
                .existing_blind_columns(&spec.table, &companions)
                .await?;
            for field in encrypted {
                if Blind::of(spec, field).is_some() {
                    continue;
                }
                let column = format!("{}{BLIND_INDEX_SUFFIX}", field.name);
                if existing.contains(&column) {
                    self.batch(&format!(
                        "ALTER TABLE {} DROP COLUMN {}",
                        erp_sql::quote(&spec.table),
                        erp_sql::quote(&column)
                    ))
                    .await?;
                    ddl_ran = true;
                }
            }
        }
        Ok(ddl_ran)
    }

    async fn existing_blind_columns(
        &mut self,
        table: &str,
        companions: &[String],
    ) -> Result<Vec<String>> {
        let rows = self
            .query(
                "SELECT a.attname::text \
                 FROM pg_attribute a \
                 JOIN pg_class c ON c.oid = a.attrelid \
                 JOIN pg_namespace n ON n.oid = c.relnamespace \
                 WHERE n.nspname = current_schema() AND c.relname = $1 \
                   AND a.attname = ANY($2) AND a.attnum > 0 AND NOT a.attisdropped",
                &[&table, &companions],
            )
            .await?;
        let mut existing = Vec::with_capacity(rows.len());
        for row in rows {
            existing.push(
                row.try_get(0)
                    .map_err(db_error("could not read the catalog of a blind index"))?,
            );
        }
        Ok(existing)
    }

    async fn has_valid_blind_index(&mut self, table: &str, idx_name: &str) -> Result<bool> {
        let rows = self
            .query(
                "SELECT EXISTS ( \
                     SELECT 1 FROM pg_index i \
                     JOIN pg_class idx ON idx.oid = i.indexrelid \
                     JOIN pg_class tbl ON tbl.oid = i.indrelid \
                     JOIN pg_namespace n ON n.oid = tbl.relnamespace \
                     WHERE n.nspname = current_schema() AND tbl.relname = $1 \
                       AND idx.relname = $2 AND i.indisvalid \
                 )",
                &[&table, &idx_name],
            )
            .await?;
        Ok(rows
            .first()
            .and_then(|r| r.try_get::<_, bool>(0).ok())
            .unwrap_or(false))
    }

    /// Fill the companion column of `field` where it does not hold the hash of the current index
    /// key: rows with a value and no hash or a hash of another key, and rows whose value was
    /// cleared but kept a hash. Batches of [`FILL_BATCH`] rows by ascending id.
    async fn fill_blind_index(
        &mut self,
        spec: &ModelSpec,
        field: &FieldSpec,
        blind: &Blind,
        in_clear: bool,
    ) -> Result<()> {
        let keys = keys_for(spec.name, field.name)?;
        let prefix = format!("{}:", keys.index_id());
        let table = erp_sql::quote(&spec.table);
        let value = erp_sql::quote(field.name);
        let column = erp_sql::quote(blind.column());
        let select = format!(
            "SELECT id, {value} FROM {table} WHERE id > $1 AND (({value} IS NOT NULL AND \
             ({column} IS NULL OR NOT starts_with({column}, $3))) OR ({value} IS NULL AND \
             {column} IS NOT NULL)) ORDER BY id LIMIT $2"
        );
        let update = format!(
            "UPDATE {table} SET {column} = u.h FROM unnest($1::int8[], $2::text[]) AS u(id, h) \
             WHERE {table}.id = u.id"
        );
        let sealed = Sealed::of(spec, field)
            .ok_or_else(|| Error::invariant("a blind-indexed field without a sealed column"))?;
        let mut after: i64 = 0;
        loop {
            let rows = self.query(&select, &[&after, &FILL_BATCH, &prefix]).await?;
            let Some(last) = rows.last() else {
                break;
            };
            after = last
                .try_get(0)
                .map_err(db_error("could not read the id of a blind-indexed row"))?;
            let mut ids = Vec::with_capacity(rows.len());
            let mut hashes: Vec<Option<String>> = Vec::with_capacity(rows.len());
            for row in &rows {
                let id: i64 = row
                    .try_get(0)
                    .map_err(db_error("could not read the id of a blind-indexed row"))?;
                let stored: Option<Vec<u8>> = row
                    .try_get(1)
                    .map_err(db_error("could not read a blind-indexed value"))?;
                let hash = match stored {
                    None => None,
                    Some(bytes) => {
                        let text = if in_clear {
                            std::str::from_utf8(&bytes)
                                .map_err(|_| {
                                    Error::new(
                                        ErrorKind::Database,
                                        format!(
                                            "{}.{}: a value converted for encryption is not UTF-8 \
                                             text",
                                            spec.name, field.name
                                        ),
                                    )
                                })?
                                .to_owned()
                        } else {
                            match sealed.open(Cell::Bytes(bytes.into()))? {
                                Cell::Str(text) => text.to_string(),
                                _ => {
                                    return Err(Error::invariant(
                                        "opening a value did not produce text",
                                    ));
                                }
                            }
                        };
                        Some(blind.hash(keys, &text)?)
                    }
                };
                ids.push(id);
                hashes.push(hash);
            }
            self.execute(&update, &[&ids, &hashes]).await?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn index_names_fit_postgres() {
        assert_eq!(
            index_name("hr_person", "nid__bidx"),
            "hr_person_nid__bidx_idx"
        );
        let long = index_name(&"t".repeat(60), "national_identity_number__bidx");
        assert!(long.len() <= MAX_IDENT_LEN, "{long}");
        assert!(long.ends_with("_bidx_idx"));
        assert_ne!(long, index_name(&"t".repeat(60), "other__bidx"));
    }
}
