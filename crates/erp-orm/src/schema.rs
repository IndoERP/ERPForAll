//! Schema synchronization (`init_models`, MODEL §4.7, CRUD §4.10).
//!
//! A transaction-scoped advisory lock serializes schema changes between processes; the catalog of
//! every table of the batch is then read in one query, and the DDL runs in MODEL-030 order: tables
//! and columns, table objects, field indexes, foreign keys. Each phase is one round trip.
//!
//! Lock-friendly forms on existing tables:
//! - a new column with a constant default uses PostgreSQL's fast default
//!   (`ADD COLUMN … DEFAULT <lit> [NOT NULL]`, then `DROP DEFAULT`): no table rewrite, no UPDATE;
//!   a `db_default` field keeps that default, and schema sync sets it on an existing
//!   column that lacks it and drops it again when the field loses the flag;
//! - foreign keys on tables that already existed are added `NOT VALID`, then validated.
//!
//! Large tables: with
//! [`Transaction::init_models_with`], work that scans an existing table with more rows than
//! [`SchemaSyncOptions::large_table_rows`] — index builds, `VALIDATE CONSTRAINT`, filling the NULLs
//! of a column that becomes required, filling a new stored computed column — leaves the
//! transaction as [`SchemaStep`]s run after the commit ([`crate::Orm::run_schema_step`]). The
//! sizes are counted in one query for the tables that have such work only. On small tables the
//! work runs inline, `SET NOT NULL` in one statement. [`Transaction::init_models`] runs everything
//! inline.
//!
//! The schema step is bounded: before it waits for anything (the schema lock included) it lowers
//! `lock_timeout` to at most [`SCHEMA_LOCK_TIMEOUT`] and `statement_timeout` to at most
//! [`SCHEMA_STATEMENT_TIMEOUT`] transaction-locally (a caller's lower value, such as a module
//! operation's, is kept; 0, no limit, is replaced), so a schema change waiting behind another
//! schema synchronization or a long business transaction, or a statement scanning a huge table,
//! fails with an error instead of stalling every request queued behind its locks (and holding a
//! pool connection meanwhile). `statement_timeout` applies to each statement on its own, not to a
//! phase as a whole (PostgreSQL 13 and later time each statement of a multi-statement query
//! separately). On success the caller's own values of both settings are put back; on error they
//! stay set and the caller rolls the transaction back ([`crate::Orm::sync_schema`] does).
//!
//! The required-column probe below and the `SET NOT NULL` that follows it are separate
//! statements: a row with a NULL inserted by another transaction in between makes the
//! `SET NOT NULL` (or its validating check) fail with the database's own error instead of the
//! probe's message; the transaction rolls back either way and nothing is half applied.
//!
//! A required field without a static default is refused on a table that has rows (new column), or
//! whose column holds NULLs (a column that becomes required): the error names the model, the field
//! and the table, before any DDL runs.
//!
//! Foreign keys are compared with the declaration (target and `ondelete`) and replaced when they
//! differ. Reflection into `ir.model*` arrives with the base addon (M3).
//!
//! Their foreign keys: `column1 → model ON DELETE CASCADE` unless another
//! field already expects something for that column, `column2 → comodel ON DELETE <ondelete>`
//! (which always wins), so mirror fields sharing a table each govern the deletion of their comodel.

use std::sync::Arc;
use std::time::Duration;

use erp_fields::{
    Cell, ColumnType, Error, FieldType, IndexKind, M2mRelation, OnDelete, RecId, Result,
};
use erp_registry::ident::index_name;
use erp_registry::{FieldDefault, FieldSpec, ModelId, ModelSpec, StaticValue, TableObjectKind};
use erp_sql::ddl::{self, ColumnDef};
use erp_sql::filter::Unaccent;
use erp_sql::literal;
use rustc_hash::{FxHashMap, FxHashSet};

use crate::Transaction;
use crate::schema_index::IndexShape;
use crate::schema_steps::{SchemaStep, SchemaSync, SchemaSyncOptions, StepAction};

/// Upper bound of the schema step's `lock_timeout`: bounds the wait for the schema lock (another
/// schema synchronization of the database) and for each table lock its DDL takes. A lower
/// `lock_timeout` of the transaction is kept.
pub const SCHEMA_LOCK_TIMEOUT: &str = "10s";
/// Upper bound of the schema step's `statement_timeout`, for each statement on its own (a DDL
/// phase is a batch of statements, each timed separately). A lower `statement_timeout` of the
/// transaction is kept.
pub const SCHEMA_STATEMENT_TIMEOUT: &str = "5min";
/// [`SCHEMA_LOCK_TIMEOUT`] as a duration (for callers that report the limit in force).
pub const SCHEMA_LOCK_TIMEOUT_DURATION: Duration = Duration::from_secs(10);
/// [`SCHEMA_STATEMENT_TIMEOUT`] as a duration.
pub const SCHEMA_STATEMENT_TIMEOUT_DURATION: Duration = Duration::from_secs(5 * 60);

/// The not-null state of a column in the catalog.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum NotNull {
    No,
    /// A not-null constraint added `NOT VALID` and not validated yet (PostgreSQL 18).
    Unvalidated,
    Yes,
}

/// `(udt name, varchar length, not null)`.
type ColumnInfo = (String, Option<u32>, NotNull);

/// A required column without a static default: its table must have no rows (a new column) or no
/// NULL in it (an existing column that becomes required).
struct NotNullCheck {
    model: &'static str,
    field: &'static str,
    table: String,
    /// `false`: a new column, the table must be empty.
    existing: bool,
}

impl NotNullCheck {
    /// `SELECT <i> WHERE EXISTS (…)`: a row when the check fails.
    fn probe(&self, i: usize) -> String {
        let table = erp_sql::quote(&self.table);
        let rows = if self.existing {
            format!(
                "SELECT FROM {table} WHERE {} IS NULL",
                erp_sql::quote(self.field)
            )
        } else {
            format!("SELECT FROM {table}")
        };
        format!("SELECT {i}::int8 WHERE EXISTS ({rows})")
    }

    fn problem(&self) -> String {
        if self.existing {
            format!(
                "required field '{}.{}' has no default and column \"{}\".\"{}\" holds NULL values; \
                 fill them or give the field a default",
                self.model, self.field, self.table, self.field
            )
        } else {
            format!(
                "required field '{}.{}' has no default and table \"{}\" has rows; give the field \
                 a default, or add it as optional, fill it, then make it required",
                self.model, self.field, self.table
            )
        }
    }
}

/// Refusal of reverse translation from jsonb to scalar on a populated table.
struct ReverseTranslationCheck {
    model: &'static str,
    field: &'static str,
    table: String,
    target_ty: ColumnType,
}

impl ReverseTranslationCheck {
    fn probe(&self, i: usize) -> String {
        let table = erp_sql::quote(&self.table);
        let col = erp_sql::quote(self.field);
        format!(
            "(SELECT {i}::int8, EXISTS (SELECT FROM {table} WHERE {col} IS NOT NULL AND jsonb_typeof({col}) = 'object' AND {col} ? 'en_US') AS translated FROM {table} LIMIT 1)"
        )
    }

    fn problem(&self, translated: bool) -> String {
        if translated {
            format!(
                "reverse translation of field '{}.{}' on populated table \"{}\" refused: converting translated jsonb column to {} would destroy non-English translations; manual migration required",
                self.model, self.field, self.table, self.target_ty
            )
        } else {
            format!(
                "conversion of jsonb field '{}.{}' to {} on populated table \"{}\" refused: converting jsonb column to {} would destroy existing JSON data; manual migration required",
                self.model, self.field, self.target_ty, self.table, self.target_ty
            )
        }
    }
}

/// Existing JSONB must be a valid property map before schema changes.
struct CompanyJsonbCheck {
    model: &'static str,
    field: &'static str,
    table: String,
    ty: FieldType,
}

impl CompanyJsonbCheck {
    fn problem(&self) -> Error {
        Error::validation(format!(
            "{}.{}: invalid company-dependent JSONB storage; an explicit migration is required",
            self.model, self.field,
        ))
    }
}

/// Validation of pre-existing jsonb column when declared translated.
struct JsonbValidationCheck {
    model: &'static str,
    field: &'static str,
    table: String,
}

impl JsonbValidationCheck {
    fn probe(&self, i: usize) -> String {
        let table = erp_sql::quote(&self.table);
        let col = erp_sql::quote(self.field);
        format!(
            "SELECT {i}::int8 WHERE EXISTS (SELECT FROM {table} WHERE {col} IS NOT NULL AND (jsonb_typeof({col}) != 'object' OR NOT ({col} ? 'en_US')))"
        )
    }

    fn problem(&self) -> String {
        format!(
            "translated field '{}.{}' in table \"{}\" contains invalid JSONB (must be an object containing 'en_US' key)",
            self.model, self.field, self.table
        )
    }
}

/// Table recording the translated jsonb columns whose content schema sync has verified
/// : a row binds a successful scan to the table OID and attribute
/// number. Recreated columns and legacy rows without identity are scanned again.
const JSONB_VERIFIED_TABLE: &str = "erp_translated_verified";

/// `(definition, comment, "type:columns")`.
type ConstraintInfo = (String, Option<String>, String);

#[derive(Debug, Default)]
struct Catalog {
    tables: FxHashSet<String>,
    /// tables that are views (`relkind = 'v'`)
    views: FxHashSet<String>,
    /// tables that are partitioned (`relkind = 'p'`)
    partitioned: FxHashSet<String>,
    /// table → fill factor storage parameter, when set
    fillfactors: FxHashMap<String, u8>,
    /// table → column → (udt name, varchar length, not null)
    columns: FxHashMap<String, FxHashMap<String, ColumnInfo>>,
    /// Qualified catalog identities of collatable columns; never durable OIDs.
    collations: FxHashMap<String, FxHashMap<String, String>>,
    /// table → constraint name → (definition, comment, "type:columns")
    constraints: FxHashMap<String, FxHashMap<String, ConstraintInfo>>,
    /// table → index name → comment, valid indexes only
    indexes: FxHashMap<String, FxHashMap<String, Option<String>>>,
    /// table → valid index name → its structure
    index_structs: FxHashMap<String, FxHashMap<String, crate::schema_index::IndexStruct>>,
    /// table → index name → access method, partial flag and operator class
    index_shapes: FxHashMap<String, FxHashMap<String, IndexShape>>,
    /// unique indexes (a field index is never unique, so one under its name is rebuilt)
    unique_indexes: FxHashSet<String>,
    /// invalid indexes (an interrupted or failed `CREATE INDEX CONCURRENTLY`): built again
    invalid_indexes: FxHashSet<String>,
    /// valid index name → `pg_get_indexdef`
    index_defs: FxHashMap<String, String>,
    /// `(table, column)` with a database default (identity and generated columns excluded) →
    /// its expression as the catalog prints it (`pg_get_expr`)
    defaults: FxHashMap<(String, String), String>,
    /// `(table, column)` marked with a pending compute backfill
    /// ([`ddl::PENDING_BACKFILL_COMMENT_PREFIX`])
    pending_backfills: FxHashSet<(String, String)>,
    extensions: FxHashSet<String>,
    /// table → trigger name → comment (user triggers only)
    triggers: FxHashMap<String, FxHashMap<String, Option<String>>>,
    /// signal bump function name → comment
    functions: FxHashMap<String, Option<String>>,
    /// `current_schema()`
    schema: Option<String>,
    /// table → (sequence its `id` default depends on, the default is exactly `nextval` of it)
    id_sequences: FxHashMap<String, (String, bool)>,
    /// every relation asked for → its `relkind` (what holds a shared sequence's name)
    relkinds: FxHashMap<String, String>,
    /// every sequence asked for → (type, increment, cycle, owned by a column)
    seq_params: FxHashMap<String, SequenceParams>,
    /// table → column → generated column catalog definition
    generated_columns:
        FxHashMap<String, FxHashMap<String, crate::schema_generated::GeneratedColumnInfo>>,
    /// table → column → provenance record from erp_schema_generated
    provenance: FxHashMap<String, FxHashMap<String, crate::schema_generated::ProvenanceInfo>>,
}

/// The parameters of an existing sequence that schema sync checks before sharing it (
/// §3): what [`ddl::create_sequence`] creates is `bigint`, increment 1, range 1 to `i64::MAX`, no
/// cycle, owned by no column.
#[derive(Debug, Clone, PartialEq, Eq)]
struct SequenceParams {
    ty: String,
    increment: String,
    cycle: bool,
    owned: bool,
    min: i64,
    max: i64,
}

impl SequenceParams {
    /// The first parameter that differs from a shared id sequence's, as `what is X, not Y`.
    fn mismatch(&self) -> Option<String> {
        if self.ty != "bigint" {
            return Some(format!("its type is {}, not bigint", self.ty));
        }
        if self.increment != "1" {
            return Some(format!("its increment is {}, not 1", self.increment));
        }
        if self.min != 1 || self.max != i64::MAX {
            return Some(format!(
                "its range is {} to {}, not 1 to {}",
                self.min,
                self.max,
                i64::MAX
            ));
        }
        if self.cycle {
            return Some("it cycles (CYCLE), so ids could repeat".to_owned());
        }
        if self.owned {
            return Some(
                "it is owned by a column (OWNED BY, or an identity), so dropping that column or \
                 table would drop it"
                    .to_owned(),
            );
        }
        None
    }
}

/// A static default as a JSON document, as `erp_fields::convert::value_to_json` writes the same
/// scalar (a static default is never a list, a map or a result batch).
fn static_json(value: &StaticValue) -> erp_fields::serde_json::Value {
    use erp_fields::serde_json::Value as J;
    match value {
        StaticValue::False => J::Bool(false),
        StaticValue::Bool(b) => J::Bool(*b),
        StaticValue::Int(i) => J::from(*i),
        StaticValue::Float(f) => {
            erp_fields::serde_json::Number::from_f64(*f).map_or(J::Null, J::Number)
        }
        StaticValue::Str(s) => J::String((*s).to_owned()),
    }
}

/// A static default as an SQL literal of the column's type, or `None` when it does not apply.
fn sql_default(field: &FieldSpec, ty: ColumnType) -> Option<String> {
    if field.company_dependent {
        return None;
    }
    // existing rows start at version 1 (fast default, dropped again)
    if matches!(field.ty, FieldType::RowVersion) {
        return Some("1::int8".to_owned());
    }
    let Some(FieldDefault::Value(value)) = &field.default else {
        return None;
    };
    let cast = |lit: String| format!("{lit}::{}", ty.udt_name());
    match (value, ty) {
        (StaticValue::Bool(b), ColumnType::Bool) => {
            Some(if *b { "TRUE" } else { "FALSE" }.to_owned())
        }
        (
            StaticValue::Int(i),
            ColumnType::Int4
            | ColumnType::Int8
            | ColumnType::BigInt
            | ColumnType::Float8
            | ColumnType::Numeric,
        ) => Some(cast(i.to_string())),
        (StaticValue::Float(f), ColumnType::Float8 | ColumnType::Numeric) if f.is_finite() => {
            Some(cast(format!("{f:?}")))
        }
        (
            StaticValue::Str(s),
            ColumnType::BigInt
            | ColumnType::Varchar(_)
            | ColumnType::Text
            | ColumnType::Date
            | ColumnType::Timestamp,
        ) => Some(cast(literal(s))),
        // JSON (and, later, translated text) columns take a JSON document.
        (value, ColumnType::Jsonb) => Some(cast(literal(&static_json(value).to_string()))),
        _ => None,
    }
}

/// The database default a `db_default` field keeps: `(literal to set, the expression
/// the catalog prints for it)`. `None` for every other field. The registry admits the flag only on
/// stored required booleans with a static boolean default, so both forms are exact.
fn persistent_default(field: &FieldSpec) -> Option<(&'static str, &'static str)> {
    if !field.db_default {
        return None;
    }
    static_bool_default(field)
}

/// A static boolean default as `(literal, catalog expression)`.
fn static_bool_default(field: &FieldSpec) -> Option<(&'static str, &'static str)> {
    match &field.default {
        Some(FieldDefault::Value(StaticValue::Bool(true))) => Some(("TRUE", "true")),
        Some(FieldDefault::Value(StaticValue::Bool(false))) => Some(("FALSE", "false")),
        _ => None,
    }
}

/// `(relation table, column)`.
type RelationColumn = (String, String);
/// `(target table, ondelete, forced)`.
type ForeignKeyTarget = (String, OnDelete, bool);

/// What a re-plan of one step looks at ([`Transaction::plan_schema_step`]).
struct Replan<'a> {
    table: &'a str,
    /// The stored computed fields the queued backfill recorded.
    recorded: &'a [&'a str],
}

/// Full sync is strict; a lifecycle subset may wait for a later module's table.
struct SchemaPlanMode<'a> {
    replan: Option<&'a Replan<'a>>,
    sql_objects: crate::schema_sqlobj::SqlObjectScope,
}

/// A relation table to create and the foreign keys its columns should have.
#[derive(Debug, Default)]
struct RelationTables {
    /// table → (relation, model table, comodel table), first declaration wins.
    tables: Vec<(M2mRelation, String, String)>,
    /// (table, column) → (target table, ondelete, forced).
    keys: Vec<(RelationColumn, ForeignKeyTarget)>,
}

impl RelationTables {
    fn expect(&mut self, table: &str, column: &str, target: &str, ondelete: OnDelete, force: bool) {
        let key = (table.to_owned(), column.to_owned());
        match self.keys.iter_mut().find(|(k, _)| *k == key) {
            Some((_, current)) if force => *current = (target.to_owned(), ondelete, true),
            Some(_) => {}
            None => self.keys.push((key, (target.to_owned(), ondelete, force))),
        }
    }
}

/// The stored many2many relations of `specs` (REL §4.7.3–4.7.4).
fn relation_tables(registry: &erp_registry::Registry, specs: &[&ModelSpec]) -> RelationTables {
    let mut out = RelationTables::default();
    for spec in specs {
        for field in &spec.fields {
            let FieldType::Many2many {
                comodel,
                relation: Some(rel),
                ondelete,
            } = &field.ty
            else {
                continue;
            };
            if field.inherited.is_some() {
                continue;
            }
            let Some(comodel) = registry.model(comodel).filter(|m| m.has_table()) else {
                continue;
            };
            if !out.tables.iter().any(|(r, _, _)| r.table == rel.table) {
                out.tables
                    .push((rel.clone(), spec.table.clone(), comodel.table.clone()));
            }
            out.expect(
                &rel.table,
                &rel.column1,
                &spec.table,
                OnDelete::Cascade,
                false,
            );
            out.expect(&rel.table, &rel.column2, &comodel.table, *ondelete, true);
        }
    }
    out
}

/// Whether an existing foreign key's definition matches the declared target and action.
fn fk_matches(definition: &str, column: &str, target: &str, ondelete: OnDelete) -> bool {
    let def = definition.replace('"', "");
    def.starts_with(&format!("FOREIGN KEY ({column}) REFERENCES {target}(id)"))
        && def.contains(&format!("ON DELETE {}", ondelete.sql()))
}

/// Relation names (tables, indexes, index-backed constraints) must be unique in the schema: two
/// declarations producing the same (shortened) name would make one DDL statement fail or, worse,
/// keep the other object.
fn unreadable_setting(error: tokio_postgres::Error) -> Error {
    Error::new(
        erp_fields::ErrorKind::Database,
        "could not read the current timeouts",
    )
    .with_source(error)
}

fn check_name_collisions(specs: &[&ModelSpec]) -> Result<()> {
    let mut owners: FxHashMap<String, String> = FxHashMap::default();
    let mut claim = |name: String, owner: String| -> Result<()> {
        match owners.get(&name) {
            Some(other) if *other != owner => Err(Error::programming(format!(
                "database identifier '{name}' is produced by both {other} and {owner}; rename one"
            ))),
            _ => {
                owners.insert(name, owner);
                Ok(())
            }
        }
    };
    for spec in specs {
        claim(
            spec.table.clone(),
            format!("the table of model '{}'", spec.name),
        )?;
        for field in spec.column_fields().filter(|f| f.index.is_some()) {
            claim(
                index_name(&spec.table, field.name),
                format!("the index of field {}.{}", spec.name, field.name),
            )?;
        }
        for obj in &spec.table_objects {
            // constraints and triggers are named per table, not in the schema-wide relation
            // namespace
            if !matches!(
                obj.kind,
                TableObjectKind::Constraint { .. }
                    | TableObjectKind::SignalTrigger(_)
                    | TableObjectKind::FollowDelete(_)
            ) {
                claim(
                    obj.full_name.clone(),
                    format!("table object '{}' of model '{}'", obj.full_name, spec.name),
                )?;
            }
        }
    }
    Ok(())
}

/// Decode names structurally: identifiers can themselves contain punctuation/separators.
fn decode_column_collation(payload: &str) -> Result<String> {
    let (schema, name): (String, String) = serde_json::from_str(payload).map_err(|source| {
        Error::new(
            erp_fields::ErrorKind::Database,
            "unreadable column collation identity",
        )
        .with_source(source)
    })?;
    if schema.is_empty() || name.is_empty() || schema.contains('\0') || name.contains('\0') {
        return Err(Error::new(
            erp_fields::ErrorKind::Database,
            "invalid column collation identity",
        ));
    }
    Ok(format!(
        "{}.{}",
        erp_sql::quote(&schema),
        erp_sql::quote(&name)
    ))
}

fn declared_collation(field: &FieldSpec) -> String {
    format!(
        "{}.{}",
        erp_sql::quote("pg_catalog"),
        erp_sql::quote(field.collation.as_str())
    )
}

fn catalog_collation<'a>(cat: &'a Catalog, spec: &ModelSpec, field: &FieldSpec) -> Result<&'a str> {
    cat.collations
        .get(&spec.table)
        .and_then(|columns| columns.get(field.name))
        .map(String::as_str)
        .ok_or_else(|| {
            Error::new(
                erp_fields::ErrorKind::Database,
                format!(
                    "missing catalog collation of '{}.{}'",
                    spec.name, field.name
                ),
            )
        })
}

impl Catalog {
    /// Forget everything about `table` (it is dropped and created again in phase 1).
    fn forget(&mut self, table: &str) {
        self.tables.remove(table);
        self.partitioned.remove(table);
        self.fillfactors.remove(table);
        self.columns.remove(table);
        self.collations.remove(table);
        self.constraints.remove(table);
        self.indexes.remove(table);
        self.defaults.retain(|(t, _), _| t != table);
        self.generated_columns.remove(table);
        self.provenance.remove(table);
    }
}

/// What the catalog says about `spec`'s table, for the partitioning plan.
fn partition_state<'a>(
    spec: &'a ModelSpec,
    cat: &Catalog,
) -> crate::partition::sync::TableState<'a> {
    let key_udt = spec.partition.and_then(|p| {
        cat.columns
            .get(&spec.table)
            .and_then(|columns| columns.get(p.key_name()))
            .map(|(udt, _, _)| udt.clone())
    });
    let primary_key = cat.constraints.get(&spec.table).and_then(|constraints| {
        constraints
            .values()
            .find(|(_, _, kind)| kind.starts_with("p:"))
            .map(|(definition, _, _)| definition.clone())
    });
    crate::partition::sync::TableState {
        spec,
        exists: cat.tables.contains(&spec.table),
        partitioned: cat.partitioned.contains(&spec.table),
        key_udt,
        primary_key,
    }
}

/// A pending provenance record to write into `erp_schema_generated` after DDL execution.
#[derive(Debug, Clone)]
struct PendingProvenance {
    table: String,
    column: String,
    signature: String,
}

/// Validate generated column definitions against the existing catalog and provenance.
///
/// Refuses:
/// - Catalog drift: actual PostgreSQL expression/kind/type does not match recorded provenance,
///   or existing generated column lacks a provenance row.
/// - Destructive conversions: ordinary to generated, generated to ordinary, storage kind switch
///   (VIRTUAL <-> STORED), output type change, or generation removal.
fn check_generated_columns(specs: &[&ModelSpec], cat: &Catalog) -> Result<()> {
    for spec in specs {
        let existing_cols = cat.columns.get(&spec.table);
        let existing_gen = cat.generated_columns.get(&spec.table);
        let prov_map = cat.provenance.get(&spec.table);

        if let Some(prov_cols) = prov_map {
            for (col_name, prov) in prov_cols {
                if prov.format_version != crate::schema_generated::GENERATED_FORMAT_VERSION {
                    return Err(Error::programming(format!(
                        "Generated field '{}.{}': unsupported provenance format; explicit migration required",
                        spec.name, col_name
                    )));
                }
                let Some(actual_info) = existing_gen.and_then(|g| g.get(col_name)) else {
                    return Err(Error::programming(format!(
                        "Generated field '{}.{}': catalog drift; explicit migration required",
                        spec.name, col_name
                    )));
                };
                if actual_info.expression != prov.catalog_expression
                    || actual_info.kind != prov.catalog_kind
                    || actual_info.catalog_type() != prov.catalog_type
                    || actual_info.collation != prov.catalog_collation
                {
                    return Err(Error::programming(format!(
                        "Generated field '{}.{}': catalog drift; explicit migration required",
                        spec.name, col_name
                    )));
                }
                let Some(field) = spec.column_fields().find(|f| f.name == col_name.as_str()) else {
                    return Err(Error::programming(format!(
                        "Generated field '{}.{}': automatic conversion is refused; explicit migration required",
                        spec.name, col_name
                    )));
                };
                if !field.is_generated() {
                    return Err(Error::programming(format!(
                        "Generated field '{}.{}': automatic conversion is refused; explicit migration required",
                        spec.name, col_name
                    )));
                }
            }
        }

        for field in spec.column_fields() {
            let is_declared_gen = field.is_generated();
            let is_actual_gen = existing_gen.and_then(|g| g.get(field.name)).is_some();
            let is_actual_col = existing_cols.and_then(|c| c.get(field.name)).is_some();

            if is_declared_gen {
                let gen_spec = field.generated().ok_or_else(|| {
                    Error::programming(format!(
                        "Generated field '{}.{}': missing generation declaration",
                        spec.name, field.name
                    ))
                })?;
                let declared_storage = gen_spec.storage();
                let declared_ty = field.column().ok_or_else(|| {
                    Error::programming(format!(
                        "Generated field '{}.{}': missing column type",
                        spec.name, field.name
                    ))
                })?;

                if is_actual_col && !is_actual_gen {
                    return Err(Error::programming(format!(
                        "Generated field '{}.{}': automatic conversion is refused; explicit migration required",
                        spec.name, field.name
                    )));
                }

                if let Some(actual_info) = existing_gen.and_then(|g| g.get(field.name)) {
                    let prov_info = prov_map.and_then(|p| p.get(field.name));

                    let Some(_prov) = prov_info else {
                        return Err(Error::programming(format!(
                            "Generated field '{}.{}': catalog drift; explicit migration required",
                            spec.name, field.name
                        )));
                    };

                    if field.collation_declared
                        && matches!(declared_ty, ColumnType::Varchar(_) | ColumnType::Text)
                        && actual_info.collation.as_deref()
                            != Some(declared_collation(field).as_str())
                    {
                        return Err(Error::programming(format!(
                            "Generated field '{}.{}': collation change requires an explicit migration",
                            spec.name, field.name
                        )));
                    }
                    let matches_storage = declared_storage.attgenerated() == actual_info.kind;
                    if !matches_storage {
                        return Err(Error::programming(format!(
                            "Generated field '{}.{}': automatic conversion is refused; explicit migration required",
                            spec.name, field.name
                        )));
                    }

                    let declared_typmod = match declared_ty {
                        ColumnType::Varchar(Some(size)) => {
                            i64::from(size).checked_add(4).ok_or_else(|| {
                                Error::programming("generated varchar typmod overflow")
                            })?
                        }
                        _ => -1, // All other supported ColumnType variants emit no typmod.
                    };
                    if actual_info.type_namespace != "pg_catalog"
                        || declared_ty.udt_name() != actual_info.udt_name
                        || declared_typmod != i64::from(actual_info.typmod)
                    {
                        return Err(Error::programming(format!(
                            "Generated field '{}.{}': automatic conversion is refused; explicit migration required",
                            spec.name, field.name
                        )));
                    }
                }
            } else if is_actual_gen {
                return Err(Error::programming(format!(
                    "Generated field '{}.{}': automatic conversion is refused; explicit migration required",
                    spec.name, field.name
                )));
            }
        }

        if let Some(gen_cols) = existing_gen {
            for col_name in gen_cols.keys() {
                if spec.column_fields().all(|f| f.name != col_name.as_str()) {
                    return Err(Error::programming(format!(
                        "Generated field '{}.{}': automatic conversion is refused; explicit migration required",
                        spec.name, col_name
                    )));
                }
            }
        }
    }
    Ok(())
}

impl Transaction {
    /// The constraints of `table` as schema sync reads them from the catalog, or `None` when the
    /// table does not exist (for `Orm::build_object`, which must see what sync sees).
    pub(crate) async fn catalog_constraints(
        &mut self,
        table: &str,
    ) -> Result<Option<crate::exclusion::CatalogConstraints>> {
        let mut cat = self.catalog(&[table.to_owned()]).await?;
        if !cat.tables.contains(table) {
            return Ok(None);
        }
        Ok(Some(cat.constraints.remove(table).unwrap_or_default()))
    }

    /// Take the transaction-level schema lock (re-taking it in the same transaction is free), so
    /// schema synchronizations of one database run one after the other.
    ///
    /// # Errors
    ///
    /// `Lock` when another synchronization holds it past `lock_timeout`.
    pub(crate) async fn take_schema_lock(&mut self, lock_timeout: &str) -> Result<()> {
        self.query(ddl::SCHEMA_LOCK, &[])
            .await
            .map(|_| ())
            .map_err(|error| {
                if error.kind() != erp_fields::ErrorKind::Lock {
                    return error;
                }
                let sqlstate = error.sqlstate().map(str::to_owned);
                Error::new(
                    erp_fields::ErrorKind::Lock,
                    format!(
                        "another schema synchronization of this database is running; gave up \
                         waiting for it after {lock_timeout} (lock_timeout), try again \
                         when it has finished"
                    ),
                )
                .with_sqlstate(sqlstate)
                .with_source(error)
            })
    }

    /// Probe PostgreSQL to canonicalise a WHERE clause on `table` by creating a probe index
    /// on a temporary table matching `table`, reading its `pg_get_expr(indpred)` and dropping the
    /// probe again (SEC-D2-1, D7.FOLLOW).
    ///
    /// The probe creates `_erp_probe` with `(LIKE table) ON COMMIT DROP` and index `_erp_probe_i`
    /// on `((1))`, queries `pg_get_expr`, then runs an explicit `DROP TABLE` so a second probe in
    /// the same transaction starts clean. No raw `SAVEPOINT` is used: the transaction wrapper
    /// refuses further statements after a server error, so a savepoint recovery could never run.
    /// If a probe statement fails, the error is returned as-is (naming table, index, predicate and
    /// the server's message) and aborts the sync transaction; the temp table disappears with it
    /// (and `ON COMMIT DROP` covers any path that still commits).
    ///
    /// # Privileges
    ///
    /// Adopting a foreign partial index (an index the ORM did not create, found without the ORM's
    /// comment) needs the `TEMP` privilege on the database for the schema-sync role: the probe
    /// creates a temporary table. Without it the probe's `CREATE TEMP TABLE` fails with
    /// "permission denied to create temporary tables" and the sync is refused with the
    /// "could not canonicalise index predicate" error. Grant `TEMP` (PostgreSQL grants it to
    /// `PUBLIC` by default) or drop the foreign index so the ORM builds its own.
    ///
    /// Note (L6): `CREATE TEMP TABLE ... (LIKE table)` acquires an ACCESS SHARE lock on `table`
    /// which persists for the duration of the sync transaction; this is harmless as DDL follows.
    async fn canonical_predicate(
        &mut self,
        table: &str,
        index_name: &str,
        predicate: &str,
    ) -> Result<String> {
        const PROBE_TABLE: &str = "_erp_probe";
        const PROBE_INDEX: &str = "_erp_probe_i";

        let create_table = format!(
            "CREATE TEMP TABLE {} (LIKE {}) ON COMMIT DROP",
            erp_sql::quote(PROBE_TABLE),
            erp_sql::quote(table)
        );
        let create_idx = format!(
            "CREATE INDEX {} ON {} ((1)) WHERE {predicate}",
            erp_sql::quote(PROBE_INDEX),
            erp_sql::quote(PROBE_TABLE)
        );
        let drop_table = format!("DROP TABLE {}", erp_sql::quote(PROBE_TABLE));
        let get_expr = "SELECT pg_get_expr(ix.indpred, ix.indrelid) \
                        FROM pg_index ix \
                        WHERE ix.indexrelid = 'pg_temp._erp_probe_i'::regclass";

        let probe_failed = |err: Error| {
            Error::programming(format!(
                "could not canonicalise index predicate '{predicate}' for table '{table}' \
                 (index '{index_name}'): {err} (a later release)"
            ))
        };

        self.execute(&create_table, &[])
            .await
            .map_err(probe_failed)?;
        self.execute(&create_idx, &[]).await.map_err(probe_failed)?;
        let rows = self.query(get_expr, &[]).await.map_err(probe_failed)?;
        let row = rows.first().ok_or_else(|| {
            Error::programming(format!(
                "probe index '{PROBE_INDEX}' for table '{table}' (index '{index_name}') \
                 not found in catalog (a later release)"
            ))
        })?;
        let expr: String = row.try_get(0).map_err(|e| {
            Error::programming(format!(
                "failed to decode pg_get_expr for table '{table}' (index '{index_name}'): {e} (a later release)"
            ))
        })?;
        self.execute(&drop_table, &[]).await.map_err(probe_failed)?;
        Ok(expr)
    }

    async fn catalog(&mut self, tables: &[String]) -> Result<Catalog> {
        let rows = self.query(ddl::CATALOG_QUERY, &[&tables]).await?;
        let mut cat = Catalog::default();
        let text = |row: &tokio_postgres::Row, i: usize| -> Result<Option<String>> {
            row.try_get::<_, Option<String>>(i).map_err(|e| {
                Error::new(
                    erp_fields::ErrorKind::Database,
                    "could not read the catalog",
                )
                .with_source(e)
            })
        };
        for row in &rows {
            let kind = text(row, 0)?.unwrap_or_default();
            let table = text(row, 1)?.unwrap_or_default();
            let name = text(row, 2)?.unwrap_or_default();
            match kind.as_str() {
                "table" => {
                    if let Some(ff) = text(row, 3)? {
                        let ff = ff.parse::<u8>().map_err(|_| {
                            Error::new(
                                erp_fields::ErrorKind::Database,
                                format!("unreadable fillfactor {ff:?} of table {table}"),
                            )
                        })?;
                        cat.fillfactors.insert(table.clone(), ff);
                    }
                    if text(row, 4)?.as_deref() == Some("p") {
                        cat.partitioned.insert(table.clone());
                    }
                    if text(row, 4)?.as_deref() == Some("v") {
                        cat.views.insert(table.clone());
                    }
                    cat.tables.insert(table);
                }
                "column_collation" => {
                    let payload = text(row, 3)?.ok_or_else(|| {
                        Error::new(
                            erp_fields::ErrorKind::Database,
                            "missing column collation identity",
                        )
                    })?;
                    let identity = decode_column_collation(&payload)?;
                    cat.collations
                        .entry(table)
                        .or_default()
                        .insert(name, identity);
                }
                "column" => {
                    let udt = text(row, 3)?.unwrap_or_default();
                    let len = match text(row, 4)? {
                        Some(l) => Some(l.parse().map_err(|_| {
                            Error::new(
                                erp_fields::ErrorKind::Database,
                                format!("unreadable length {l:?} of column {table}.{name}"),
                            )
                        })?),
                        None => None,
                    };
                    let not_null = match text(row, 5)?.as_deref() {
                        Some("true") => NotNull::Yes,
                        Some("false") => NotNull::No,
                        Some("unvalidated") => NotNull::Unvalidated,
                        other => {
                            return Err(Error::new(
                                erp_fields::ErrorKind::Database,
                                format!(
                                    "unreadable not-null state {other:?} of column {table}.{name}"
                                ),
                            ));
                        }
                    };
                    cat.columns
                        .entry(table)
                        .or_default()
                        .insert(name, (udt, len, not_null));
                }
                "constraint" => {
                    let def = text(row, 3)?.unwrap_or_default();
                    let comment = text(row, 4)?;
                    let kind_cols = text(row, 5)?.unwrap_or_default();
                    cat.constraints
                        .entry(table)
                        .or_default()
                        .insert(name, (def, comment, kind_cols));
                }
                "index" => {
                    let comment = text(row, 4)?;
                    match text(row, 5)?.as_deref() {
                        Some("true") => {
                            cat.index_defs
                                .insert(name.clone(), text(row, 3)?.unwrap_or_default());
                            cat.indexes.entry(table).or_default().insert(name, comment);
                        }
                        Some("false") => {
                            cat.invalid_indexes.insert(name);
                        }
                        other => {
                            return Err(Error::new(
                                erp_fields::ErrorKind::Database,
                                format!("unreadable validity {other:?} of index {name}"),
                            ));
                        }
                    }
                }
                "default" => {
                    let expression = text(row, 3)?.ok_or_else(|| {
                        Error::new(
                            erp_fields::ErrorKind::Database,
                            format!(
                                "the catalog has no default expression of column {table}.{name}"
                            ),
                        )
                    })?;
                    cat.defaults.insert((table, name), expression);
                }
                "pending" => {
                    cat.pending_backfills.insert((table, name));
                }
                "ext" => {
                    cat.extensions.insert(name);
                }
                "index_struct" => {
                    let parsed = crate::schema_index::IndexStruct::from_row(
                        &name,
                        text(row, 3)?,
                        text(row, 4)?,
                        text(row, 5)?,
                    )?;
                    cat.index_structs
                        .entry(table)
                        .or_default()
                        .insert(name, parsed);
                }
                "index_unique" => {
                    cat.unique_indexes.insert(name);
                }
                "index_shape" => {
                    let shape =
                        IndexShape::from_row(&name, text(row, 3)?, text(row, 4)?, text(row, 5)?)?;
                    cat.index_shapes
                        .entry(table)
                        .or_default()
                        .insert(name, shape);
                }
                "trigger" => {
                    let comment = text(row, 4)?;
                    cat.triggers.entry(table).or_default().insert(name, comment);
                }
                "function" => {
                    cat.functions.insert(name, text(row, 4)?);
                }
                "schema" => {
                    // NULL when no schema of the search path exists (nothing can be created)
                    cat.schema = text(row, 2)?;
                }
                "id_seq" => {
                    let exact = match text(row, 3)?.as_deref() {
                        Some("true") => true,
                        Some("false") => false,
                        other => {
                            return Err(Error::new(
                                erp_fields::ErrorKind::Database,
                                format!("unreadable id default state {other:?} of table {table}"),
                            ));
                        }
                    };
                    cat.id_sequences.insert(table, (name, exact));
                }
                "relkind" => {
                    let kind = text(row, 3)?.ok_or_else(|| {
                        Error::new(
                            erp_fields::ErrorKind::Database,
                            format!("unreadable relkind of relation {name}"),
                        )
                    })?;
                    cat.relkinds.insert(name, kind);
                }
                "seq_params" => {
                    let unreadable = |what: &str| {
                        Error::new(
                            erp_fields::ErrorKind::Database,
                            format!("unreadable {what} of sequence {name}"),
                        )
                    };
                    let ty = text(row, 3)?.ok_or_else(|| unreadable("type"))?;
                    let increment = text(row, 4)?.ok_or_else(|| unreadable("increment"))?;
                    let flags = text(row, 5)?
                        .ok_or_else(|| unreadable("cycle, owner, minimum and maximum"))?;
                    let parts: Vec<&str> = flags.split(':').collect();
                    let flag = |text: &str| match text {
                        "true" => Ok(true),
                        "false" => Ok(false),
                        _ => Err(unreadable("cycle and owner")),
                    };
                    let bound = |text: &str| {
                        text.parse::<i64>()
                            .map_err(|_| unreadable("minimum and maximum"))
                    };
                    let [cycle, owned, min, max] = parts.as_slice() else {
                        return Err(unreadable("cycle, owner, minimum and maximum"));
                    };
                    let (cycle, owned, min, max) =
                        (flag(cycle)?, flag(owned)?, bound(min)?, bound(max)?);
                    cat.seq_params.insert(
                        name,
                        SequenceParams {
                            ty,
                            increment,
                            cycle,
                            owned,
                            min,
                            max,
                        },
                    );
                }
                "generated_column" => {
                    let required = |index| -> Result<String> {
                        text(row, index)?.ok_or_else(|| {
                            Error::new(
                                erp_fields::ErrorKind::Database,
                                format!(
                                    "missing generated column catalog value for {table}.{name}"
                                ),
                            )
                        })
                    };
                    let info = crate::schema_generated::GeneratedColumnInfo::from_catalog(
                        required(3)?,
                        required(4)?,
                        &required(5)?,
                    )?;
                    cat.generated_columns
                        .entry(table)
                        .or_default()
                        .insert(name, info);
                }
                // `ddl::CATALOG_QUERY` produces exactly the kinds above
                other => {
                    return Err(Error::programming(format!(
                        "the catalog query returned a row of unknown kind {other:?} (table \
                         {table:?}, name {name:?})"
                    )));
                }
            }
        }
        Ok(cat)
    }

    /// Load provenance information from `erp_schema_generated` for the given tables.
    async fn load_provenance(&mut self, cat: &mut Catalog, tables: &[String]) -> Result<()> {
        if !cat.tables.contains(crate::schema_generated::TABLE_NAME) {
            return Ok(());
        }
        let prov_rows = self
            .query(
                "SELECT table_name, column_name, format_version, declaration_signature, \
                        catalog_expression, catalog_type, catalog_kind, catalog_collation \
                 FROM erp_schema_generated \
                 WHERE schema_name = current_schema() AND table_name = ANY($1)",
                &[&tables],
            )
            .await?;
        for prow in &prov_rows {
            let tbl: String = prow.try_get(0).map_err(|e| {
                Error::new(
                    erp_fields::ErrorKind::Database,
                    "unreadable table_name in erp_schema_generated",
                )
                .with_source(e)
            })?;
            let col: String = prow.try_get(1).map_err(|e| {
                Error::new(
                    erp_fields::ErrorKind::Database,
                    "unreadable column_name in erp_schema_generated",
                )
                .with_source(e)
            })?;
            let format_version: i32 = prow.try_get(2).map_err(|e| {
                Error::new(
                    erp_fields::ErrorKind::Database,
                    "unreadable format_version in erp_schema_generated",
                )
                .with_source(e)
            })?;
            let declaration_signature: String = prow.try_get(3).map_err(|e| {
                Error::new(
                    erp_fields::ErrorKind::Database,
                    "unreadable declaration_signature in erp_schema_generated",
                )
                .with_source(e)
            })?;
            let catalog_expression: String = prow.try_get(4).map_err(|e| {
                Error::new(
                    erp_fields::ErrorKind::Database,
                    "unreadable catalog_expression in erp_schema_generated",
                )
                .with_source(e)
            })?;
            let catalog_type: String = prow.try_get(5).map_err(|e| {
                Error::new(
                    erp_fields::ErrorKind::Database,
                    "unreadable catalog_type in erp_schema_generated",
                )
                .with_source(e)
            })?;
            let catalog_kind: String = prow.try_get(6).map_err(|e| {
                Error::new(
                    erp_fields::ErrorKind::Database,
                    "unreadable catalog_kind in erp_schema_generated",
                )
                .with_source(e)
            })?;
            let catalog_collation: Option<String> = prow.try_get(7).map_err(|e| {
                Error::new(
                    erp_fields::ErrorKind::Database,
                    "unreadable catalog_collation in erp_schema_generated",
                )
                .with_source(e)
            })?;
            cat.provenance.entry(tbl).or_default().insert(
                col,
                crate::schema_generated::ProvenanceInfo {
                    format_version,
                    declaration_signature,
                    catalog_expression,
                    catalog_type,
                    catalog_kind,
                    catalog_collation,
                },
            );
        }
        Ok(())
    }

    /// Refuse the required columns without a default that would fail on existing rows (one query,
    /// only when there are candidates).
    async fn check_not_null(&mut self, checks: &[NotNullCheck]) -> Result<()> {
        if checks.is_empty() {
            return Ok(());
        }
        let sql = checks
            .iter()
            .enumerate()
            .map(|(i, c)| c.probe(i))
            .collect::<Vec<_>>()
            .join(" UNION ALL ");
        let rows = self.query(&sql, &[]).await?;
        let mut failed = Vec::with_capacity(rows.len());
        for row in &rows {
            let i: i64 = row.try_get(0).map_err(|e| {
                Error::new(
                    erp_fields::ErrorKind::Database,
                    "could not read the required-column check",
                )
                .with_source(e)
            })?;
            let check = usize::try_from(i)
                .ok()
                .and_then(|i| checks.get(i))
                .ok_or_else(|| {
                    Error::programming(format!("required-column check returned unknown row {i}"))
                })?;
            failed.push(check.problem());
        }
        if failed.is_empty() {
            return Ok(());
        }
        failed.sort();
        Err(Error::validation(format!(
            "cannot update the schema: {}",
            failed.join("; ")
        )))
    }

    /// Read back server-normalized expression, kind, type, collation from pg_catalog and
    /// record/update provenance in `erp_schema_generated`.
    async fn record_generated_provenance(&mut self, pending: &[PendingProvenance]) -> Result<()> {
        for item in pending {
            let rows = self
                .query(
                    "SELECT a.attgenerated::text, pg_get_expr(ad.adbin, ad.adrelid)::text, \
                        json_build_array(tn.nspname, t.typname, a.atttypmod, \
                            CASE WHEN col.oid IS NULL THEN NULL \
                                 ELSE json_build_array(cn.nspname, col.collname) END)::text \
                 FROM pg_attribute a \
                 JOIN pg_class c ON c.oid = a.attrelid \
                 JOIN pg_namespace n ON n.oid = c.relnamespace \
                 JOIN pg_type t ON t.oid = a.atttypid \
                 JOIN pg_namespace tn ON tn.oid = t.typnamespace \
                 LEFT JOIN pg_attrdef ad ON ad.adrelid = a.attrelid AND ad.adnum = a.attnum \
                 LEFT JOIN pg_collation col ON col.oid = a.attcollation \
                 LEFT JOIN pg_namespace cn ON cn.oid = col.collnamespace \
                 WHERE n.nspname = current_schema() AND c.relname = $1 AND a.attname = $2 \
                   AND a.attnum > 0 AND NOT a.attisdropped AND a.attgenerated != ''",
                    &[&item.table, &item.column],
                )
                .await?;
            let [row] = rows.as_slice() else {
                return Err(Error::programming(format!(
                    "generated column '{}.{}' not found uniquely in catalog after DDL",
                    item.table, item.column
                )));
            };
            let text = |index: usize| -> Result<String> {
                row.try_get(index).map_err(|source| {
                    Error::new(
                        erp_fields::ErrorKind::Database,
                        "unreadable generated column catalog value after DDL",
                    )
                    .with_source(source)
                })
            };
            let actual = crate::schema_generated::GeneratedColumnInfo::from_catalog(
                text(0)?,
                text(1)?,
                &text(2)?,
            )?;
            let catalog_type = actual.catalog_type();
            self.execute(
                "INSERT INTO erp_schema_generated ( \
                     schema_name, table_name, column_name, format_version, declaration_signature, \
                     catalog_expression, catalog_type, catalog_kind, catalog_collation \
                 ) VALUES (current_schema(), $1, $2, $3, $4, $5, $6, $7, $8) \
                 ON CONFLICT (schema_name, table_name, column_name) DO UPDATE SET \
                     format_version = EXCLUDED.format_version, \
                     declaration_signature = EXCLUDED.declaration_signature, \
                     catalog_expression = EXCLUDED.catalog_expression, \
                     catalog_type = EXCLUDED.catalog_type, \
                     catalog_kind = EXCLUDED.catalog_kind, \
                     catalog_collation = EXCLUDED.catalog_collation",
                &[
                    &item.table,
                    &item.column,
                    &crate::schema_generated::GENERATED_FORMAT_VERSION,
                    &item.signature,
                    &actual.expression,
                    &catalog_type,
                    &actual.kind,
                    &actual.collation,
                ],
            )
            .await?;
        }
        Ok(())
    }

    /// Refuse reverse translations from jsonb to scalar on populated tables before DDL.
    async fn check_reverse_translations(
        &mut self,
        checks: &[ReverseTranslationCheck],
    ) -> Result<()> {
        if checks.is_empty() {
            return Ok(());
        }
        let sql = checks
            .iter()
            .enumerate()
            .map(|(i, c)| c.probe(i))
            .collect::<Vec<_>>()
            .join(" UNION ALL ");
        let rows = self.query(&sql, &[]).await?;
        let mut failed = Vec::with_capacity(rows.len());
        for row in &rows {
            let i: i64 = row.try_get(0).map_err(|e| {
                Error::new(
                    erp_fields::ErrorKind::Database,
                    "could not read the reverse translation check",
                )
                .with_source(e)
            })?;
            let translated: bool = row.try_get(1).map_err(|e| {
                Error::new(
                    erp_fields::ErrorKind::Database,
                    "could not read the reverse translation check flag",
                )
                .with_source(e)
            })?;
            let check = usize::try_from(i)
                .ok()
                .and_then(|i| checks.get(i))
                .ok_or_else(|| {
                    Error::programming(format!(
                        "reverse translation check returned unknown row {i}"
                    ))
                })?;
            failed.push(check.problem(translated));
        }
        if failed.is_empty() {
            return Ok(());
        }
        failed.sort();
        Err(Error::validation(format!(
            "cannot update the schema: {}",
            failed.join("; ")
        )))
    }

    /// Validate all rows in bounded windows, before applying schema DDL. There is deliberately
    /// no persistent marker: a marker for translations cannot establish company-map validity.
    pub(crate) async fn check_company_storage(
        &mut self,
        spec: &ModelSpec,
        field: &FieldSpec,
    ) -> Result<()> {
        self.check_company_jsonb(&[CompanyJsonbCheck {
            model: spec.name,
            field: field.name,
            table: spec.table.clone(),
            ty: field.ty.clone(),
        }])
        .await
    }

    async fn check_company_jsonb(&mut self, checks: &[CompanyJsonbCheck]) -> Result<()> {
        // Every admitted byte can expand to at most six JSON text bytes (a Unicode escape).
        // The codec's 64-byte per-entry charge covers punctuation and scalar-number text.
        // Mask larger values in SQL before driver ownership, and cap each result's total bytes.
        const WIRE_LIMIT: usize = crate::company_dependent::MAX_MAP_BYTES * 6;
        let decode_error = |error: tokio_postgres::Error| {
            Error::new(
                erp_fields::ErrorKind::Database,
                "could not read company-map schema validation",
            )
            .with_source(error)
        };
        for check in checks {
            let table = erp_sql::quote(&check.table);
            let column = erp_sql::quote(check.field);
            let sql = format!(
                "WITH candidates AS MATERIALIZED (\
                 SELECT id, octet_length({column}::text)::int8 AS bytes FROM {table} \
                 WHERE {column} IS NOT NULL AND ($1::int8 IS NULL OR id > $1) ORDER BY id LIMIT 64), \
                 sized AS (SELECT id, bytes, \
                 sum(LEAST(bytes, {WIRE_LIMIT}) + 32) OVER (ORDER BY id) AS total FROM candidates) \
                 SELECT t.id, CASE WHEN s.bytes <= {WIRE_LIMIT} THEN t.{column} ELSE NULL END, \
                 s.bytes > {WIRE_LIMIT} FROM sized s JOIN {table} t ON t.id=s.id \
                 WHERE s.total <= {WIRE_LIMIT} + 32 OR s.id=(SELECT min(id) FROM candidates) \
                 ORDER BY s.id"
            );
            let mut after: Option<i64> = None;
            loop {
                let rows = self.query(&sql, &[&after]).await?;
                if rows.is_empty() {
                    break;
                }
                for row in &rows {
                    let oversized: bool = row.try_get(2).map_err(decode_error)?;
                    if oversized {
                        return Err(check.problem());
                    }
                    let value: serde_json::Value = row.try_get(1).map_err(|_| check.problem())?;
                    crate::company_dependent::validate_storage(
                        &check.ty,
                        &Cell::Json(Arc::new(value)),
                    )
                    .map_err(|_| check.problem())?;
                    after = Some(row.try_get(0).map_err(decode_error)?);
                }
            }
        }
        Ok(())
    }

    /// Validate pre-existing jsonb columns declared translated before DDL. Columns already marked
    /// ([`JSONB_VERIFIED_TABLE`]) are not scanned; the ones that pass are marked.
    async fn check_jsonb_validations(&mut self, checks: &[JsonbValidationCheck]) -> Result<()> {
        if checks.is_empty() {
            return Ok(());
        }
        let marked = self.jsonb_verified_marks().await?;
        let checks: Vec<&JsonbValidationCheck> = checks
            .iter()
            .filter(|c| !marked.contains(&(c.table.clone(), c.field.to_owned())))
            .collect();
        if checks.is_empty() {
            return Ok(());
        }
        let sql = checks
            .iter()
            .enumerate()
            .map(|(i, c)| c.probe(i))
            .collect::<Vec<_>>()
            .join(" UNION ALL ");
        let rows = self.query(&sql, &[]).await?;
        let mut failed = Vec::with_capacity(rows.len());
        for row in &rows {
            let i: i64 = row.try_get(0).map_err(|e| {
                Error::new(
                    erp_fields::ErrorKind::Database,
                    "could not read the jsonb validation check",
                )
                .with_source(e)
            })?;
            let check = usize::try_from(i)
                .ok()
                .and_then(|i| checks.get(i))
                .ok_or_else(|| {
                    Error::programming(format!("jsonb validation check returned unknown row {i}"))
                })?;
            failed.push(check.problem());
        }
        if !failed.is_empty() {
            failed.sort();
            return Err(Error::validation(format!(
                "cannot update the schema: {}",
                failed.join("; ")
            )));
        }
        let verified: Vec<(&str, &str)> =
            checks.iter().map(|c| (c.table.as_str(), c.field)).collect();
        self.mark_jsonb_verified(&verified).await
    }

    /// Whether identity columns exist; reject a half-migrated mark table.
    async fn jsonb_verified_has_identity(&mut self) -> Result<bool> {
        let marks = erp_sql::quote(JSONB_VERIFIED_TABLE);
        let relation = erp_sql::literal(&marks);
        let exists_sql = format!(
            "SELECT to_regclass({relation}) IS NOT NULL, \
             (SELECT count(*) FROM pg_catalog.pg_attribute \
              WHERE attrelid = to_regclass({relation}) AND attnum > 0 AND NOT attisdropped \
              AND attname IN ('table_oid', 'column_attnum'))"
        );
        let states = self.query(&exists_sql, &[]).await?;
        let state = states.first().ok_or_else(|| {
            Error::new(
                erp_fields::ErrorKind::Database,
                "the verified-mark lookup returned no row",
            )
        })?;
        let exists: bool = state.try_get(0).map_err(|e| {
            Error::new(
                erp_fields::ErrorKind::Database,
                "could not read the verified-mark lookup",
            )
            .with_source(e)
        })?;
        let identity_columns: i64 = state.try_get(1).map_err(|e| {
            Error::new(
                erp_fields::ErrorKind::Database,
                "could not read the verified-mark identity state",
            )
            .with_source(e)
        })?;
        if !exists || identity_columns == 0 {
            return Ok(false);
        }
        if identity_columns != 2 {
            return Err(Error::programming(
                "Translation verification identity metadata is incomplete",
            ));
        }
        Ok(true)
    }

    /// The `(table, column)` pairs marked verified; empty for absent or legacy tables.
    async fn jsonb_verified_marks(&mut self) -> Result<FxHashSet<(String, String)>> {
        let mut marked = FxHashSet::default();
        if !self.jsonb_verified_has_identity().await? {
            return Ok(marked);
        }
        let marks = erp_sql::quote(JSONB_VERIFIED_TABLE);
        let rows = self
            .query(&format!("SELECT m.table_name, m.column_name FROM {marks} m \
                JOIN pg_catalog.pg_class c ON c.oid = m.table_oid \
                JOIN pg_catalog.pg_namespace n ON n.oid = c.relnamespace \
                JOIN pg_catalog.pg_attribute a ON a.attrelid = c.oid AND a.attnum = m.column_attnum \
                WHERE n.nspname = current_schema() AND c.relname = m.table_name \
                AND a.attname = m.column_name AND NOT a.attisdropped \
                AND a.atttypid = 'pg_catalog.jsonb'::regtype"), &[])
            .await?;
        for row in &rows {
            let read = |i: usize| -> Result<String> {
                row.try_get::<_, String>(i).map_err(|e| {
                    Error::new(
                        erp_fields::ErrorKind::Database,
                        "could not read the verified marks",
                    )
                    .with_source(e)
                })
            };
            marked.insert((read(0)?, read(1)?));
        }
        Ok(marked)
    }

    /// Mark translated jsonb columns verified: the ones that passed the scan, and the ones this
    /// sync created or converted itself (valid by construction).
    async fn mark_jsonb_verified(&mut self, columns: &[(&str, &str)]) -> Result<()> {
        if columns.is_empty() {
            return Ok(());
        }
        let marks = erp_sql::quote(JSONB_VERIFIED_TABLE);
        if !self.jsonb_verified_has_identity().await? {
            self.execute(
                &format!(
                    "CREATE TABLE IF NOT EXISTS {marks} (table_name text NOT NULL, \
                 column_name text NOT NULL, table_oid oid, column_attnum smallint, \
                 PRIMARY KEY (table_name, column_name))"
                ),
                &[],
            )
            .await?;
            self.execute(
                &format!(
                    "ALTER TABLE {marks} ADD COLUMN IF NOT EXISTS table_oid oid, \
            ADD COLUMN IF NOT EXISTS column_attnum smallint"
                ),
                &[],
            )
            .await?;
        }
        let tables: Vec<&str> = columns.iter().map(|c| c.0).collect();
        let names: Vec<&str> = columns.iter().map(|c| c.1).collect();
        let written = self.execute(
            &format!(
                "INSERT INTO {marks} (table_name, column_name, table_oid, column_attnum) \
                 SELECT v.table_name, v.column_name, c.oid, a.attnum \
                 FROM unnest($1::text[], $2::text[]) AS v(table_name, column_name) \
                 JOIN pg_catalog.pg_namespace n ON n.nspname = current_schema() \
                 JOIN pg_catalog.pg_class c ON c.relnamespace = n.oid AND c.relname = v.table_name \
                 JOIN pg_catalog.pg_attribute a ON a.attrelid = c.oid AND a.attname = v.column_name \
                 WHERE a.attnum > 0 AND NOT a.attisdropped AND a.atttypid = 'pg_catalog.jsonb'::regtype \
                 ON CONFLICT (table_name, column_name) DO UPDATE SET \
                 table_oid = EXCLUDED.table_oid, column_attnum = EXCLUDED.column_attnum"
            ),
            &[&tables, &names],
        )
        .await?;
        if usize::try_from(written).ok() != Some(columns.len()) {
            return Err(Error::programming(
                "Could not identify every translated column to verify",
            ));
        }
        Ok(())
    }

    /// Run one phase of schema DDL. With signals, a phase that changes the schema bumps the
    /// registry generation in the same transaction ([`Transaction::signal_schema_change`]); no
    /// caller can forget it. A sync that changes nothing bumps nothing, so restarting a worker
    /// does not make the others rebuild.
    ///
    /// `diagnose` is `Some(statement timeout of the conflict diagnostic)` for a phase planned with
    /// a business key or an exclusion over an existing table: a statement of it can fail on
    /// existing rows, the failed statement aborts the transaction, so the phase runs in a
    /// savepoint and [`Transaction::schema_conflict`] reads the ids after the rollback to it.
    async fn run_phase(
        &mut self,
        statements: Vec<String>,
        diagnose: Option<Duration>,
    ) -> Result<()> {
        if statements.is_empty() {
            return Ok(());
        }
        let sql = statements.join(";\n");
        match diagnose {
            Some(timeout) => {
                if let Err(error) = self.savepoint(async |tx| tx.batch(&sql).await).await {
                    return Err(self.schema_conflict(error, timeout).await);
                }
            }
            None => self.batch(&sql).await?,
        }
        self.signal_schema_change().await
    }

    /// The error of a failed schema phase. A unique violation (`23505`) or exclusion violation
    /// (`23P01`) on a business key, exclusion or temporal key is schema sync failing on existing
    /// records, not a business rule being broken by a write: `Validation`, naming the object and
    /// the model and up to 20 id pairs (design §10, review D7). Any other error is returned as it
    /// is. Needs the transaction usable again (the phase ran in a savepoint).
    ///
    /// The diagnostic query runs in a savepoint of its own that is always rolled back (the pairs
    /// leave through a captured variable and a sentinel error), so its `read only` and
    /// `statement_timeout` settings never outlive it.
    async fn schema_conflict(&mut self, error: Error, timeout: Duration) -> Error {
        if !matches!(error.sqlstate(), Some("23505" | "23P01")) {
            return error;
        }
        let Some(db) = std::error::Error::source(&error)
            .and_then(|s| s.downcast_ref::<tokio_postgres::Error>())
            .and_then(tokio_postgres::Error::as_db_error)
        else {
            return error;
        };
        let (Some(table), Some(constraint)) = (db.table(), db.constraint()) else {
            return error;
        };
        let registry = std::sync::Arc::clone(&self.shared.registry);
        let Some(spec) = registry.model_by_table(table) else {
            return error;
        };
        let Some(object) = spec.table_object(constraint) else {
            return error;
        };
        let (kind, query) = match &object.kind {
            TableObjectKind::Key(key) => (
                erp_sql::conflict::ConflictObject::Key,
                erp_sql::conflict::key_conflict_query(
                    &spec.table,
                    key.fields,
                    if key.nulls_distinct {
                        erp_sql::conflict::KeyMatch::Equal
                    } else {
                        erp_sql::conflict::KeyMatch::NullsAgree
                    },
                ),
            ),
            TableObjectKind::Exclusion(_) | TableObjectKind::TemporalKey(_) => {
                match self.shared.exclusions.conflict_probe(spec.id, constraint) {
                    Some((kind, query)) => (kind, query.to_owned()),
                    None => return error,
                }
            }
            _ => return error,
        };
        let sqlstate = error.sqlstate().map(str::to_owned);
        let setting = erp_sql::conflict::timeout_setting(timeout);
        let mut pairs: Option<Vec<(i64, i64)>> = None;
        let outcome = self
            .savepoint(async |tx| {
                tx.batch("SET LOCAL transaction_read_only = on").await?;
                tx.query(
                    "SELECT set_config('statement_timeout', $1, true)",
                    &[&setting],
                )
                .await?;
                let rows = tx.query(&query, &[]).await?;
                pairs = Some(crate::exclusion::read_conflict_pairs(&rows).map_err(|e| {
                    Error::new(erp_fields::ErrorKind::Database, "could not read the ids")
                        .with_source(e)
                })?);
                // always roll the savepoint back: the settings above end with it
                Err::<(), _>(
                    Error::programming("conflict diagnostic finished").with_source(DiagnosticDone),
                )
            })
            .await;
        // the savepoint returns the sentinel itself when its rollback worked; anything else is
        // a failed rollback (the transaction is dead), which must not hide behind a clean error
        let rolled_back = outcome.as_ref().err().is_some_and(|e| {
            std::error::Error::source(e).is_some_and(|s| s.is::<DiagnosticDone>())
        });
        let message = match pairs {
            Some(pairs) => {
                let message =
                    erp_sql::conflict::conflict_message(kind, object.name, spec.name, &pairs);
                if !rolled_back && let Err(rollback) = outcome {
                    return Error::new(
                        rollback.kind(),
                        format!(
                            "{message}; the transaction is unusable: rolling back the \
                             diagnostic's savepoint failed: {}",
                            rollback.message()
                        ),
                    )
                    .with_sqlstate(sqlstate)
                    .with_source(rollback);
                }
                message
            }
            None => {
                tracing::warn!(
                    model = spec.name,
                    object = object.name,
                    error = ?outcome.err().map(|e| e.message().to_owned()),
                    "the conflict diagnostic query failed; the conflict is reported without pairs"
                );
                erp_sql::conflict::conflict_message_unlisted(kind, object.name, spec.name)
            }
        };
        Error::validation(message)
            .with_sqlstate(sqlstate)
            .with_source(error)
    }

    /// Run the upgrade of the signal table's `key` check ([`crate::signal::upgrade_statements`]).
    /// A role that does not own the table cannot alter it: the error says so and names the table.
    async fn run_signal_upgrade(&mut self, statements: Vec<String>) -> Result<()> {
        if statements.is_empty() {
            return Ok(());
        }
        let sql = statements.join(";\n");
        self.batch(&sql).await.map_err(|error| {
            let not_owner = error.sqlstate() == Some("42501");
            let sqlstate = error.sqlstate().map(str::to_owned);
            let message = if not_owner {
                format!(
                    "upgrading the key check of the signal table {} failed: the connected role \
                     does not own it; run the schema sync as the table's owner once",
                    crate::signal::SIGNAL_TABLE
                )
            } else {
                format!(
                    "upgrading the key check of the signal table {} failed: {}",
                    crate::signal::SIGNAL_TABLE,
                    error.message()
                )
            };
            Error::new(error.kind(), message)
                .with_sqlstate(sqlstate)
                .with_source(error)
        })?;
        self.signal_schema_change().await
    }

    /// With signals, record in this transaction that the schema changes for the registry this
    /// process built ([`SelfApplied::Yes`](crate::SelfApplied)), so other processes rebuild and
    /// drop their statement caches; nothing without signals. One bump per transaction however
    /// often it is called ([`Transaction::signal_bump`]).
    pub(crate) async fn signal_schema_change(&mut self) -> Result<()> {
        if self.shared.signal.is_none() {
            return Ok(());
        }
        self.signal_bump(
            &crate::signal::SignalKey::Registry,
            crate::signal::SelfApplied::Yes,
        )
        .await
    }

    /// Create or update the tables of `models` (all models when empty) in the database, running
    /// all of the work in this transaction whatever the size of the tables (index builds,
    /// constraint validation, NULL fills, backfills of new stored computed columns): the form for
    /// tests and new databases. [`crate::Orm::sync_schema`] and the module operations
    /// (`erp-modules`, Amendment 2) use [`Transaction::init_models_with`], which leaves
    /// the scans of large tables to steps run after the commit.
    ///
    /// The step bounds its own waits and statements (at most [`SCHEMA_LOCK_TIMEOUT`] and
    /// [`SCHEMA_STATEMENT_TIMEOUT`], transaction-local; the transaction's own lower values are
    /// kept) and puts the transaction's previous values back when it succeeds. On error they stay
    /// set: roll the transaction back.
    ///
    /// # Errors
    ///
    /// `Lock` (SQLSTATE `55P03`) when another schema synchronization of the database holds the
    /// schema lock for longer than the step's lock timeout (at most [`SCHEMA_LOCK_TIMEOUT`]), or a
    /// table lock is not granted in that time; `Validation` for a required field without default on rows that lack it;
    /// `Programming` for colliding database identifiers, database defaults set outside the ORM
    /// and parent-store cycles; `Database` for any other database error (a statement timeout is
    /// SQLSTATE `57014`); the errors of the computations that fill new stored computed columns.
    pub async fn init_models(&mut self, models: &[ModelId]) -> Result<()> {
        let sync = self
            .init_models_with(models, &SchemaSyncOptions::inline())
            .await?;
        if let Some(step) = sync.steps().first() {
            return Err(Error::programming(format!(
                "inline schema synchronization left step '{}' to run after the commit",
                step.key()
            )));
        }
        Ok(())
    }

    /// Create or update the tables of `models` (all models when empty), applying the large-table
    /// rule: on a table that already existed and has more rows than
    /// [`SchemaSyncOptions::large_table_rows`], work that scans or rewrites it does not run here
    /// but is returned as [`SchemaStep`]s, to run after this transaction commits
    /// ([`crate::Orm::run_schema_step`]):
    ///
    /// - index builds (field indexes, the parent-store index, table-object indexes) become
    ///   `CREATE INDEX CONCURRENTLY` steps;
    /// - a new foreign key is added `NOT VALID` here (enforced for new writes) and validated by a
    ///   step;
    /// - a column that becomes required: without a default, the not-null constraint is added
    ///   `NOT VALID` here and validated by a step; with a default, the step fills the NULLs in
    ///   batches, then adds and validates the constraint (adding it first would make every edit
    ///   of a row that still holds a NULL fail);
    /// - a new stored computed column is filled by a backfill step, in batches.
    ///
    /// The catalog work that stays here is short: new columns (with PostgreSQL's fast default),
    /// `NOT VALID` constraints, dropping replaced objects, the row-version trigger. Still
    /// here on any table: column type changes, the parent-store path fill of a new `parent_path`
    /// column and table-object `CHECK`/`UNIQUE` constraints. A business key that is new
    /// or changed on a large table is refused with a `UserError` before any DDL runs: its
    /// `ADD CONSTRAINT` builds the unique index under `ACCESS EXCLUSIVE`, and the post-commit
    /// build is not supported until.
    ///
    /// Work the catalog still shows as unfinished — a missing or invalid index, a `NOT VALID`
    /// foreign key or not-null constraint, NULLs in a required column, a column whose comment
    /// marks a pending compute backfill ([`erp_sql::ddl::PENDING_BACKFILL_COMMENT_PREFIX`], set
    /// here, replaced by the label once all batches of the backfill have run) — is planned again
    /// by the next synchronization, so a crash or a failure before the steps finished loses
    /// nothing of it. A pending backfill on a table that is no longer large runs in the
    /// transaction then.
    ///
    /// With signals this transaction bumps the registry generation when it changes
    /// the schema, also when the whole work of a change goes to steps (an index, a column made
    /// required with a default on a large table): other processes take the new registry before
    /// the steps run, so they write the default before the step's constraint refuses NULLs.
    /// Work re-planned from the catalog (an invalid index, a `NOT VALID` constraint, a pending
    /// backfill) bumps nothing, nor does a synchronization that finds the schema up to date.
    ///
    /// ```no_run
    /// # async fn example(orm: &erp_orm::Orm) -> erp_fields::Result<()> {
    /// use erp_orm::{SUPERUSER_ID, SchemaSyncOptions};
    /// let mut tx = orm.begin(SUPERUSER_ID).await?;
    /// let sync = tx.init_models_with(&[], &SchemaSyncOptions::new()).await?;
    /// tx.commit().await?;
    /// for step in sync.steps() {
    ///     let report = orm.run_schema_step(step, &SchemaSyncOptions::new()).await?;
    ///     for kept in report.kept_comments() {
    ///         eprintln!("kept the comment of {}.{}", kept.table(), kept.column());
    ///     }
    /// }
    /// # Ok(()) }
    /// ```
    ///
    /// # Errors
    ///
    /// As [`Transaction::init_models`].
    pub async fn init_models_with(
        &mut self,
        models: &[ModelId],
        options: &SchemaSyncOptions,
    ) -> Result<SchemaSync> {
        let registry = Arc::clone(self.registry());
        let specs: Vec<&ModelSpec> = if models.is_empty() {
            registry
                .models()
                .iter()
                .filter(|m| m.has_generic_table())
                .collect()
        } else {
            // the closed event namespace is never created by generic sync (A2-D5): its
            // table is made and checked by `install_event_schema`
            models
                .iter()
                .map(|id| registry.model_by_id(*id))
                .filter(|m| m.has_generic_table())
                .collect()
        };
        check_name_collisions(&specs)?;
        let relations = relation_tables(&registry, &specs);
        let (saved, lock_timeout) = self.set_schema_limits(false).await?;
        // Serialize schema changes, then read the catalog under the lock.
        self.take_schema_lock(&lock_timeout).await?;
        let steps = self
            .init_models_locked(
                &specs,
                &relations,
                &registry,
                options,
                SchemaPlanMode {
                    replan: None,
                    sql_objects: if models.is_empty() {
                        crate::schema_sqlobj::SqlObjectScope::Full
                    } else {
                        crate::schema_sqlobj::SqlObjectScope::Subset
                    },
                },
            )
            .await?;
        self.set_timeouts(&saved.0, &saved.1).await?;
        Ok(SchemaSync { steps })
    }

    /// Bound the rest of the transaction by [`SCHEMA_LOCK_TIMEOUT`] and
    /// [`SCHEMA_STATEMENT_TIMEOUT`] (transaction-local), before it waits for anything, the schema
    /// lock included. Returns the caller's `(lock_timeout, statement_timeout)` for
    /// [`Self::set_timeouts`] and the `lock_timeout` now in force (for the lock error message).
    ///
    /// By default (`replace` false) the bounds only lower: a caller's lower value (a module
    /// operation's) is kept. With `replace` they are set whatever the transaction had, for the
    /// early schema lock of `sync_schema`: the ORM's own limits of that transaction
    /// (default or configured) are not a caller's choice about how long a schema change may wait.
    pub(crate) async fn set_schema_limits(
        &mut self,
        replace: bool,
    ) -> Result<((String, String), String)> {
        // bound the step before it waits for anything, the schema lock included (module docs);
        // one round trip reads the caller's values and sets ours (the `OFFSET 0` subquery is
        // planned as its own node, so it reads before the outer list sets). The bounds only ever
        // lower a timeout: a caller's lower value (a module operation's) is kept,
        // and 0 (no limit) is replaced. `pg_settings.setting` is in milliseconds for both.
        let saved = self
            .query(
                "SELECT s.lock_timeout, s.statement_timeout, \
                        set_config('lock_timeout', \
                            CASE WHEN NOT $3::bool AND s.lock_ms > 0 \
                                      AND s.lock_ms * interval '1 ms' <= ($1::text)::interval \
                                 THEN s.lock_timeout ELSE $1::text END, true), \
                        set_config('statement_timeout', \
                            CASE WHEN NOT $3::bool AND s.statement_ms > 0 \
                                      AND s.statement_ms * interval '1 ms' \
                                          <= ($2::text)::interval \
                                 THEN s.statement_timeout ELSE $2::text END, true) \
                   FROM (SELECT current_setting('lock_timeout') AS lock_timeout, \
                                current_setting('statement_timeout') AS statement_timeout, \
                                (SELECT setting::int8 FROM pg_settings \
                                  WHERE name = 'lock_timeout') AS lock_ms, \
                                (SELECT setting::int8 FROM pg_settings \
                                  WHERE name = 'statement_timeout') AS statement_ms \
                         OFFSET 0) AS s",
                &[&SCHEMA_LOCK_TIMEOUT, &SCHEMA_STATEMENT_TIMEOUT, &replace],
            )
            .await?;
        // the limits the transaction began with no longer hold: errors say so
        self.note_limits_changed(Some(SCHEMA_STATEMENT_TIMEOUT_DURATION));
        let (saved, lock_timeout): ((String, String), String) = match saved.first() {
            Some(row) => (
                (
                    row.try_get(0).map_err(unreadable_setting)?,
                    row.try_get(1).map_err(unreadable_setting)?,
                ),
                row.try_get(2).map_err(unreadable_setting)?,
            ),
            None => {
                return Err(Error::new(
                    erp_fields::ErrorKind::Database,
                    "reading the current timeouts returned no row",
                ));
            }
        };
        Ok((saved, lock_timeout))
    }

    /// Plan again the one step `key` of `table` from this transaction's registry and the catalog
    /// (`Orm::plan_schema_step`): the planner of [`Transaction::init_models_with`] runs on the
    /// models of `table` with `table` treated as large, nothing is executed, and the step with
    /// that key is returned, or `None` when the catalog shows no work of it left.
    ///
    /// Errors: `Programming` when no model of the registry (and no many2many relation table) has
    /// `table`: the registry is not the one the step was planned for, and "nothing left" would be
    /// a guess.
    pub(crate) async fn plan_schema_step(
        &mut self,
        table: &str,
        key: &crate::schema_steps::SchemaStepKey,
        options: &SchemaSyncOptions,
        recorded: &[&str],
    ) -> Result<crate::schema_steps::Replanned> {
        use crate::schema_steps::Replanned;
        let registry = Arc::clone(self.registry());
        let all: Vec<&ModelSpec> = registry
            .models()
            .iter()
            .filter(|m| m.has_generic_table())
            .collect();
        // the models of the table, or the models owning the many2many relation table: nothing of
        // other tables is read, so an unrelated table cannot fail this step
        let owners: Vec<&ModelSpec> = all
            .iter()
            .copied()
            .filter(|m| {
                m.table == table
                    || m.fields.iter().any(|f| {
                        matches!(&f.ty, FieldType::Many2many { relation: Some(r), .. }
                            if r.table == table)
                    })
            })
            .collect();
        let relations = relation_tables(&registry, &owners);
        let specs: Vec<&ModelSpec> = owners
            .iter()
            .copied()
            .filter(|m| m.table == table)
            .collect();
        if owners.is_empty() {
            return Err(Error::programming(format!(
                "schema step '{key}' works on table '{table}', which no model of this registry \
                 ({} models) has: plan it with the ORM of the current module set",
                registry.models().len()
            )));
        }
        check_name_collisions(&specs)?;
        self.set_timeouts(SCHEMA_LOCK_TIMEOUT, SCHEMA_STATEMENT_TIMEOUT)
            .await?;
        // the limits the transaction began with no longer hold: errors say so
        self.note_limits_changed(Some(SCHEMA_STATEMENT_TIMEOUT_DURATION));
        let target = Replan { table, recorded };
        let steps = self
            .init_models_locked(
                &specs,
                &relations,
                &registry,
                options,
                SchemaPlanMode {
                    replan: Some(&target),
                    sql_objects: crate::schema_sqlobj::SqlObjectScope::Full,
                },
            )
            .await?;
        if let Some(step) = steps
            .into_iter()
            .find(|step| step.table == table && step.key == key.as_str())
        {
            return Ok(Replanned::Pending(step));
        }
        Ok(
            match self.undeclared(&specs, &relations, table, key, recorded) {
                Some(what) => Replanned::NotDeclared { what },
                None => Replanned::Done,
            },
        )
    }

    /// What the registry does not declare of the work a step key names on `table`, `None` when
    /// it declares it (so that no step left means the work is done).
    fn undeclared(
        &self,
        specs: &[&ModelSpec],
        relations: &RelationTables,
        table: &str,
        key: &crate::schema_steps::SchemaStepKey,
        recorded: &[&str],
    ) -> Option<String> {
        use crate::schema_steps::SchemaStepKind;
        let graph = &self.shared.graph;
        let malformed = || Some(format!("the step key {key} names no object to look for"));
        match key.kind() {
            // an `index.` key parses as `CreateIndex`; the partitioned form is the same declaration
            SchemaStepKind::CreateIndex | SchemaStepKind::CreatePartitionedIndex => {
                let Some(name) = key.object() else {
                    return malformed();
                };
                let declared = specs.iter().any(|spec| {
                    spec.table_objects.iter().any(|o| {
                        o.full_name == name
                            && matches!(
                                o.kind,
                                TableObjectKind::Index { .. }
                                    | TableObjectKind::UniqueIndex { .. }
                                    | TableObjectKind::TypedIndex(_)
                            )
                    }) || spec.column_fields().any(|f| {
                        f.index.is_some()
                            && (index_name(&spec.table, f.name) == name
                                || (spec.parent_path == Some(f.id)
                                    && index_name(&spec.table, "parent_path_c") == name))
                    })
                });
                (!declared).then(|| format!("no declared index {name} on table {table}"))
            }
            SchemaStepKind::ValidateForeignKey => {
                let Some(name) = key.object() else {
                    return malformed();
                };
                let declared = specs.iter().any(|spec| {
                    spec.column_fields().any(|f| {
                        matches!(f.ty, FieldType::Many2one { .. })
                            && !f.company_dependent
                            && ddl::foreign_key_name(&spec.table, f.name) == name
                    })
                }) || relations
                    .keys
                    .iter()
                    .any(|((t, c), _)| t == table && ddl::foreign_key_name(t, c) == name);
                (!declared).then(|| format!("no declared foreign key {name} on table {table}"))
            }
            SchemaStepKind::NotNull => {
                let Some(column) = key.object() else {
                    return malformed();
                };
                let declared = specs
                    .iter()
                    .any(|spec| spec.column_fields().any(|f| f.name == column && f.required));
                (!declared).then(|| format!("column {table}.{column} is not a required field"))
            }
            SchemaStepKind::CreateKeyIndex | SchemaStepKind::AddKeyConstraint => {
                let Some(name) = key.object() else {
                    return malformed();
                };
                let declared = specs.iter().any(|spec| {
                    spec.table_objects
                        .iter()
                        .any(|o| o.full_name == name && matches!(o.kind, TableObjectKind::Key(_)))
                });
                (!declared).then(|| format!("no declared business key {name} on table {table}"))
            }
            SchemaStepKind::Backfill => {
                let stored_computed = |name: Option<&str>| {
                    specs.iter().any(|spec| {
                        spec.column_fields().any(|f| {
                            name.is_none_or(|n| n == f.name)
                                && graph.compute((spec.id, f.id)).is_some_and(|fc| fc.stored)
                        })
                    })
                };
                let declared = if recorded.is_empty() {
                    stored_computed(None)
                } else {
                    recorded.iter().any(|name| stored_computed(Some(name)))
                };
                (!declared).then(|| {
                    format!(
                        "none of the stored computed fields [{}] is declared on table {table}",
                        recorded.join(", ")
                    )
                })
            }
        }
    }

    /// Whether a row of `table` holds NULL in `column`.
    async fn has_null(&mut self, table: &str, column: &str) -> Result<bool> {
        let sql = format!(
            "SELECT EXISTS (SELECT 1 FROM {} WHERE {} IS NULL)",
            erp_sql::quote(table),
            erp_sql::quote(column)
        );
        // a scan of the whole table when no row is NULL: the step's statement timeout, not the
        // re-plan's short one. The value to go back to is read, not assumed (the re-plan set it
        // in `plan_schema_step`).
        let current: String = self
            .query("SELECT current_setting('statement_timeout')", &[])
            .await?
            .first()
            .ok_or_else(|| {
                Error::new(
                    erp_fields::ErrorKind::Database,
                    "reading the statement timeout returned no row",
                )
            })?
            .try_get(0)
            .map_err(|e| {
                Error::new(
                    erp_fields::ErrorKind::Database,
                    "could not read the statement timeout",
                )
                .with_source(e)
            })?;
        self.query(
            "SELECT set_config('statement_timeout', $1, true)",
            &[&crate::schema_steps::SCHEMA_STEP_STATEMENT_TIMEOUT],
        )
        .await?;
        self.note_limits_changed(None);
        // a failed probe aborts the transaction: its error is returned as it is, and nothing is
        // restored (a statement after it would only fail with 25P02 and hide the cause)
        let rows = self.query(&sql, &[]).await?;
        self.query(
            "SELECT set_config('statement_timeout', $1, true)",
            &[&current],
        )
        .await?;
        rows.first()
            .ok_or_else(|| {
                Error::new(
                    erp_fields::ErrorKind::Database,
                    "the NULL probe returned no row",
                )
            })?
            .try_get(0)
            .map_err(|e| {
                Error::new(
                    erp_fields::ErrorKind::Database,
                    "could not read the NULL probe",
                )
                .with_source(e)
            })
    }

    /// Set `lock_timeout` and `statement_timeout` for the rest of the transaction.
    pub(crate) async fn set_timeouts(
        &mut self,
        lock_timeout: &str,
        statement_timeout: &str,
    ) -> Result<()> {
        self.query(
            "SELECT set_config('lock_timeout', $1, true), set_config('statement_timeout', $2, true)",
            &[&lock_timeout, &statement_timeout],
        )
        .await?;
        Ok(())
    }

    /// The tables of `tables` with more rows than `threshold` (`None`: no table is large). One
    /// query; each count stops at `threshold + 1` rows.
    pub(crate) async fn large_tables(
        &mut self,
        tables: &[String],
        threshold: Option<u64>,
    ) -> Result<FxHashSet<String>> {
        let mut large = FxHashSet::default();
        let Some(threshold) = threshold else {
            return Ok(large);
        };
        // a limit beyond what an int8 holds counts every row: no table is that large
        let Some(limit) = threshold.checked_add(1).and_then(|l| i64::try_from(l).ok()) else {
            return Ok(large);
        };
        if tables.is_empty() {
            return Ok(large);
        }
        let sql = tables
            .iter()
            .enumerate()
            .map(|(i, t)| {
                format!(
                    "SELECT {i}::int8, (SELECT count(*) FROM (SELECT FROM {} LIMIT $1) s)",
                    erp_sql::quote(t)
                )
            })
            .collect::<Vec<_>>()
            .join(" UNION ALL ");
        let rows = self.query(&sql, &[&limit]).await?;
        for row in &rows {
            let read = |e| {
                Error::new(
                    erp_fields::ErrorKind::Database,
                    "could not read a table size",
                )
                .with_source(e)
            };
            let i: i64 = row.try_get(0).map_err(read)?;
            let count: i64 = row.try_get(1).map_err(read)?;
            let table = usize::try_from(i)
                .ok()
                .and_then(|i| tables.get(i))
                .ok_or_else(|| {
                    Error::programming(format!("table size query returned unknown row {i}"))
                })?;
            if u64::try_from(count).is_ok_and(|c| c > threshold) {
                large.insert(table.clone());
            }
        }
        Ok(large)
    }

    /// System backfills have no originating request operation. Refuse before
    /// planned DDL/data work instead of substituting a flusher's provenance.
    pub(crate) fn check_backfill_capture(&self, spec: &ModelSpec, fields: &[&str]) -> Result<()> {
        for name in fields {
            let field = spec.field(name).ok_or_else(|| {
                Error::programming(format!("backfill of unknown field '{}.{name}'", spec.name))
            })?;
            if self.capture_relevant(spec.id, field.id) {
                return Err(Error::programming(format!(
                    "outbox: stored-compute backfill capture for '{}.{name}' is not implemented; requires the a later release stored-compute-backfill capture slice",
                    spec.name
                )));
            }
        }
        Ok(())
    }

    /// True when some of `fields` reaches eventful storage, so a backfill of them needs capture
    /// admission. Non-eventful backfills pay no probe query.
    fn backfill_capture_relevant(&self, spec: &ModelSpec, fields: &[&str]) -> Result<bool> {
        let mut relevant = false;
        for name in fields {
            let field = spec.field(name).ok_or_else(|| {
                Error::programming(format!("backfill of unknown field '{}.{name}'", spec.name))
            })?;
            relevant |= self.capture_relevant(spec.id, field.id);
        }
        Ok(relevant)
    }

    /// [`Self::check_backfill_capture`] for a table that exists: the refusal applies only when the
    /// table holds at least one row, because an empty table has no record to report and nothing
    /// to credit. The probe runs only when some
    /// field is capture-relevant. It is a planning-time answer under this transaction's snapshot:
    /// a row committed after the snapshot is not seen here, which is why the inline backfill
    /// checks again under a SHARE lock ([`Self::check_backfill_relation_empty`]).
    pub(crate) async fn check_backfill_admission(
        &mut self,
        spec: &ModelSpec,
        table: &str,
        fields: &[&str],
    ) -> Result<()> {
        if !self.backfill_capture_relevant(spec, fields)? {
            return Ok(());
        }
        let sql = format!("SELECT EXISTS (SELECT FROM {})", erp_sql::quote(table));
        let rows = self.query(&sql, &[]).await?;
        let read = |e| {
            Error::new(
                erp_fields::ErrorKind::Database,
                format!("could not read the row probe of table '{table}'"),
            )
            .with_source(e)
        };
        let has_rows: bool = rows
            .first()
            .ok_or_else(|| {
                Error::new(
                    erp_fields::ErrorKind::Database,
                    format!("the row probe of table '{table}' returned no row"),
                )
            })?
            .try_get(0)
            .map_err(read)?;
        if has_rows {
            self.check_backfill_capture(spec, fields)?;
        }
        Ok(())
    }

    /// The inline backfill's second admission, run before the NULL scan under a SHARE lock on the
    /// table (the `ALTER`'s stronger lock when there was one), so no row can arrive after it. It
    /// does not depend on the snapshot: the sync transaction is REPEATABLE READ and its snapshot
    /// predates the planning probe, so a row another node committed in between is invisible to the
    /// probe and to the NULL scan alike. `pg_relation_size` reads the relation itself and answers
    /// more than zero whenever a page exists; the sizes of the members of `pg_partition_tree` are
    /// added, because a partitioned parent has no pages of its own (for a plain table that function
    /// returns no row). It is conservative: a table whose rows were all deleted but not yet
    /// vacuumed is refused too, loudly, with the same message as a populated table. Only
    /// capture-relevant backfills pay the lock and the query.
    async fn check_backfill_relation_empty(
        &mut self,
        spec: &ModelSpec,
        table: &str,
        fields: &[&str],
    ) -> Result<()> {
        if !self.backfill_capture_relevant(spec, fields)? {
            return Ok(());
        }
        let quoted = erp_sql::quote(table);
        // A pending-marked field on an existing table is backfilled with no `ALTER` on it, so no
        // lock would keep a writer out between this check and the NULL scan. SHARE blocks inserts
        // and updates until commit; when the `ALTER` already holds ACCESS EXCLUSIVE it changes
        // nothing. The sync's `lock_timeout` bounds the wait.
        self.execute_orm(&format!("LOCK TABLE {quoted} IN SHARE MODE"), &[])
            .await?;
        let rows = self
            .query(
                "SELECT (pg_relation_size(r) + coalesce((SELECT sum(pg_relation_size(t.relid)) \
                 FROM pg_partition_tree(r) t WHERE t.relid <> r), 0))::int8 \
                 FROM (SELECT $1::text::regclass AS r) q",
                &[&quoted],
            )
            .await?;
        let read = |e| {
            Error::new(
                erp_fields::ErrorKind::Database,
                format!("could not read the size of table '{table}'"),
            )
            .with_source(e)
        };
        let size: i64 = rows
            .first()
            .ok_or_else(|| {
                Error::new(
                    erp_fields::ErrorKind::Database,
                    format!("the size probe of table '{table}' returned no row"),
                )
            })?
            .try_get(0)
            .map_err(read)?;
        if size > 0 {
            self.check_backfill_capture(spec, fields)?;
        }
        Ok(())
    }

    /// Fill the new stored computed columns of small tables in this transaction: every record
    /// with a NULL in one of them is marked to compute, in batches flushed one after the other.
    async fn backfill_inline(
        &mut self,
        registry: &erp_registry::Registry,
        backfills: &[(String, &'static str, Vec<&'static str>)],
        batch: usize,
    ) -> Result<()> {
        for (table, model, fields) in backfills {
            let spec = registry.model(model).ok_or_else(|| {
                Error::programming(format!("backfill of unknown model '{model}'"))
            })?;
            self.check_backfill_relation_empty(spec, table, fields)
                .await?;
            let ids = select_null_ids(self, table, fields, None).await?;
            for chunk in ids.chunks(batch.max(1)) {
                mark_to_compute(self, spec, fields, chunk)?;
                self.flush().await?;
            }
        }
        Ok(())
    }

    /// [`Transaction::init_models_with`] once the schema lock is held. With `replan` (a table),
    /// nothing is executed: `table` counts as large and the steps planned for it are returned
    /// ([`Transaction::plan_schema_step`]).
    async fn init_models_locked(
        &mut self,
        specs: &[&ModelSpec],
        relations: &RelationTables,
        registry: &erp_registry::Registry,
        options: &SchemaSyncOptions,
        mode: SchemaPlanMode<'_>,
    ) -> Result<Vec<SchemaStep>> {
        let replan = mode.replan;
        let mut tables: Vec<String> = specs.iter().map(|m| m.table.clone()).collect();
        tables.extend(relations.tables.iter().map(|(r, _, _)| r.table.clone()));
        // the shared id sequences: the catalog says whether each exists, and as what
        let mut sequences: Vec<&'static str> = specs.iter().filter_map(|m| m.id_sequence).collect();
        sequences.sort_unstable();
        sequences.dedup();
        tables.extend(sequences.iter().map(|s| (*s).to_owned()));
        if self.shared.signal.is_some() || specs.iter().any(|spec| has_signal_trigger(spec)) {
            // the triggers write into it (phase 5 creates it when it is missing), and a table of
            // an older shape is upgraded below whichever way the Orm was built
            tables.push(crate::signal::SIGNAL_TABLE.to_owned());
        }
        tables.push(crate::schema_generated::TABLE_NAME.to_owned());
        tables.push(crate::schema_sqlobj::OBJECT_TABLE.to_owned());
        let mut cat = self.catalog(&tables).await?;
        self.load_provenance(&mut cat, &tables).await?;
        check_generated_columns(specs, &cat)?;
        // partitioned leaf models: adopt matching tables, recreate empty ones whose
        // partitioning changes, refuse the ones with rows
        let states: Vec<crate::partition::sync::TableState<'_>> = specs
            .iter()
            .map(|spec| partition_state(spec, &cat))
            .collect();
        let partitions = self.plan_partitions(&states).await?;
        for table in partitions.recreated() {
            cat.forget(table);
        }

        // ORM columns have no database default: `create` pads the columns a row does not give
        // with NULL, which equals omitting them only then. A default set outside
        // the ORM is refused, naming the columns. Exempt: `db_default` columns, whose default
        // schema sync owns (the ORM gives them in every row of a create), and an
        // unflagged required column whose default is the field's own static boolean default,
        // which is what a field that lost `db_default` leaves behind: that one is dropped below.
        let mut lost_defaults: Vec<Work> = Vec::new();
        let mut defaulted: Vec<String> = Vec::new();
        for spec in specs {
            for f in spec.column_fields() {
                if f.column().is_none()
                    || matches!(f.ty, FieldType::RowVersion)
                    || f.is_generated()
                    // a shared id sequence's default is checked below
                    || (spec.id_sequence.is_some() && matches!(f.ty, FieldType::Id))
                    || f.db_default
                {
                    continue;
                }
                let Some(expression) = cat.defaults.get(&(spec.table.clone(), f.name.to_owned()))
                else {
                    continue;
                };
                // only a field that could have carried the flag (the registry's conditions)
                if f.required
                    && !f.company_dependent
                    && static_bool_default(f).is_some_and(|(_, catalog)| catalog == expression)
                {
                    lost_defaults.push(Work::Sql(ddl::drop_column_default(&spec.table, f.name)));
                } else {
                    defaulted.push(format!("\"{}\".\"{}\"", spec.table, f.name));
                }
            }
        }
        if !defaulted.is_empty() {
            defaulted.sort();
            return Err(Error::programming(format!(
                "ORM columns must not have a database default (set outside the ORM): {}; declare \
                 the default on the field and drop the column default \
                 (ALTER TABLE … ALTER COLUMN … DROP DEFAULT)",
                defaulted.join(", ")
            )));
        }

        // The single extension gate: a declared index (or exclusion) whose extension is neither
        // installed nor installed by this sync fails the install before any DDL.
        let needed_extensions = self.extension_needs(specs, &cat.extensions);
        crate::schema_index::check_extensions(specs, &cat.extensions, &needed_extensions)?;

        let shared = Arc::clone(&self.shared);
        let graph = &shared.graph;

        // Plan every phase first: which existing tables have work that scans them decides, by
        // their size, what runs here and what becomes a step.

        // Shared id sequences: an existing table must already draw from its
        // sequence; converting a table with ids of its own is a data migration, refused.
        for spec in specs.iter().filter(|m| cat.tables.contains(&m.table)) {
            let Some(seq) = spec.id_sequence else {
                // the declaration was dropped while the table still draws from a sequence: the
                // ids would come from a default the ORM does not know about
                if let Some((found, _)) = cat.id_sequences.get(&spec.table) {
                    return Err(Error::programming(format!(
                        "model '{}' sets no id_sequence, but its existing table \"{}\" draws its \
                         id from sequence '{found}': declare id_sequence = \
                         \"{found}\" again, or migrate the ids and the column default by hand",
                        spec.name, spec.table
                    )));
                }
                continue;
            };
            match cat.id_sequences.get(&spec.table) {
                Some((found, true)) if found == seq => {}
                found => {
                    let has = match found {
                        None => "an identity id (or no id default)".to_owned(),
                        Some((other, true)) => format!("an id drawn from sequence '{other}'"),
                        Some((other, false)) => format!(
                            "an id default that uses sequence '{other}' but is not a plain nextval"
                        ),
                    };
                    return Err(Error::programming(format!(
                        "model '{}' sets id_sequence '{seq}', but its existing table \"{}\" has \
                         {has}; converting a table's ids to a shared sequence is not done by schema \
                         sync: migrate the ids and the column default by hand",
                        spec.name, spec.table
                    )));
                }
            }
        }
        let mut create_sequences: Vec<Work> = Vec::new();
        for seq in &sequences {
            match cat.relkinds.get(*seq).map(String::as_str) {
                Some("S") => {
                    let params = cat.seq_params.get(*seq).ok_or_else(|| {
                        Error::new(
                            erp_fields::ErrorKind::Database,
                            format!("the catalog has no parameters of the sequence '{seq}'"),
                        )
                    })?;
                    if let Some(why) = params.mismatch() {
                        return Err(Error::programming(format!(
                            "the shared id sequence '{seq}' exists, but {why}: a \
                             shared id sequence is bigint, increment 1, range 1 to the bigint \
                             maximum, no cycle, owned by no column; fix or rename the existing \
                             sequence"
                        )));
                    }
                }
                None => create_sequences.push(Work::Sql(ddl::create_sequence(seq))),
                Some(kind) => {
                    return Err(Error::programming(format!(
                        "the shared id sequence '{seq}' is taken by a relation of kind \
                         '{kind}' in the schema; rename that relation or the sequence"
                    )));
                }
            }
        }

        // Phase 1: tables and columns (MODEL-030 step 1).
        // tables whose partitioning changes are dropped first and created again below
        let mut phase1: Vec<Work> = partitions.drops().into_iter().map(Work::Sql).collect();
        phase1.extend(create_sequences);
        phase1.extend(lost_defaults);
        let mut post: Vec<Work> = Vec::new();
        // parent stores whose `parent_path` column is new on an existing table (CRUD §4.11)
        let mut fill_paths: Vec<(String, String)> = Vec::new();
        let mut not_null_checks: Vec<NotNullCheck> = Vec::new();
        let mut reverse_translation_checks: Vec<ReverseTranslationCheck> = Vec::new();
        let mut jsonb_validations: Vec<JsonbValidationCheck> = Vec::new();
        let mut company_jsonb_checks = Vec::new();
        // translated jsonb columns this sync creates or converts: marked verified after phase 1
        let mut fresh_translated: Vec<(String, String)> = Vec::new();
        // existing tables with stored computed columns to fill
        let mut backfills: Vec<PlannedBackfill> = Vec::new();
        // marks of pending backfills on columns no longer stored computed: cleared
        let mut stale_marks: Vec<String> = Vec::new();
        if !cat.tables.contains(crate::schema_generated::TABLE_NAME) {
            phase1.push(Work::Sql(crate::schema_generated::GENERATED_DDL.to_owned()));
        }
        let mut pending_provenance: Vec<PendingProvenance> = Vec::new();
        let mut pending_stored_checks: Vec<(&'static str, &'static str, String)> = Vec::new();
        let mut pending_collation_checks: Vec<(&'static str, &'static str, String)> = Vec::new();
        let mut pending_analyzes: Vec<(String, String)> = Vec::new();
        for spec in specs {
            let columns: Vec<(&FieldSpec, ColumnDef)> = spec
                .column_fields()
                .filter_map(|f| {
                    f.column().map(|ty| {
                        (
                            f,
                            ColumnDef {
                                collation: f.collation,
                                name: f.name.to_owned(),
                                ty,
                                // the partition key is in the primary key
                                not_null: (f.required && !f.company_dependent)
                                    || crate::partition::is_key(spec, f.id),
                                comment: f.string.clone(),
                                generated: f.generated().map(|g| {
                                    erp_fields::ColumnGenerated::new(
                                        g.storage(),
                                        g.sql().to_owned(),
                                    )
                                }),
                            },
                        )
                    })
                })
                .collect();
            if !cat.tables.contains(&spec.table) {
                fresh_translated.extend(
                    columns
                        .iter()
                        .filter(|(f, c)| {
                            f.translate && !f.company_dependent && c.ty == ColumnType::Jsonb
                        })
                        .map(|(f, _)| (spec.table.clone(), f.name.to_owned())),
                );
                let defs: Vec<ColumnDef> = columns.iter().map(|(_, c)| c.clone()).collect();
                phase1.extend(
                    match spec.partition {
                        Some(partition) => erp_sql::partition::create_partitioned_table(
                            &spec.table,
                            &spec.description,
                            &defs,
                            partition.key_name(),
                        ),
                        None => ddl::create_table_with_persistence(
                            &spec.table,
                            &spec.description,
                            &defs,
                            spec.fillfactor,
                            spec.id_sequence,
                            registry
                                .sql_objects()
                                .objects()
                                .iter()
                                .find_map(|o| match o.kind() {
                                    erp_registry::SqlObjectKind::TableOptions(t)
                                        if t.model().table() == spec.table =>
                                    {
                                        Some(t.persistence())
                                    }
                                    _ => None, // absence preserves the default logged model table
                                })
                                .unwrap_or(erp_registry::SqlTablePersistence::Logged),
                        ),
                    }
                    .into_iter()
                    .map(Work::Sql),
                );
                // the default stays on the column (catalog-only, same round trip)
                phase1.extend(columns.iter().filter_map(|(field, col)| {
                    persistent_default(field).map(|(lit, _)| {
                        Work::Sql(ddl::set_column_default(&spec.table, &col.name, lit))
                    })
                }));
                for (field, col) in &columns {
                    if let Some(gen_spec) = field.generated() {
                        let sig = crate::schema_generated::compute_signature(
                            gen_spec.storage(),
                            col.ty,
                            gen_spec.sql(),
                        );
                        pending_provenance.push(PendingProvenance {
                            table: spec.table.clone(),
                            column: col.name.clone(),
                            signature: sig,
                        });
                    }
                }
                continue;
            }
            if cat.views.contains(&spec.table) {
                for (field, _) in &columns {
                    if field.collation_declared
                        && catalog_collation(&cat, spec, field)? != declared_collation(field)
                    {
                        return Err(Error::programming(format!(
                            "view field '{}.{}': collation mismatch; explicit view migration required",
                            spec.name, field.name
                        )));
                    }
                }
                let missing = missing_view_columns(
                    columns.iter().map(|(_, c)| c.name.as_str()),
                    cat.columns.get(&spec.table),
                );
                if !missing.is_empty() {
                    return Err(Error::programming(format!(
                        "the view '{}' backing model '{}' lacks the column(s) {}: add them to the view \
                         or remove the fields",
                        spec.table,
                        spec.name,
                        missing.join(", ")
                    )));
                }
                continue;
            }
            // a declared fill factor is kept in sync; an undeclared one is left as the database
            // has it (a DBA may tune it),
            if let Some(ff) = spec.fillfactor
                && cat.fillfactors.get(&spec.table) != Some(&ff)
            {
                phase1.push(Work::Sql(ddl::set_fillfactor(&spec.table, ff)));
            }
            let existing = cat.columns.get(&spec.table);
            let mut computed: Vec<BackfillField> = Vec::new();
            for (field, col) in &columns {
                if field.generated().is_some() {
                    continue;
                }
                match existing.and_then(|e| e.get(&col.name)) {
                    None => {
                        if field.translate
                            && !field.company_dependent
                            && col.ty == ColumnType::Jsonb
                        {
                            fresh_translated.push((spec.table.clone(), field.name.to_owned()));
                        }
                        // A non-required boolean defaulting to False stays NULL (reads False).
                        let skip = matches!(
                            field.default,
                            Some(FieldDefault::Value(StaticValue::Bool(false)))
                        ) && !field.required;
                        let default = sql_default(field, col.ty).filter(|_| !skip);
                        let add = match persistent_default(field) {
                            // the fast default stays as the column's default
                            Some((lit, _)) => {
                                ddl::add_column_keeping_default(&spec.table, col, lit)
                            }
                            None => ddl::add_column(&spec.table, col, default.as_deref()),
                        };
                        phase1.extend(add.into_iter().map(Work::Sql));
                        if spec.parent_path == Some(field.id)
                            && let Some(parent) = spec.parent_name
                        {
                            fill_paths.push((
                                spec.table.clone(),
                                erp_sql::relational::compute_parent_paths(
                                    &spec.table,
                                    spec.field_by_id(parent).name,
                                ),
                            ));
                        }
                        if default.is_none()
                            && graph
                                .compute((spec.id, field.id))
                                .is_some_and(|fc| fc.stored)
                        {
                            computed.push(BackfillField {
                                name: field.name,
                                label: col.comment.clone(),
                                marked: false,
                            });
                        }
                        if col.not_null && default.is_none() {
                            // the check below makes sure the table is empty: no scan to defer
                            not_null_checks.push(NotNullCheck {
                                model: spec.name,
                                field: field.name,
                                table: spec.table.clone(),
                                existing: false,
                            });
                            post.push(Work::Sql(ddl::set_not_null(&spec.table, &col.name)));
                        }
                    }
                    Some((udt, len, not_null)) => {
                        if udt == "jsonb" && field.company_dependent {
                            company_jsonb_checks.push(CompanyJsonbCheck {
                                model: spec.name,
                                field: field.name,
                                table: spec.table.clone(),
                                ty: field.ty.clone(),
                            });
                        }
                        if udt == "jsonb" && field.translate && !field.company_dependent {
                            jsonb_validations.push(JsonbValidationCheck {
                                model: spec.name,
                                field: field.name,
                                table: spec.table.clone(),
                            });
                        }
                        if udt == "jsonb"
                            && !field.translate
                            && !field.company_dependent
                            && col.ty != ColumnType::Jsonb
                        {
                            reverse_translation_checks.push(ReverseTranslationCheck {
                                model: spec.name,
                                field: field.name,
                                table: spec.table.clone(),
                                target_ty: col.ty,
                            });
                        }
                        // a backfill interrupted before all of its batches ran (error, crash)
                        if cat
                            .pending_backfills
                            .contains(&(spec.table.clone(), col.name.clone()))
                        {
                            if graph
                                .compute((spec.id, field.id))
                                .is_some_and(|fc| fc.stored)
                            {
                                computed.push(BackfillField {
                                    name: field.name,
                                    label: col.comment.clone(),
                                    marked: true,
                                });
                            } else {
                                stale_marks.push(ddl::comment_on_column(
                                    &spec.table,
                                    &col.name,
                                    &col.comment,
                                ));
                            }
                        } else if replan.is_some_and(|r| r.recorded.contains(&field.name))
                            && graph
                                .compute((spec.id, field.id))
                                .is_some_and(|fc| fc.stored)
                            && self.has_null(&spec.table, &col.name).await?
                        {
                            // the queued backfill recorded this field and rows still lack its
                            // value, but its column comment lost the pending mark (changed
                            // outside the ORM): pending all the same
                            computed.push(BackfillField {
                                name: field.name,
                                label: col.comment.clone(),
                                marked: true,
                            });
                        }
                        let wider = match (col.ty, len) {
                            (ColumnType::Varchar(None), Some(_)) => true,
                            (ColumnType::Varchar(Some(want)), Some(have)) => want > *have,
                            _ => false,
                        };
                        let converted = udt != col.ty.udt_name() || wider;
                        let textual = matches!(col.ty, ColumnType::Varchar(_) | ColumnType::Text);
                        let collation_changed = textual
                            && matches!(udt.as_str(), "varchar" | "text" | "bpchar")
                            && catalog_collation(&cat, spec, field)? != declared_collation(field);
                        if collation_changed {
                            pending_collation_checks.push((
                                spec.name,
                                field.name,
                                spec.table.clone(),
                            ));
                        }
                        if !converted && collation_changed {
                            // Ordinary schema sync never narrows varchar: retain the installed width.
                            let retained = match col.ty {
                                ColumnType::Varchar(_) => ColumnType::Varchar(*len),
                                _ => col.ty,
                            };
                            phase1.push(Work::Sql(ddl::alter_column_collation(
                                &spec.table,
                                &col.name,
                                retained,
                                field.collation,
                            )));
                        }
                        if converted {
                            if field.company_dependent {
                                self.batch(&format!(
                                    "LOCK TABLE {} IN ACCESS EXCLUSIVE MODE",
                                    erp_sql::quote(&spec.table)
                                ))
                                .await?;
                                let populated = self
                                    .query(
                                        &format!(
                                            "SELECT EXISTS(SELECT FROM {} WHERE {} IS NOT NULL)",
                                            erp_sql::quote(&spec.table),
                                            erp_sql::quote(field.name)
                                        ),
                                        &[],
                                    )
                                    .await?;
                                let has_values: bool = populated[0].try_get(0).map_err(|e| {
                                    Error::new(
                                        erp_fields::ErrorKind::Database,
                                        "could not read company migration admission",
                                    )
                                    .with_source(e)
                                })?;
                                if has_values {
                                    return Err(Error::programming(format!(
                                        "{}.{}: populated scalar conversion requires an explicit company migration policy",
                                        spec.name, field.name
                                    )));
                                }
                                let expected = field.ty.column_type().ok_or_else(|| {
                                    Error::programming(
                                        "company migration target has no scalar column",
                                    )
                                })?;
                                if udt != expected.udt_name() {
                                    return Err(Error::programming(format!(
                                        "{}.{}: incompatible company migration predecessor type",
                                        spec.name, field.name
                                    )));
                                }
                                let attr=self.query("SELECT attnum::int4 FROM pg_attribute WHERE attrelid=to_regclass($1) AND attname=$2 AND NOT attisdropped",&[&erp_sql::quote(&spec.table),&field.name]).await?;
                                let row = attr.first().ok_or_else(|| {
                                    Error::programming(
                                        "company migration predecessor column is missing",
                                    )
                                })?;
                                let number: i32 = row.try_get(0).map_err(|e| {
                                    Error::new(
                                        erp_fields::ErrorKind::Database,
                                        "could not read company migration column",
                                    )
                                    .with_source(e)
                                })?;
                                phase1.extend(
                                    self.company_migration_dependencies(spec, field, number)
                                        .await?
                                        .into_iter()
                                        .map(Work::Sql),
                                );
                            }
                            if col.ty == ColumnType::BigInt
                                && !matches!(udt.as_str(), "int4" | "int8")
                            {
                                return Err(Error::programming(format!(
                                    "{}.{}: automatic conversion from '{}' to bigint requires an explicit migration; existing column is unchanged",
                                    spec.name, field.name, udt
                                )));
                            }
                            if udt == "int8" && col.ty == ColumnType::Int4 {
                                return Err(Error::programming(format!(
                                    "{}.{}: narrowing int8 to int4 requires an explicit migration; existing column is unchanged",
                                    spec.name, field.name
                                )));
                            }
                            // These scalars carry identity, an unsigned transaction id or an
                            // absolute instant. PostgreSQL casts may accept legacy text or
                            // reinterpret a naive timestamp in the session timezone. Neither
                            // populated nor empty tables are silently adopted across this
                            // boundary; an operator must perform an explicit migration first.
                            if matches!(udt.as_str(), "uuid" | "xid8" | "timestamptz")
                                || matches!(
                                    col.ty,
                                    ColumnType::Uuid | ColumnType::Xid8 | ColumnType::TimestampTz
                                )
                            {
                                return Err(Error::programming(format!(
                                    "{}.{}: native scalar schema type conversion from '{}' to '{}' \
                                     requires an explicit migration; existing column is unchanged",
                                    spec.name,
                                    field.name,
                                    udt,
                                    col.ty.udt_name()
                                )));
                            }
                            if field.company_dependent {
                                phase1.push(Work::Sql(format!(
                                    "ALTER TABLE {} ALTER COLUMN {} TYPE jsonb USING NULL::jsonb",
                                    erp_sql::quote(&spec.table),
                                    erp_sql::quote(field.name)
                                )));
                            } else if field.translate
                                && col.ty == ColumnType::Jsonb
                                && udt != "jsonb"
                            {
                                if !field.company_dependent {
                                    fresh_translated
                                        .push((spec.table.clone(), field.name.to_owned()));
                                }
                                let field_idx = index_name(&spec.table, &col.name);
                                if cat
                                    .indexes
                                    .get(&spec.table)
                                    .is_some_and(|i| i.contains_key(&field_idx))
                                {
                                    phase1.push(Work::Sql(ddl::drop_index(&field_idx)));
                                }
                                if let Some(structs) = cat.index_structs.get(&spec.table) {
                                    for (idx_name, st) in structs {
                                        if st.keys.iter().any(|k| k == &col.name) {
                                            phase1.push(Work::Sql(ddl::drop_index(idx_name)));
                                        }
                                    }
                                }
                                phase1.push(Work::Sql(ddl::alter_column_to_translated_jsonb(
                                    &spec.table,
                                    &col.name,
                                )));
                            } else {
                                let alter = if textual {
                                    ddl::alter_column_type_collated(
                                        &spec.table,
                                        &col.name,
                                        col.ty,
                                        field.collation,
                                    )
                                } else {
                                    ddl::alter_column_type(&spec.table, &col.name, col.ty)
                                };
                                phase1.push(Work::Sql(alter));
                            }
                        }
                        if col.not_null && *not_null != NotNull::Yes {
                            let fill = sql_default(field, col.ty);
                            let unvalidated = if *not_null == NotNull::Unvalidated {
                                Some(not_null_constraint(&cat, &spec.table, &col.name)?)
                            } else {
                                if fill.is_none() {
                                    not_null_checks.push(NotNullCheck {
                                        model: spec.name,
                                        field: field.name,
                                        table: spec.table.clone(),
                                        existing: true,
                                    });
                                }
                                None
                            };
                            post.push(Work::Table {
                                table: spec.table.clone(),
                                work: TableWork::NotNull {
                                    column: col.name.clone(),
                                    fill,
                                    unvalidated,
                                },
                            });
                        } else if !col.not_null && *not_null != NotNull::No {
                            phase1.push(Work::Sql(ddl::drop_not_null(&spec.table, &col.name)));
                        }
                        // a missing or different default is set (catalog-only); a type
                        // conversion above drops it, so it is set again after one
                        if let Some((lit, catalog)) = persistent_default(field)
                            && (converted
                                || cat
                                    .defaults
                                    .get(&(spec.table.clone(), col.name.clone()))
                                    .map(String::as_str)
                                    != Some(catalog))
                        {
                            phase1.push(Work::Sql(ddl::set_column_default(
                                &spec.table,
                                &col.name,
                                lit,
                            )));
                        }
                    }
                }
            }
            // Pass 2: Plan generated column additions and expression alterations (R2).
            for (field, col) in &columns {
                if let Some(gen_spec) = field.generated() {
                    let sig = crate::schema_generated::compute_signature(
                        gen_spec.storage(),
                        col.ty,
                        gen_spec.sql(),
                    );
                    match existing.and_then(|e| e.get(&col.name)) {
                        None => {
                            phase1.extend(
                                ddl::add_column(&spec.table, col, None)
                                    .into_iter()
                                    .map(Work::Sql),
                            );
                            if gen_spec.is_stored() {
                                pending_stored_checks.push((
                                    spec.name,
                                    field.name,
                                    spec.table.clone(),
                                ));
                                pending_analyzes.push((spec.table.clone(), col.name.clone()));
                            }
                            pending_provenance.push(PendingProvenance {
                                table: spec.table.clone(),
                                column: col.name.clone(),
                                signature: sig,
                            });
                        }
                        Some(_) => {
                            let prov = cat
                                .provenance
                                .get(&spec.table)
                                .and_then(|p| p.get(&col.name));
                            let sig_changed = prov.is_none_or(|p| p.declaration_signature != sig);
                            if sig_changed {
                                phase1.push(Work::Sql(ddl::alter_column_set_expression(
                                    &spec.table,
                                    &col.name,
                                    gen_spec.sql(),
                                )));
                                if gen_spec.is_stored() {
                                    pending_stored_checks.push((
                                        spec.name,
                                        field.name,
                                        spec.table.clone(),
                                    ));
                                    pending_analyzes.push((spec.table.clone(), col.name.clone()));
                                }
                                pending_provenance.push(PendingProvenance {
                                    table: spec.table.clone(),
                                    column: col.name.clone(),
                                    signature: sig,
                                });
                            }
                        }
                    }
                }
            }
            if !computed.is_empty() {
                let fields: Vec<_> = computed.iter().map(|field| field.name).collect();
                // a table that is not there yet is created empty
                if existing.is_some() {
                    self.check_backfill_admission(spec, &spec.table, &fields)
                        .await?;
                }
                backfills.push(PlannedBackfill {
                    table: spec.table.clone(),
                    model: spec.name,
                    fields: computed,
                });
            }
            // Columns no longer backed by a stored field lose NOT NULL (never dropped).
            if let Some(existing) = existing {
                let mut stale: Vec<&String> = existing
                    .iter()
                    .filter(|(name, (_, _, not_null))| {
                        *not_null != NotNull::No
                            && *name != "id"
                            && !columns.iter().any(|(_, c)| &c.name == *name)
                    })
                    .map(|(name, _)| name)
                    .collect();
                stale.sort();
                for name in stale {
                    phase1.push(Work::Sql(ddl::drop_not_null(&spec.table, name)));
                }
            }
        }
        // relation tables of many2many fields (never altered once they exist)
        for (rel, table1, table2) in &relations.tables {
            if !cat.tables.contains(&rel.table) {
                phase1.extend(
                    erp_sql::relational::create_relation_table(
                        erp_sql::relational::Relation {
                            table: &rel.table,
                            column1: &rel.column1,
                            column2: &rel.column2,
                        },
                        table1,
                        table2,
                    )
                    .into_iter()
                    .map(Work::Sql),
                );
            }
        }
        phase1.extend(post);

        // Extensions the table objects need, installed before them (design §5.3).
        if replan.is_none() {
            self.install_extensions(needed_extensions, &mut cat.extensions)
                .await?;
        }

        // trigram indexes (field-level and typed) fold accents with the same expression as
        // `ilike` (review S2 item 4)
        // a re-plan runs no DDL: it reads the wrapper the operation's sync created
        let unaccent = if replan.is_some() {
            self.unaccent_status().await?
        } else {
            self.setup_unaccent().await?
        };

        // The partitions of each declared window. No generation bump: attaching a partition
        // changes no result shape (design §7.9); a new partitioned table bumped in phase 1.
        // (run once phase 1 has created the tables)
        let window = partitions.window_statements(specs)?;

        // Phase 2: table objects (CRUD §4.9).
        let mut phase2: Vec<Work> = Vec::new();
        for spec in specs {
            if cat.views.contains(&spec.table) {
                continue;
            }
            let existed = cat.tables.contains(&spec.table);
            let constraints = cat.constraints.get(&spec.table);
            let indexes = cat.indexes.get(&spec.table);
            for obj in &spec.table_objects {
                match obj.kind {
                    TableObjectKind::Constraint { definition, .. } => {
                        let mut statements = Vec::new();
                        match constraints.and_then(|c| c.get(&obj.full_name)) {
                            None => statements.extend(ddl::add_constraint(
                                &spec.table,
                                &obj.full_name,
                                definition,
                            )),
                            Some((_, comment, _)) if comment.as_deref() == Some(definition) => {}
                            Some(_) => {
                                statements.push(ddl::drop_constraint(&spec.table, &obj.full_name));
                                statements.extend(ddl::add_constraint(
                                    &spec.table,
                                    &obj.full_name,
                                    definition,
                                ));
                            }
                        }
                        if statements.is_empty() {
                            continue;
                        }
                        if existed
                            && cat.partitioned.contains(&spec.table)
                            && ddl::is_unique_constraint(definition)
                        {
                            phase2.push(Work::Table {
                                table: spec.table.clone(),
                                work: TableWork::PartitionedUnique {
                                    model: spec.name,
                                    constraint: obj.full_name.clone(),
                                    statements,
                                },
                            });
                        } else {
                            phase2.extend(statements.into_iter().map(Work::Sql));
                        }
                    }
                    TableObjectKind::Key(key) => {
                        phase2.extend(key_work(spec, &obj.full_name, &key, existed, &cat)?);
                    }
                    TableObjectKind::Index { definition }
                    | TableObjectKind::UniqueIndex { definition, .. } => {
                        let unique = matches!(obj.kind, TableObjectKind::UniqueIndex { .. });
                        let expected = format!(
                            "{} {definition}",
                            if unique { "UNIQUE INDEX" } else { "INDEX" }
                        );
                        if definition.is_empty() {
                            continue;
                        }
                        let build = |replaced: bool| {
                            let (create, comment) = ddl::create_table_object_index(
                                &spec.table,
                                &obj.full_name,
                                definition,
                                unique,
                            );
                            index_work(
                                &cat,
                                &spec.table,
                                existed,
                                &obj.full_name,
                                create,
                                Some(comment),
                                replaced,
                            )
                        };
                        match indexes.and_then(|i| i.get(&obj.full_name)) {
                            None => phase2.push(build(false)),
                            // same declaration, or a manual index without comment: kept
                            Some(None) => {}
                            Some(Some(c)) if *c == expected => {}
                            Some(Some(_)) => phase2.push(build(true)),
                        }
                    }
                    // phase 5, once every table (relation tables included) exists
                    TableObjectKind::SignalTrigger(_) | TableObjectKind::FollowDelete(_) => {}
                    // typed objects: no online build, so a table above the
                    // large-table threshold refuses them (design §11)
                    TableObjectKind::Exclusion(_) | TableObjectKind::TemporalKey(_) => {
                        let Some(sync) = crate::exclusion::sync_statements(
                            &self.shared.exclusions,
                            spec,
                            obj,
                            constraints,
                        )?
                        else {
                            continue;
                        };
                        if existed {
                            phase2.push(Work::Table {
                                table: spec.table.clone(),
                                work: TableWork::Exclusion {
                                    model: spec.name,
                                    object: obj.name,
                                    what: sync.what,
                                    kind: sync.kind,
                                    statements: sync.statements,
                                },
                            });
                        } else {
                            // a new table has no rows to scan
                            phase2.extend(sync.statements.into_iter().map(Work::Sql));
                        }
                    }
                    TableObjectKind::TypedIndex(index) => {
                        use crate::schema_index::TypedIndexPlan;
                        let predicate = self
                            .shared
                            .exclusions
                            .index_filter(spec.id, &obj.full_name, &index)?
                            .map(str::to_owned);
                        let existing = indexes.and_then(|i| i.get(&obj.full_name));
                        let existing_struct = cat
                            .index_structs
                            .get(&spec.table)
                            .and_then(|d| d.get(&obj.full_name));
                        let needs_canonical = match existing {
                            Some(Some(comment)) => !comment.starts_with("INDEX USING "),
                            Some(None) => true,
                            None => false,
                        };
                        let canonical_predicate = if needs_canonical
                            && existing_struct.is_some()
                            && let Some(pred) = predicate.as_deref()
                        {
                            Some(
                                self.canonical_predicate(&spec.table, &obj.full_name, pred)
                                    .await?,
                            )
                        } else {
                            None
                        };
                        let plan = crate::schema_index::typed_index_plan(
                            &crate::schema_index::TypedIndexInput {
                                model: spec.name,
                                declared: obj.name,
                                runtime: obj.runtime,
                                table: &spec.table,
                                full_name: &obj.full_name,
                                index: &index,
                                predicate: predicate.as_deref(),
                                canonical_predicate: canonical_predicate.as_deref(),
                                unaccent,
                                existing,
                                existing_struct,
                                is_translated: &|name| {
                                    spec.field(name).is_some_and(|f| f.translate)
                                },
                            },
                        )?;
                        match plan {
                            TypedIndexPlan::Keep => {}
                            // the index is the declared one without the ORM's comment (a build
                            // that stopped before the comment): only the comment is missing
                            TypedIndexPlan::CommentOnly { comment } => {
                                phase2.push(Work::Sql(comment));
                            }
                            TypedIndexPlan::Build {
                                create,
                                comment,
                                replaced,
                            } => phase2.push(index_work(
                                &cat,
                                &spec.table,
                                existed,
                                &obj.full_name,
                                create,
                                Some(comment),
                                replaced,
                            )),
                        }
                    }
                }
            }
        }
        // the row-version bump function and trigger of versioned tables:
        // no scan (`CREATE TRIGGER` takes a short `SHARE ROW EXCLUSIVE`), so in the transaction
        // whatever the table size
        phase2.extend(row_version_ddl(specs, &cat).into_iter().map(Work::Sql));

        // Phase 3: field indexes (CRUD §4.10 `check_indexes`; every many2one except the audit
        // fields is indexed by default).
        // (the extensions they need were checked before phase 1)
        let mut phase3: Vec<Work> = Vec::new();
        for spec in specs {
            if cat.views.contains(&spec.table) {
                continue;
            }
            let existed = cat.tables.contains(&spec.table);
            let indexes = cat.indexes.get(&spec.table);
            let shapes = cat.index_shapes.get(&spec.table);
            for field in spec.column_fields() {
                let Some(kind) = field.index else { continue };
                if spec.parent_path == Some(field.id) {
                    // byte order, for the prefix ranges of the parent store (CRUD §4.11)
                    let name = index_name(&spec.table, "parent_path_c");
                    if !indexes.is_some_and(|i| i.contains_key(&name)) {
                        let create =
                            erp_sql::relational::create_parent_path_index(&spec.table, &name);
                        phase3.push(index_work(
                            &cat,
                            &spec.table,
                            existed,
                            &name,
                            create,
                            None,
                            false,
                        ));
                    }
                    continue;
                }
                let name = index_name(&spec.table, field.name);
                // an index of that name with another method, predicate or operator class is
                // rebuilt; a matching one is kept
                // (an invalid index is not in `indexes`: `index_work` builds it again)
                let mut replaced = false;
                if indexes.is_some_and(|i| i.contains_key(&name)) {
                    // a field index is never unique: a unique index under its name enforces a
                    // rule, and rebuilding it non-unique would drop that rule silently
                    if cat.unique_indexes.contains(&name) {
                        return Err(Error::programming(format!(
                            "index {name} on table {} is unique, but that is the generated name of \
                             the index of field {}.{}, which is never unique; it is not rebuilt (a \
                             uniqueness rule would be lost). Rename the unique index or drop it",
                            spec.table, spec.name, field.name
                        )));
                    }
                    match shapes.and_then(|s| s.get(&name)) {
                        Some(shape) if shape.matches(kind) => {
                            if kind == IndexKind::Trigram {
                                let struct_matches = cat
                                    .index_structs
                                    .get(&spec.table)
                                    .and_then(|s| s.get(&name))
                                    .is_some_and(|st| {
                                        let has_expr =
                                            st.keys.first().is_some_and(|k| k == "(expr)");
                                        if field.translate {
                                            if !has_expr {
                                                return false;
                                            }
                                            st.key_exprs
                                                .as_ref()
                                                .and_then(|exprs| exprs.first())
                                                .is_some_and(|expr| {
                                                    crate::schema_index::matches_trigram_key(
                                                        expr, field.name, true,
                                                    )
                                                })
                                        } else if unaccent == Unaccent::Indexable {
                                            if !has_expr {
                                                return false;
                                            }
                                            st.key_exprs
                                                .as_ref()
                                                .and_then(|exprs| exprs.first())
                                                .is_some_and(|expr| {
                                                    crate::schema_index::matches_trigram_key(
                                                        expr, field.name, false,
                                                    )
                                                })
                                        } else {
                                            st.keys.first().is_some_and(|k| k == field.name)
                                        }
                                    });
                                if struct_matches {
                                    continue;
                                }
                                replaced = true;
                            } else {
                                continue;
                            }
                        }
                        Some(_) => replaced = true,
                        // listed without a shape row: the catalog query gives every valid index one
                        None => {
                            return Err(Error::programming(format!(
                                "index {name} of {}: the catalog listed it without its shape",
                                spec.table
                            )));
                        }
                    }
                }
                let create = ddl::create_field_index_translated(
                    &spec.table,
                    field.name,
                    &name,
                    kind,
                    field.translate,
                    |extension| cat.extensions.contains(extension),
                    unaccent,
                )
                .map_err(|e| {
                    crate::schema_index::missing_field_extension(
                        spec.name,
                        field.name,
                        kind,
                        e.extension,
                    )
                })?;
                phase3.push(index_work(
                    &cat,
                    &spec.table,
                    existed,
                    &name,
                    create,
                    None,
                    replaced,
                ));
            }
        }

        // Phase 4: foreign keys, compared with the declaration.
        let mut phase4: Vec<Work> = Vec::new();
        let mut keys: Vec<(String, &str, String, OnDelete)> = Vec::new();
        for spec in specs {
            if cat.views.contains(&spec.table) {
                continue;
            }
            for field in spec.column_fields() {
                if field.company_dependent {
                    continue;
                }
                let FieldType::Many2one { comodel, ondelete } = &field.ty else {
                    continue;
                };
                let Some(target) = registry.model(comodel).filter(|m| m.has_table()) else {
                    continue;
                };
                keys.push((
                    spec.table.clone(),
                    field.name,
                    target.table.clone(),
                    *ondelete,
                ));
            }
        }
        for ((table, column), (target, ondelete, _)) in &relations.keys {
            keys.push((table.clone(), column, target.clone(), *ondelete));
        }
        if let Some(only) = replan {
            keys.retain(|(table, ..)| table == only.table);
        }
        for (table, column, target, ondelete) in keys {
            let existed = cat.tables.contains(&table);
            let wanted = format!("f:{column}");
            let mut current: Vec<(&String, &ConstraintInfo)> = cat
                .constraints
                .get(&table)
                .map(|c| c.iter().filter(|(_, (_, _, k))| *k == wanted).collect())
                .unwrap_or_default();
            current.sort();
            if let Some((name, (def, _, _))) = current
                .iter()
                .find(|(_, (def, _, _))| fk_matches(def, column, &target, ondelete))
            {
                // added `NOT VALID` by an earlier synchronization whose validation never ran
                if def.ends_with(" NOT VALID") {
                    phase4.push(Work::Table {
                        table: table.clone(),
                        work: TableWork::ForeignKey {
                            name: (*name).clone(),
                            add: None,
                        },
                    });
                }
                continue;
            }
            for (name, _) in &current {
                phase4.push(Work::Sql(ddl::drop_constraint(&table, name)));
            }
            if existed {
                phase4.push(Work::Table {
                    table: table.clone(),
                    work: TableWork::ForeignKey {
                        name: ddl::foreign_key_name(&table, column),
                        add: Some(ddl::add_foreign_key_not_valid(
                            &table, column, &target, ondelete,
                        )),
                    },
                });
            } else {
                phase4.extend(
                    ddl::add_foreign_key(&table, column, &target, ondelete, false)
                        .into_iter()
                        .map(Work::Sql),
                );
            }
        }

        // Phase 5: signal triggers, once every table exists.
        let phase5 = if replan.is_some() {
            Vec::new()
        } else {
            plan_signal_triggers(specs, registry, &cat)?
        };
        // a signal table of an older shape (its `key` check refuses `membership`) is upgraded on
        // every sync path, before the triggers that write the new key; planning one step again
        // runs nothing, so it plans no upgrade either
        let signal_upgrade = if replan.is_some() {
            Vec::new()
        } else {
            crate::signal::upgrade_statements(cat.constraints.get(crate::signal::SIGNAL_TABLE))
        };

        // Which tables with work that scans them are large (one query).
        let mut candidates: Vec<String> = phase1
            .iter()
            .chain(&phase2)
            .chain(&phase3)
            .chain(&phase4)
            .filter_map(|w| match w {
                Work::Table { table, .. } => Some(table.clone()),
                Work::Sql(_) => None,
            })
            .chain(backfills.iter().map(|b| b.table.clone()))
            .chain(pending_stored_checks.iter().map(|(_, _, t)| t.clone()))
            .chain(pending_collation_checks.iter().map(|(_, _, t)| t.clone()))
            .collect();
        candidates.sort();
        candidates.dedup();
        let large = match replan {
            Some(target) => std::iter::once(target.table.to_owned()).collect(),
            None => self.large_tables(&candidates, options.large_rows()).await?,
        };
        if options.large_rows().is_some() {
            for (model, field, table) in &pending_collation_checks {
                if large.contains(table) {
                    return Err(Error::programming(format!(
                        "field '{model}.{field}': collation change requires a maintenance window"
                    )));
                }
            }
            for (model, field, table) in &pending_stored_checks {
                if large.contains(table) {
                    return Err(Error::programming(format!(
                        "Generated field '{}.{}': rewrite requires a maintenance window",
                        model, field
                    )));
                }
            }
        }
        let mut plan = Plan {
            large: &large,
            options,
            steps: Vec::new(),
            changed: false,
            replan: replan.is_some(),
        };
        let (mut phase1, _) = plan.resolve(phase1)?;
        phase1.extend(stale_marks);
        let (phase2, conflicts2) = plan.resolve(phase2)?;
        let (phase3, conflicts3) = plan.resolve(phase3)?;
        let (phase4, conflicts4) = plan.resolve(phase4)?;
        let diagnostic = options.diagnostic_timeout_value();
        let diagnose = |conflicts: bool| conflicts.then_some(diagnostic);
        let mut inline_backfills = Vec::new();
        // marks of pending backfills that run inline: cleared once they have run
        let mut cleared_marks = Vec::new();
        for backfill in backfills {
            let PlannedBackfill {
                table,
                model,
                fields,
            } = backfill;
            if large.contains(&table) {
                // a new column (not marked yet) is a change; marked ones are re-planned
                plan.changed |= fields.iter().any(|f| !f.marked);
                // marked in this transaction, cleared by the step once all of its batches ran:
                // a backfill interrupted before that is planned again by the next sync
                phase1.extend(
                    fields
                        .iter()
                        .filter(|f| !f.marked)
                        .map(|f| ddl::comment_pending_backfill(&table, f.name, &f.label)),
                );
                plan.steps.push(SchemaStep {
                    key: format!("backfill.{table}"),
                    table,
                    action: StepAction::Backfill {
                        model,
                        fields: fields.iter().map(|f| f.name).collect(),
                    },
                });
            } else {
                cleared_marks.extend(
                    fields
                        .iter()
                        .filter(|f| f.marked)
                        .map(|f| ddl::comment_on_column(&table, f.name, &f.label)),
                );
                inline_backfills.push((table, model, fields.iter().map(|f| f.name).collect()));
            }
        }
        let changed_by_steps = plan.changed;
        let steps = plan.steps;
        if replan.is_some() {
            return Ok(steps);
        }
        // S0 of the addon-owned SQL objects: every refusal before any DDL
        let has_provenance = cat.tables.contains(crate::schema_sqlobj::OBJECT_TABLE);
        let mut sql_objects = self
            .plan_sql_objects(registry, has_provenance, specs, options, mode.sql_objects)
            .await?;
        sql_objects.refuse_integrity_recreation(partitions.recreated())?;
        // No B6 integrity is published with deferred work (is a later slice).
        for object in registry.sql_objects().objects() {
            if sql_objects.is_deferred(object.id()) {
                continue;
            }
            if let erp_registry::SqlObjectKind::ForeignKey(f) = object.kind() {
                let missing = !cat
                    .constraints
                    .get(f.model().table())
                    .is_some_and(|c| c.contains_key(f.name()));
                if missing
                    && (large.contains(f.model().table())
                        || steps.iter().any(|s| s.table == f.target().table()))
                {
                    return Err(Error::programming(format!(
                        "SQL object '{}': deferred integrity installation requires in a later release",
                        object.id()
                    )));
                }
            }
        }

        self.check_not_null(&not_null_checks).await?;
        self.check_reverse_translations(&reverse_translation_checks)
            .await?;
        self.check_company_jsonb(&company_jsonb_checks).await?;
        self.check_jsonb_validations(&jsonb_validations).await?;
        // S1: independent functions before the tables
        self.install_sql_objects_early(&mut sql_objects).await?;
        self.run_phase(phase1, None).await?;
        self.record_generated_provenance(&pending_provenance)
            .await?;
        for (table, col) in &pending_analyzes {
            self.execute(
                &format!(
                    "ANALYZE {} ({})",
                    erp_sql::quote(table),
                    erp_sql::quote(col)
                ),
                &[],
            )
            .await
            .map_err(|e| {
                Error::new(
                    erp_fields::ErrorKind::Database,
                    format!("ANALYZE on {table}.{col} failed"),
                )
                .with_source(e)
            })?;
        }
        let fresh: Vec<(&str, &str)> = fresh_translated
            .iter()
            .map(|(t, c)| (t.as_str(), c.as_str()))
            .collect();
        self.mark_jsonb_verified(&fresh).await?;
        if !window.is_empty() {
            self.run_window(window).await?;
        }
        for (table, sql) in fill_paths {
            self.execute(&sql, &[]).await?;
            // rows no root reaches (a cycle in existing data) would be left without a path
            let left = format!(
                "SELECT \"id\" FROM {} WHERE \"parent_path\" IS NULL ORDER BY \"id\" LIMIT 20",
                erp_sql::quote(&table)
            );
            let rows = self.query(&left, &[]).await?;
            if !rows.is_empty() {
                let ids =
                    rows.iter()
                        .map(|r| {
                            r.try_get::<_, i64>(0).map(|id| id.to_string()).map_err(|e| {
                            Error::programming(format!(
                                "{table}: reading the id of a row without a parent path: {e}"
                            ))
                        })
                        })
                        .collect::<Result<Vec<String>>>()?;
                return Err(Error::programming(format!(
                    "{table}: no root reaches the rows {} (a cycle in their parent links): fix the \
                     data, then update the schema again to build the parent store",
                    ids.join(", ")
                )));
            }
        }
        self.run_phase(phase2, diagnose(conflicts2)).await?;
        self.run_phase(phase3, diagnose(conflicts3)).await?;
        self.run_phase(phase4, diagnose(conflicts4)).await?;
        self.run_signal_upgrade(signal_upgrade).await?;
        // S2: catalog-derived bodies from the final columns (no later phase changes a column)
        self.install_sql_objects_derived(&mut sql_objects).await?;
        self.run_phase(phase5, None).await?;
        // S3: triggers, after the functions they execute
        self.install_sql_objects_triggers(&mut sql_objects).await?;
        self.install_sql_object_constraints(&mut sql_objects)
            .await?;
        // a batch larger than the address space is one batch
        let batch = usize::try_from(options.backfill_records().get()).unwrap_or(usize::MAX);
        self.backfill_inline(registry, &inline_backfills, batch)
            .await?;
        self.run_phase(cleared_marks, None).await?;
        // S4: provenance of what changed, in the same transaction
        self.record_sql_objects(&sql_objects).await?;
        // a change whose whole work went to steps (an index, a column made required with a
        // default) ran no DDL above: signal it all the same, so other processes take the new
        // registry before its steps run
        if changed_by_steps {
            self.signal_schema_change().await?;
        }
        Ok(steps)
    }
}

/// The stored columns of a view-backed model that the view does not expose.
fn missing_view_columns<'a>(
    wanted: impl Iterator<Item = &'a str>,
    existing: Option<&FxHashMap<String, ColumnInfo>>,
) -> Vec<&'a str> {
    wanted
        .filter(|name| existing.is_none_or(|e| !e.contains_key(*name)))
        .collect()
}

/// True when the model declares a trigger that writes the signal table (a signal trigger, or a
/// follow-delete trigger, which bumps a cache generation).
fn has_signal_trigger(spec: &ModelSpec) -> bool {
    spec.table_objects.iter().any(|o| {
        matches!(
            o.kind,
            TableObjectKind::SignalTrigger(_) | TableObjectKind::FollowDelete(_)
        )
    })
}

/// Phase 5: the signal table when it is missing, the bump function
/// of each signal the declarations use, then their statement-level triggers, each only when the
/// catalog lacks it or holds it with another definition (its comment), so a sync that changes
/// nothing sends no DDL. A signal trigger no declaration names any more is left in place: it can
/// only bump more often than needed. A follow-delete trigger (or its `_truncate` companion) that
/// no model of the registry declares any more is removed (review C3-R3): left in place it would
/// still delete key rows, and refuse `TRUNCATE`, for a declaration that is gone. Only triggers
/// carrying the follow-delete comment are removed, so a trigger written outside the ORM is never
/// touched; the registry-wide declarations decide, so a sync of some models never removes another
/// model's trigger on a shared table.
fn plan_signal_triggers(
    specs: &[&ModelSpec],
    registry: &erp_registry::Registry,
    cat: &Catalog,
) -> Result<Vec<String>> {
    use crate::signal::{bump_trigger, follow_delete};
    let mut signals: Vec<erp_registry::TriggerSignal> = Vec::new();
    let mut triggers: Vec<String> = follow_delete_orphans(registry, cat)?;
    let mut follows = false;
    for spec in specs {
        for obj in &spec.table_objects {
            if let TableObjectKind::FollowDelete(decl) = obj.kind {
                follows = true;
                let table = spec.follow_delete_table(&decl)?;
                let expected = follow_delete::trigger_comment(&decl, spec.name);
                let current = |name: &str| {
                    cat.triggers
                        .get(table)
                        .and_then(|t| t.get(name))
                        .and_then(Option::as_deref)
                };
                let truncate = follow_delete::truncate_trigger_name(&obj.full_name);
                if current(&obj.full_name) != Some(expected.as_str())
                    || current(&truncate) != Some(expected.as_str())
                {
                    triggers.push(follow_delete::create_trigger(
                        table,
                        &obj.full_name,
                        &decl,
                        spec.name,
                    ));
                }
                continue;
            }
            let TableObjectKind::SignalTrigger(decl) = obj.kind else {
                continue;
            };
            let table = spec.signal_trigger_table(&decl)?;
            if !signals.contains(&decl.signal) {
                signals.push(decl.signal);
            }
            let expected = bump_trigger::trigger_comment(&decl);
            let current = cat
                .triggers
                .get(table)
                .and_then(|t| t.get(&obj.full_name))
                .and_then(Option::as_deref);
            if current != Some(expected.as_str()) {
                triggers.push(bump_trigger::create_trigger(table, &obj.full_name, &decl));
            }
        }
    }
    if signals.is_empty() && !follows {
        // only orphaned follow-delete triggers (if any) to drop: no function, no schema needed
        // (review C4-R1)
        return Ok(triggers);
    }
    let schema = cat.schema.as_deref().ok_or_else(|| {
        Error::programming(
            "signal triggers need a schema to create their function in, but current_schema() is \
             NULL (no schema of the search_path exists)",
        )
    })?;
    let mut out = Vec::new();
    if !cat.tables.contains(crate::signal::SIGNAL_TABLE) {
        out.push(crate::signal::INSTALL_SQL.to_owned());
    }
    for signal in signals {
        let expected = bump_trigger::function_comment(signal, schema);
        let current = cat
            .functions
            .get(&bump_trigger::function_name(signal))
            .and_then(Option::as_deref);
        if current != Some(expected.as_str()) {
            out.push(bump_trigger::create_function(signal, schema));
        }
    }
    if follows {
        let expected = follow_delete::function_comment(schema);
        let current = cat
            .functions
            .get(follow_delete::FUNCTION)
            .and_then(Option::as_deref);
        if current != Some(expected.as_str()) {
            out.push(follow_delete::create_function(schema));
        }
    }
    out.extend(triggers);
    Ok(out)
}

/// The statements removing every follow-delete trigger of the catalog's tables (recognised by its
/// comment, [`crate::signal::follow_delete::TRIGGER_COMMENT_PREFIX`]) that no model of `registry`
/// declares, the `_truncate` companions included ([`plan_signal_triggers`]). Sorted, so the DDL a
/// sync sends does not depend on hash order.
fn follow_delete_orphans(registry: &erp_registry::Registry, cat: &Catalog) -> Result<Vec<String>> {
    use crate::signal::follow_delete;
    let mut declared: FxHashSet<(&str, String)> = FxHashSet::default();
    for spec in registry.models().iter().filter(|m| m.has_table()) {
        for obj in &spec.table_objects {
            if let TableObjectKind::FollowDelete(decl) = obj.kind {
                let table = spec.follow_delete_table(&decl)?;
                declared.insert((table, obj.full_name.clone()));
                declared.insert((table, follow_delete::truncate_trigger_name(&obj.full_name)));
            }
        }
    }
    let mut orphans: Vec<(&str, &str)> = cat
        .triggers
        .iter()
        .flat_map(|(table, by_name)| {
            by_name.iter().filter_map(move |(name, comment)| {
                comment
                    .as_deref()
                    .is_some_and(|c| c.starts_with(follow_delete::TRIGGER_COMMENT_PREFIX))
                    .then_some((table.as_str(), name.as_str()))
            })
        })
        .filter(|(table, name)| !declared.contains(&(*table, (*name).to_owned())))
        .collect();
    orphans.sort_unstable();
    Ok(orphans
        .into_iter()
        .map(|(table, name)| follow_delete::drop_trigger(table, name))
        .collect())
}

/// The work for an index `name` of `table`: plain SQL on a new table, else table work (built
/// concurrently by a step when the table is large). `replaced`: a valid index of that name with
/// another declaration exists and is dropped first.
fn index_work(
    cat: &Catalog,
    table: &str,
    existed: bool,
    name: &str,
    create: String,
    comment: Option<String>,
    replaced: bool,
) -> Work {
    if !existed {
        // a new table has no rows: nothing to build concurrently (and no comment-less form)
        return Work::Sql(match comment {
            Some(comment) => format!("{create};\n{comment}"),
            None => create,
        });
    }
    Work::Table {
        table: table.to_owned(),
        work: TableWork::Index {
            name: name.to_owned(),
            create,
            comment,
            replaced,
            invalid: cat.invalid_indexes.contains(name),
            partitioned: cat.partitioned.contains(table),
        },
    }
}

/// The name of the unvalidated not-null constraint of `table.column`.
fn not_null_constraint(cat: &Catalog, table: &str, column: &str) -> Result<String> {
    let wanted = format!("n:{column}");
    cat.constraints
        .get(table)
        .and_then(|c| c.iter().find(|(_, (_, _, k))| *k == wanted))
        .map(|(name, _)| name.clone())
        .ok_or_else(|| {
            Error::new(
                erp_fields::ErrorKind::Database,
                format!(
                    "column {table}.{column} has an unvalidated not-null constraint that the \
                     catalog does not list"
                ),
            )
        })
}

/// Ids of `table` with a NULL in one of `fields`, by id; with `page = (after, limit)` only ids
/// above `after`, at most `limit` of them.
pub(crate) async fn select_null_ids(
    tx: &mut Transaction,
    table: &str,
    fields: &[&str],
    page: Option<(i64, i64)>,
) -> Result<Vec<RecId>> {
    let nulls = fields
        .iter()
        .map(|f| format!("{} IS NULL", erp_sql::quote(f)))
        .collect::<Vec<_>>()
        .join(" OR ");
    let rows = match page {
        None => {
            let sql = format!(
                "SELECT \"id\" FROM {} WHERE {nulls} ORDER BY \"id\"",
                erp_sql::quote(table)
            );
            tx.query(&sql, &[]).await?
        }
        Some((after, limit)) => {
            let sql = format!(
                "SELECT \"id\" FROM {} WHERE \"id\" > $1 AND ({nulls}) ORDER BY \"id\" LIMIT $2",
                erp_sql::quote(table)
            );
            tx.query(&sql, &[&after, &limit]).await?
        }
    };
    rows.iter()
        .map(|row| {
            let raw: i64 = row.try_get(0).map_err(|e| {
                Error::new(
                    erp_fields::ErrorKind::Database,
                    "could not read a record id",
                )
                .with_source(e)
            })?;
            RecId::from_real(raw)
                .ok_or_else(|| Error::programming(format!("{table}: invalid record id {raw}")))
        })
        .collect()
}

/// Mark `ids` of `spec` to compute for `fields` (stored computed fields).
pub(crate) fn mark_to_compute(
    tx: &mut Transaction,
    spec: &ModelSpec,
    fields: &[&str],
    ids: &[RecId],
) -> Result<()> {
    // nothing to mark: an empty table (or a step whose rows are gone) has no record to report
    if ids.is_empty() {
        return Ok(());
    }
    tx.check_backfill_capture(spec, fields)?;
    for name in fields {
        let field = spec.field(name).ok_or_else(|| {
            Error::programming(format!("backfill of unknown field '{}.{name}'", spec.name))
        })?;
        let storage = field.storage();
        tx.cache
            .model_mut(spec.id)
            .mark_pending(field.id, storage, ids);
    }
    Ok(())
}

/// New (or still pending) stored computed columns of an existing table, to fill.
struct PlannedBackfill {
    table: String,
    model: &'static str,
    fields: Vec<BackfillField>,
}

struct BackfillField {
    name: &'static str,
    /// the column's comment once filled
    label: String,
    /// marked pending in the catalog by an earlier sync whose backfill did not finish
    marked: bool,
}

/// Planned DDL of one phase.
enum Work {
    /// Runs in the transaction.
    Sql(String),
    /// Scans an existing table: in the transaction when the table is small, else (mostly) a step.
    Table { table: String, work: TableWork },
}

enum TableWork {
    Index {
        name: String,
        create: String,
        comment: Option<String>,
        /// a valid index of this name with another declaration: dropped first
        replaced: bool,
        /// an invalid index of this name (an interrupted concurrent build): dropped first; on a
        /// partitioned table the unfinished parent index is kept, its build resumes
        invalid: bool,
        /// the table is partitioned: `CREATE INDEX CONCURRENTLY` is refused on it, so a large
        /// one is built `ON ONLY` the parent, per partition, then attached (design §7.10)
        partitioned: bool,
    },
    /// A `UNIQUE` or `PRIMARY KEY` constraint object on an existing partitioned table: no online
    /// build exists (`ADD CONSTRAINT … USING INDEX` is refused on a partitioned table), so it
    /// runs in the transaction on a small table only.
    PartitionedUnique {
        model: &'static str,
        constraint: String,
        statements: Vec<String>,
    },
    /// An exclusion or temporal key: no concurrent build, so never deferred (refused when large).
    Exclusion {
        model: &'static str,
        /// the declared object name, for the command in the refusal
        object: &'static str,
        what: String,
        kind: crate::exclusion::ObjectKind,
        statements: Vec<String>,
    },
    ForeignKey {
        name: String,
        /// the `NOT VALID` `ADD CONSTRAINT`; `None` when the key exists, not validated
        add: Option<String>,
    },
    NotNull {
        column: String,
        /// the field's default as an SQL literal, for the NULLs
        fill: Option<String>,
        /// the existing not-null constraint, added `NOT VALID` and not validated
        unvalidated: Option<String>,
    },
    /// A business key: `ADD CONSTRAINT … UNIQUE` builds its index with a full scan
    /// under `ACCESS EXCLUSIVE`, so it runs in the transaction on a small table only. On a large
    /// table a new key is two post-commit steps (`CREATE UNIQUE INDEX CONCURRENTLY`, then `ADD
    /// CONSTRAINT … USING INDEX`, design §11); a changed one is refused.
    Key {
        model: &'static str,
        /// the declared name (`_quant_key`)
        declared: String,
        /// the constraint name (also the name of the key's unique index)
        constraint: String,
        /// the key's columns, in the constraint's order
        columns: Vec<String>,
        nulls_distinct: bool,
        /// the canonical definition (the constraint's comment, the index's comment)
        definition: String,
        /// `ADD CONSTRAINT` and the comment holding the definition
        add: Vec<String>,
        /// a constraint of this name with another definition: dropped first
        replaced: bool,
        /// what an earlier synchronization left of the key's unique index
        index: KeyIndex,
    },
}

/// What the catalog holds of the unique index a business key is built on (it carries the key's
/// name; after the key is added it is the constraint's own index).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum KeyIndex {
    /// no index of the key's name
    Missing,
    /// an interrupted or failed `CREATE UNIQUE INDEX CONCURRENTLY`
    Invalid,
    /// a valid index of the declared shape with no comment (the build finished, the comment
    /// after it did not): step 1 only writes the comment
    Uncommented,
    /// a valid index of the declared shape built for this definition (its comment), not yet
    /// adopted by the constraint
    Built,
}

/// The error the conflict diagnostic's savepoint ends with, so a rollback that worked can be told
/// from one that failed.
#[derive(Debug)]
struct DiagnosticDone;

impl std::fmt::Display for DiagnosticDone {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("the conflict diagnostic finished")
    }
}

impl std::error::Error for DiagnosticDone {}

/// Resolves planned work into statements of the transaction and steps.
struct Plan<'a> {
    large: &'a FxHashSet<String>,
    options: &'a SchemaSyncOptions,
    steps: Vec<SchemaStep>,
    /// A step was planned for a change: work the registry declares and the catalog shows nothing
    /// of (a new or replaced index, a new foreign key, a not-null constraint not added yet, a
    /// new stored computed column). Unfinished work of an earlier sync that the catalog does
    /// show (an invalid index, a `NOT VALID` constraint, a pending backfill mark) is not one,
    /// so re-planning it on a restart signals nothing. A step whose earlier run never started
    /// left nothing in the catalog and counts as a change again (one more bump, harmless).
    changed: bool,
    /// Planning one step again ([`Transaction::plan_schema_step`]): work that has no step form
    /// (exclusions, a unique constraint of a partitioned table, a business key whose declaration
    /// changed) is not this step's and is skipped instead of refused. A new business key has a
    /// step form (its two steps) and is planned as in a sync.
    replan: bool,
}

impl Plan<'_> {
    /// The statements of `works` for the transaction, and whether one of them can fail on
    /// existing rows that violate a business key or an exclusion (its phase then runs where a
    /// conflict diagnostic can follow, [`Transaction::run_phase`]).
    fn resolve(&mut self, works: Vec<Work>) -> Result<(Vec<String>, bool)> {
        let mut sql = Vec::new();
        let mut may_conflict = false;
        for work in works {
            match work {
                Work::Sql(statement) => sql.push(statement),
                Work::Table { table, work } => {
                    if self.large.contains(&table) {
                        self.defer(table, work, &mut sql)?;
                    } else {
                        may_conflict |=
                            matches!(work, TableWork::Key { .. } | TableWork::Exclusion { .. });
                        inline(&table, work, &mut sql);
                    }
                }
            }
        }
        Ok((sql, may_conflict))
    }

    /// The large-table form: catalog changes in `sql`, the scan as a step.
    fn defer(&mut self, table: String, work: TableWork, sql: &mut Vec<String>) -> Result<()> {
        let step = match work {
            TableWork::Exclusion {
                model,
                object,
                what,
                kind,
                ..
            } => {
                if self.replan {
                    return Ok(());
                }
                let limit = self.options.large_rows().ok_or_else(|| {
                    Error::programming(format!(
                        "table '{table}' is large but no large-table threshold is set"
                    ))
                })?;
                return Err(crate::exclusion::large_table_refusal(
                    model, &table, object, &what, kind, limit,
                ));
            }
            TableWork::PartitionedUnique {
                model, constraint, ..
            } => {
                if self.replan {
                    return Ok(());
                }
                let rows = self.options.large_rows().ok_or_else(|| {
                    Error::programming(format!(
                        "table '{table}' was found large without a large_table_rows threshold"
                    ))
                })?;
                return Err(large_partitioned_unique_refusal(
                    model,
                    &constraint,
                    &table,
                    rows,
                ));
            }
            TableWork::Index {
                name,
                create,
                comment,
                replaced,
                invalid,
                partitioned: true,
            } => {
                // an invalid parent index is the unfinished build of an earlier sync: the step
                // resumes it (the partition indexes already attached stay)
                self.changed |= !invalid;
                if replaced {
                    sql.push(ddl::drop_index(&name));
                }
                // the step is the whole build: `ON ONLY` parent, one concurrent index per
                // partition, attach (checked here so a statement it cannot derive fails the sync)
                if ddl::create_index_on_only(&create, &name, &table).is_none() {
                    return Err(Error::programming(format!(
                        "index {name}: no ON ONLY form of {create}"
                    )));
                }
                SchemaStep {
                    key: format!("index.{name}"),
                    table,
                    action: StepAction::PartitionedIndex {
                        name,
                        statement: create,
                        comment,
                    },
                }
            }
            TableWork::Index {
                name,
                create,
                comment,
                replaced,
                invalid,
                partitioned: false,
            } => {
                // an invalid index is an interrupted build of an earlier sync
                self.changed |= !invalid;
                // nothing is dropped in the transaction (a plain `DROP INDEX` takes ACCESS
                // EXCLUSIVE on the table until the install commits): when replacing an index,
                // the step builds the new index concurrently under a temporary name
                // (`swap_index_name`), then drops the old and renames the new in one short
                // transaction; for a new or invalid index it builds under `name` concurrently.
                let statement = if replaced {
                    let temp_name = ddl::swap_index_name(&name);
                    ddl::create_swap_index_concurrently(&create, &name, &temp_name).ok_or_else(
                        || {
                            Error::programming(format!(
                                "index {name}: no swap concurrent form of {create}"
                            ))
                        },
                    )?
                } else {
                    ddl::create_index_concurrently(&create).ok_or_else(|| {
                        Error::programming(format!("index {name}: no concurrent form of {create}"))
                    })?
                };
                SchemaStep {
                    key: format!("index.{name}"),
                    table,
                    action: StepAction::Index {
                        name,
                        statement,
                        comment,
                        replaces: replaced,
                    },
                }
            }
            TableWork::ForeignKey { name, add } => {
                // without `add` the key exists, `NOT VALID`, from an earlier sync
                self.changed |= add.is_some();
                sql.extend(add);
                SchemaStep {
                    key: format!("validate.{name}"),
                    table,
                    action: StepAction::ValidateForeignKey { name },
                }
            }
            TableWork::NotNull {
                column,
                fill,
                unvalidated,
            } => {
                // an unvalidated constraint was added by an earlier sync
                self.changed |= unvalidated.is_none();
                let constraint = match (unvalidated, &fill) {
                    (Some(name), _) => name,
                    // without a default the check refused NULLs: enforce at once, validate later
                    (None, None) => {
                        let name = ddl::not_null_constraint_name(&table, &column);
                        sql.push(ddl::add_not_null_not_valid(&table, &column, &name));
                        name
                    }
                    // with a default the step fills first, then adds the constraint
                    (None, Some(_)) => ddl::not_null_constraint_name(&table, &column),
                };
                SchemaStep {
                    key: format!("not_null.{table}.{column}"),
                    table,
                    action: StepAction::NotNull {
                        column,
                        constraint,
                        fill,
                    },
                }
            }
            TableWork::Key {
                model,
                declared,
                constraint,
                columns,
                nulls_distinct,
                definition,
                replaced,
                index,
                ..
            } => {
                if replaced {
                    // a changed key has no step form: planning one step again skips it (it is
                    // not that step's work) instead of refusing
                    if self.replan {
                        return Ok(());
                    }
                    let rows = self.options.large_rows().ok_or_else(|| {
                        Error::programming(format!(
                            "table '{table}' was found large without a large_table_rows threshold"
                        ))
                    })?;
                    return Err(large_key_change_refusal(
                        model,
                        &declared,
                        &constraint,
                        &table,
                        rows,
                    ));
                }
                // nothing in the transaction: the key is added after the commit, in two steps.
                // An invalid or valid-and-built index is the work of an earlier sync, not a change.
                self.changed |= matches!(index, KeyIndex::Missing);
                let build = crate::schema_steps::KeyBuild {
                    model,
                    declared,
                    name: constraint.clone(),
                    columns,
                    nulls_distinct,
                    definition,
                };
                if !matches!(index, KeyIndex::Built) {
                    self.steps.push(SchemaStep {
                        key: build.index_step_key(),
                        table: table.clone(),
                        action: StepAction::KeyIndex(build.clone()),
                    });
                }
                SchemaStep {
                    key: build.constraint_step_key(),
                    table,
                    action: StepAction::KeyConstraint(build),
                }
            }
        };
        self.steps.push(step);
        Ok(())
    }
}

/// The small-table form: everything in the transaction.
fn inline(table: &str, work: TableWork, sql: &mut Vec<String>) {
    match work {
        TableWork::Exclusion { statements, .. } => sql.extend(statements),
        TableWork::PartitionedUnique { statements, .. } => sql.extend(statements),
        TableWork::Index {
            name,
            create,
            comment,
            replaced,
            invalid,
            ..
        } => {
            if replaced || invalid {
                sql.push(ddl::drop_index(&name));
            }
            sql.push(create);
            sql.extend(comment);
        }
        TableWork::ForeignKey { name, add } => {
            sql.extend(add);
            sql.push(ddl::validate_constraint(table, &name));
        }
        TableWork::NotNull {
            column,
            fill,
            unvalidated,
        } => {
            if let Some(lit) = fill {
                sql.push(ddl::fill_nulls(table, &column, &lit));
            }
            match unvalidated {
                Some(name) => sql.push(ddl::validate_constraint(table, &name)),
                None => sql.push(ddl::set_not_null(table, &column)),
            }
        }
        TableWork::Key {
            model,
            declared,
            constraint,
            definition,
            add,
            replaced,
            index,
            ..
        } => {
            if replaced {
                sql.push(ddl::drop_constraint(table, &constraint));
                sql.extend(add);
                return;
            }
            match index {
                KeyIndex::Missing => sql.extend(add),
                // an interrupted concurrent build of an earlier sync, on a table that is small
                // now: not while the step that builds it is still running (it would lose its
                // build), checked by the statement itself with the step's advisory lock
                KeyIndex::Invalid => {
                    sql.push(ddl::guard_key_index_build(
                        &crate::schema_steps::key_index_step_key(&constraint),
                        &format!(
                            "cannot replace the invalid index '{constraint}' of key '{declared}' of \
                             '{model}': its post-commit build step is running (the per-step lock \
                             is held); wait for it to finish and run the schema sync again"
                        ),
                    ));
                    sql.push(ddl::drop_index(&constraint));
                    sql.extend(add);
                }
                // built by an earlier sync's step 1: adopt it, no second scan
                KeyIndex::Uncommented | KeyIndex::Built => {
                    sql.extend(ddl::add_constraint_using_index(
                        table,
                        &constraint,
                        &constraint,
                        &definition,
                    ));
                }
            }
        }
    }
}

/// The install-path refusal of a `UNIQUE` or `PRIMARY KEY` constraint on a partitioned table
/// above `large_table_rows` (design §7.10): `Programming`, like the other install-path refusals
/// of a declaration the host cannot build online. A unique index built partition by partition is
/// the supported online form.
fn large_partitioned_unique_refusal(
    model: &str,
    constraint: &str,
    table: &str,
    rows: u64,
) -> Error {
    Error::programming(format!(
        "cannot add or change unique constraint '{constraint}' of '{model}' during the install: \
         partitioned table '{table}' has more than {rows} rows (large_table_rows), ADD CONSTRAINT \
         would build an index on every partition under ACCESS EXCLUSIVE, and PostgreSQL has no \
         ADD CONSTRAINT … USING INDEX on a partitioned table, so no online build exists; declare \
         a unique index instead (built partition by partition after the commit)"
    ))
}

/// The install-path refusal of a *changed* business key on a table above `large_table_rows`
/// (design §11.1): `UserError`. A new key is built after the commit; replacing one has no safe
/// online form, since the old constraint would have to go (the key unenforced) or stay (upserts
/// naming the key would arbitrate on its old definition) while the new index is built.
fn large_key_change_refusal(
    model: &str,
    declared: &str,
    constraint: &str,
    table: &str,
    rows: u64,
) -> Error {
    Error::user(format!(
        "cannot change business key '{declared}' of '{model}' during the install: table \
         '{table}' has more than {rows} rows (large_table_rows), and a changed key has no online \
         replacement: building the new unique index under ACCESS EXCLUSIVE scans the whole \
         table, and keeping the old constraint while it is built would leave upserts naming the \
         key on its old definition. Drop the old constraint in a maintenance window \
         (ALTER TABLE \"{table}\" DROP CONSTRAINT \"{constraint}\"; upserts naming the key fail \
         until the new one is built) and run the schema sync again: it adds the new key in two \
         steps after the commit"
    ))
}

// ----: business keys and row versions (D3) ------------------------------

/// The work for a business key: added when missing, re-created when its stored definition
/// differs (the definition is kept as the constraint's comment, CRUD §4.9). Plain SQL on a new
/// table; table work on an existing one, because `ADD CONSTRAINT … UNIQUE` builds the key's index
/// with a full scan under `ACCESS EXCLUSIVE`: in the transaction on a small table, refused on a
/// large one ([`Plan::defer`]).
fn key_work(
    spec: &ModelSpec,
    full_name: &str,
    key: &erp_registry::KeyDecl,
    existed: bool,
    cat: &Catalog,
) -> Result<Vec<Work>> {
    let definition = erp_sql::upsert::key_definition(key.fields, key.nulls_distinct);
    let constraints = cat.constraints.get(&spec.table);
    let replaced = match constraints.and_then(|c| c.get(full_name)) {
        None => false,
        // up to date: the comment holds the declaration and the catalog agrees (review D2: a
        // constraint changed by hand keeps the comment)
        Some((def, comment, kind))
            if comment.as_deref() == Some(definition.as_str())
                && key_constraint_matches(def, kind, &definition) =>
        {
            return Ok(Vec::new());
        }
        Some(_) => true,
    };
    let add = ddl::add_constraint(&spec.table, full_name, &definition);
    if !existed {
        // a new table has no rows and no constraint to replace
        return Ok(add.into_iter().map(Work::Sql).collect());
    }
    // the registry composes every key table object into a business key of the same full name
    let declared = spec
        .business_keys()
        .iter()
        .find(|k| k.full_name == full_name)
        .ok_or_else(|| {
            Error::programming(format!(
                "table object '{full_name}' of '{}' is a key the registry did not compose",
                spec.name
            ))
        })?
        .name;
    let index = if cat.invalid_indexes.contains(full_name) {
        KeyIndex::Invalid
    } else {
        match cat.indexes.get(&spec.table).and_then(|i| i.get(full_name)) {
            None => KeyIndex::Missing,
            Some(comment) => {
                // a valid index of this name is adopted only when it is exactly the key's unique
                // index (review DB1); anything else is somebody else's and is never replaced
                let shaped = cat.index_defs.get(full_name).is_some_and(|def| {
                    ddl::key_index_matches(
                        def,
                        cat.schema.as_deref(),
                        &spec.table,
                        full_name,
                        key.fields,
                        key.nulls_distinct,
                    )
                });
                match comment.as_deref() {
                    Some(c) if shaped && c == definition => KeyIndex::Built,
                    None if shaped => KeyIndex::Uncommented,
                    _ if replaced => KeyIndex::Missing,
                    _ => {
                        return Err(Error::programming(format!(
                            "cannot add key '{declared}' to '{}': an index named '{full_name}' \
                             exists that is not this key's unique index (another shape, columns, \
                             comment or uniqueness); it is not replaced, drop it or rename the key",
                            spec.name
                        )));
                    }
                }
            }
        }
    };
    Ok(vec![Work::Table {
        table: spec.table.clone(),
        work: TableWork::Key {
            model: spec.name,
            declared: declared.to_owned(),
            constraint: full_name.to_owned(),
            columns: key.fields.iter().map(|f| (*f).to_owned()).collect(),
            nulls_distinct: key.nulls_distinct,
            definition,
            add,
            replaced,
            index,
        },
    }])
}

/// Whether the catalog's own account of a key constraint (`pg_get_constraintdef`, and
/// `contype:columns`) agrees with the canonical `definition`: a unique constraint whose text is
/// the definition's, double quotes aside (the catalog quotes only where needed). A constraint
/// that was hand-replaced under the same name and comment is not up to date (review D2).
fn key_constraint_matches(catalog_def: &str, kind_cols: &str, definition: &str) -> bool {
    kind_cols.starts_with('u') && unquoted(catalog_def) == unquoted(definition)
}

fn unquoted(definition: &str) -> String {
    definition.replace('"', "")
}

/// The shared bump function (when missing or changed) and the trigger of every versioned table
/// (when missing or changed): every `UPDATE` of a versioned row bumps `row_version` by one,
/// whoever sends it (design note §4.2).
fn row_version_ddl(specs: &[&ModelSpec], cat: &Catalog) -> Vec<String> {
    use erp_sql::version::{
        BUMP_FUNCTION, BUMP_FUNCTION_COMMENT, TRIGGER_DEFINITION, bump_function, bump_trigger,
        trigger_name,
    };
    let versioned: Vec<&&ModelSpec> = specs.iter().filter(|s| s.row_version.is_some()).collect();
    if versioned.is_empty() {
        return Vec::new();
    }
    let mut out = Vec::new();
    if cat.functions.get(BUMP_FUNCTION).map(Option::as_deref) != Some(Some(BUMP_FUNCTION_COMMENT)) {
        out.extend(bump_function());
    }
    for spec in versioned {
        // the database default 1 stays on the row version (review DB-S4): raw INSERTs that leave
        // the column out work, and a column added with the fast default keeps it
        if !cat
            .defaults
            .contains_key(&(spec.table.clone(), erp_registry::ROW_VERSION.to_owned()))
        {
            out.push(erp_sql::version::set_default(&spec.table));
        }
        let name = trigger_name(&spec.table);
        let current = cat.triggers.get(&spec.table).and_then(|t| t.get(&name));
        match current {
            None => out.extend(bump_trigger(&spec.table, false)),
            Some(comment) if comment.as_deref() == Some(TRIGGER_DEFINITION) => {}
            Some(_) => out.extend(bump_trigger(&spec.table, true)),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_view_missing_a_model_column_is_reported_by_name() {
        let mut view: FxHashMap<String, ColumnInfo> = FxHashMap::default();
        view.insert("id".to_owned(), ("int8".to_owned(), None, NotNull::Yes));
        let missing = missing_view_columns(["id", "name", "kind"].into_iter(), Some(&view));
        assert_eq!(missing, ["name", "kind"]);
        assert!(missing_view_columns(["id"].into_iter(), Some(&view)).is_empty());
    }

    /// Review C4-R1: with no follow-delete (and no signal) declaration left in the registry, the
    /// orphaned follow-delete trigger and its `_truncate` companion are still removed, without a
    /// schema (no function is created for them); once they are gone the plan is empty.
    #[test]
    fn orphaned_follow_triggers_are_dropped_without_any_declaration() {
        use erp_registry::{AddonDescriptor, FieldDecl, ModelDecl, RegistryBuilder};
        static T: AddonDescriptor = AddonDescriptor {
            name: "t_orphan",
            depends: &["base"],
            exact_only: false,
        };
        static M: ModelDecl =
            ModelDecl::new("t_orphan", "t.orphan").fields(const { &[FieldDecl::char("name")] });
        let registry = RegistryBuilder::new([&T]).with_decls([&M]).build().unwrap();
        let specs: Vec<&ModelSpec> = registry.models().iter().collect();
        assert!(!specs.iter().any(|spec| has_signal_trigger(spec)));
        let table = registry.model("t.orphan").unwrap().table.clone();
        let comment = format!(
            "{}v1: keys -, cache gone, type t.orphan",
            crate::signal::follow_delete::TRIGGER_COMMENT_PREFIX
        );
        let mut cat = Catalog::default();
        let on_table = cat.triggers.entry(table.clone()).or_default();
        on_table.insert("gone_follow".to_owned(), Some(comment.clone()));
        on_table.insert("gone_follow_truncate".to_owned(), Some(comment));
        on_table.insert("someone_elses".to_owned(), None);
        let planned = plan_signal_triggers(&specs, &registry, &cat).unwrap();
        assert_eq!(
            planned,
            vec![
                crate::signal::follow_delete::drop_trigger(&table, "gone_follow"),
                crate::signal::follow_delete::drop_trigger(&table, "gone_follow_truncate"),
            ]
        );
        cat.triggers
            .get_mut(&table)
            .unwrap()
            .retain(|name, _| name == "someone_elses");
        assert_eq!(
            plan_signal_triggers(&specs, &registry, &cat).unwrap(),
            Vec::<String>::new()
        );
    }

    /// A static default reads as JSON exactly as the dynamic converter writes the same value,
    /// for every variant (the match below breaks the build when a variant is added).
    #[test]
    fn static_json_matches_value_to_json() {
        let samples = [
            StaticValue::False,
            StaticValue::Bool(true),
            StaticValue::Bool(false),
            StaticValue::Int(i64::MIN),
            StaticValue::Int(0),
            StaticValue::Float(1.5),
            StaticValue::Float(-0.0),
            StaticValue::Float(f64::NAN),
            StaticValue::Float(f64::INFINITY),
            StaticValue::Str(""),
            StaticValue::Str("é \"q\""),
        ];
        for value in samples {
            match value {
                StaticValue::False
                | StaticValue::Bool(_)
                | StaticValue::Int(_)
                | StaticValue::Float(_)
                | StaticValue::Str(_) => {}
            }
            assert_eq!(
                static_json(&value),
                erp_fields::convert::value_to_json(&value.to_value()).unwrap(),
                "{value:?}"
            );
        }
        assert_eq!(
            static_json(&StaticValue::Float(f64::NAN)),
            erp_fields::serde_json::Value::Null
        );
    }

    /// The duration constants say what the setting strings say.
    #[test]
    fn timeout_durations_match_their_settings() {
        assert_eq!(
            SCHEMA_LOCK_TIMEOUT,
            format!("{}s", SCHEMA_LOCK_TIMEOUT_DURATION.as_secs())
        );
        assert_eq!(
            SCHEMA_STATEMENT_TIMEOUT,
            format!("{}min", SCHEMA_STATEMENT_TIMEOUT_DURATION.as_secs() / 60)
        );
    }

    #[test]
    fn statement_timeout_text_and_limit_agree() {
        assert_eq!(SCHEMA_STATEMENT_TIMEOUT, "5min");
        assert_eq!(SCHEMA_STATEMENT_TIMEOUT_DURATION.as_secs(), 5 * 60);
    }

    #[test]
    fn foreign_key_definitions_are_compared() {
        let def = "FOREIGN KEY (create_uid) REFERENCES res_users(id) ON DELETE SET NULL";
        assert!(fk_matches(
            def,
            "create_uid",
            "res_users",
            OnDelete::SetNull
        ));
        assert!(!fk_matches(
            def,
            "create_uid",
            "res_users",
            OnDelete::Cascade
        ));
        assert!(!fk_matches(def, "create_uid", "other", OnDelete::SetNull));
        assert!(fk_matches(
            "FOREIGN KEY (a) REFERENCES t(id) ON DELETE RESTRICT NOT VALID",
            "a",
            "t",
            OnDelete::Restrict
        ));
    }
}

#[cfg(test)]
mod collation_tests {
    use super::*;
    #[test]
    fn field_collation_catalog_decoder_is_strict() {
        assert_eq!(
            decode_column_collation(r#"["odd.schema","quote\"name"]"#).unwrap(),
            "\"odd.schema\".\"quote\"\"name\""
        );
        for payload in [
            "null",
            "[]",
            r#"["a"]"#,
            r#"[1,"C"]"#,
            r#"["a","C",0]"#,
            r#"["","C"]"#,
        ] {
            let e = decode_column_collation(payload).unwrap_err();
            assert_eq!(e.kind(), erp_fields::ErrorKind::Database);
            assert_eq!(
                e.message(),
                if payload == r#"["","C"]"# {
                    "invalid column collation identity"
                } else {
                    "unreadable column collation identity"
                }
            );
        }
    }
}
