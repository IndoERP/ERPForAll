//! The actions a post-commit step can run.
//!
//! A step is never free SQL text: the queue row of `ir_module_post_commit` holds a structured
//! [`PostCommitAction`], and the statement is built at run time from identifiers that were
//! validated when the action was made and are validated again when the row is read back. A row
//! somebody edited in the table (the phase runs with the module system's database rights) that
//! no longer parses into an action is listed as invalid and never run.
//!
//! Adopted: the two operations addons really need after a commit. Modified:
//! the queue stores the operation, not its SQL text.

use erp_fields::{Error, Result};
use erp_orm::{SchemaStep, SchemaStepKey, SchemaStepKind};
use serde::Deserialize;

/// Longest name of a table, column or operator class (bytes): PostgreSQL's identifier limit.
pub const MAX_SQL_NAME_BYTES: usize = 63;

/// Most columns of one index step.
pub const MAX_INDEX_COLUMNS: usize = 32;

/// Longest stored `spec` of a step (bytes); the column has a `CHECK` on it too.
pub const MAX_ACTION_SPEC_BYTES: usize = 4096;

/// Check a lower-case SQL name: 1 to [`MAX_SQL_NAME_BYTES`] bytes of `a-z`, `0-9`, `_`, not
/// starting with a digit.
fn check_name(what: &str, name: &str) -> Result<()> {
    // `pg_` names are PostgreSQL's catalogs (`pg_class`, `pg_toast_...`): a step never works on
    // one, and `current_schema()` qualification would not reach them anyway.
    if matches!(what, "table" | "index") && name.starts_with("pg_") {
        return Err(Error::programming(format!(
            "invalid {what} name {name:?}: names starting with pg_ are reserved for PostgreSQL's \
             catalogs"
        )));
    }
    let ok = !name.is_empty()
        && name.len() <= MAX_SQL_NAME_BYTES
        && !name.as_bytes().first().is_some_and(u8::is_ascii_digit)
        && name
            .bytes()
            .all(|b| matches!(b, b'a'..=b'z' | b'0'..=b'9' | b'_'));
    if ok {
        Ok(())
    } else {
        Err(Error::programming(format!(
            "invalid {what} name {name:?}: 1 to {MAX_SQL_NAME_BYTES} bytes of a-z, 0-9, '_', not \
             starting with a digit"
        )))
    }
}

macro_rules! sql_name {
    ($(#[$doc:meta])* $name:ident, $what:literal) => {
        $(#[$doc])*
        #[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
        pub struct $name(String);

        impl $name {
            /// Check and wrap a name.
            ///
            /// # Errors
            /// `Programming` naming the value and the rule for an empty, longer or otherwise
            /// invalid name.
            pub fn parse(name: &str) -> Result<Self> {
                check_name($what, name)?;
                Ok(Self(name.to_owned()))
            }

            /// The name.
            pub fn as_str(&self) -> &str {
                &self.0
            }
        }

        impl AsRef<str> for $name {
            fn as_ref(&self) -> &str {
                &self.0
            }
        }

        impl std::fmt::Display for $name {
            fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                f.write_str(&self.0)
            }
        }
    };
}

sql_name!(
    /// The table a post-commit step works on: 1 to [`MAX_SQL_NAME_BYTES`] bytes of `a-z`, `0-9`,
    /// `_`, not starting with a digit, and not starting with `pg_` (PostgreSQL's catalogs). The
    /// table is looked up in the current schema (where the ORM creates its tables).
    ///
    /// ```
    /// use erp_modules::post_commit::TableName;
    /// assert_eq!(TableName::parse("sale_order").unwrap().as_str(), "sale_order");
    /// assert!(TableName::parse("sale order; drop").is_err());
    /// ```
    TableName,
    "table"
);
sql_name!(
    /// The name of the index an index step builds (same rule as [`TableName`], including the
    /// refusal of `pg_` names).
    ///
    /// ```
    /// use erp_modules::post_commit::IndexName;
    /// assert_eq!(IndexName::parse("sale_order_name_idx").unwrap().as_str(), "sale_order_name_idx");
    /// assert!(IndexName::parse("a-b").is_err());
    /// ```
    IndexName,
    "index"
);
sql_name!(
    /// A column of an index step (same rule as [`TableName`]).
    ColumnName,
    "column"
);
sql_name!(
    /// An operator class of an index column, e.g. `gin_trgm_ops` (same rule as [`TableName`]).
    OpClassName,
    "operator class"
);

/// One column of an index step.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IndexColumn {
    name: ColumnName,
    opclass: Option<OpClassName>,
}

impl IndexColumn {
    /// A plain column.
    ///
    /// # Errors
    /// `Programming` for a name outside the rule of [`ColumnName`].
    pub fn new(name: &str) -> Result<Self> {
        Ok(Self {
            name: ColumnName::parse(name)?,
            opclass: None,
        })
    }

    /// The column with an operator class (`gin_trgm_ops`, `text_pattern_ops`, ...).
    ///
    /// # Errors
    /// `Programming` for a name outside the rule of [`OpClassName`].
    pub fn with_opclass(mut self, opclass: &str) -> Result<Self> {
        self.opclass = Some(OpClassName::parse(opclass)?);
        Ok(self)
    }

    /// The column.
    pub fn name(&self) -> &ColumnName {
        &self.name
    }

    /// The operator class, when one was given.
    pub fn opclass(&self) -> Option<&OpClassName> {
        self.opclass.as_ref()
    }
}

/// The access method of an index step (`USING <method>`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
#[non_exhaustive]
pub enum IndexMethod {
    /// `btree`, PostgreSQL's default.
    #[default]
    Btree,
    /// `hash`.
    Hash,
    /// `gin`.
    Gin,
    /// `gist`.
    Gist,
    /// `brin`.
    Brin,
}

impl IndexMethod {
    /// The method's SQL keyword, also its stored name.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Btree => "btree",
            Self::Hash => "hash",
            Self::Gin => "gin",
            Self::Gist => "gist",
            Self::Brin => "brin",
        }
    }

    fn parse(name: &str) -> Option<Self> {
        [Self::Btree, Self::Hash, Self::Gin, Self::Gist, Self::Brin]
            .into_iter()
            .find(|m| m.as_str() == name)
    }
}

/// `CREATE [UNIQUE] INDEX CONCURRENTLY IF NOT EXISTS <index> ON <table> USING <method> (<columns>)`.
///
/// ```
/// use erp_modules::post_commit::{CreateIndex, IndexColumn, TableName};
/// use erp_modules::IndexName;
/// let index = CreateIndex::new(
///     TableName::parse("library_book")?,
///     IndexName::parse("library_book_name_idx")?,
///     vec![IndexColumn::new("name")?],
/// )?
/// .unique();
/// assert!(index.is_unique());
/// # Ok::<(), erp_fields::Error>(())
/// ```
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CreateIndex {
    table: TableName,
    index: IndexName,
    columns: Vec<IndexColumn>,
    unique: bool,
    method: IndexMethod,
}

impl CreateIndex {
    /// A b-tree index on `columns` (1 to [`MAX_INDEX_COLUMNS`]).
    ///
    /// # Errors
    /// `Programming` for no column or more than [`MAX_INDEX_COLUMNS`].
    pub fn new(table: TableName, index: IndexName, columns: Vec<IndexColumn>) -> Result<Self> {
        if columns.is_empty() || columns.len() > MAX_INDEX_COLUMNS {
            return Err(Error::programming(format!(
                "invalid index {index}: it needs 1 to {MAX_INDEX_COLUMNS} columns, not {}",
                columns.len()
            )));
        }
        Ok(Self {
            table,
            index,
            columns,
            unique: false,
            method: IndexMethod::default(),
        })
    }

    /// Make the index `UNIQUE`.
    #[must_use]
    pub fn unique(mut self) -> Self {
        self.unique = true;
        self
    }

    /// Use another access method.
    #[must_use]
    pub fn method(mut self, method: IndexMethod) -> Self {
        self.method = method;
        self
    }

    /// The table.
    pub fn table(&self) -> &TableName {
        &self.table
    }

    /// The index name.
    pub fn index(&self) -> &IndexName {
        &self.index
    }

    /// The columns.
    pub fn columns(&self) -> &[IndexColumn] {
        &self.columns
    }

    /// Whether the index is `UNIQUE`.
    pub fn is_unique(&self) -> bool {
        self.unique
    }

    /// The access method.
    pub fn index_method(&self) -> IndexMethod {
        self.method
    }
}

/// The names a queue row gives the kinds of schema step (`spec.work`), the one list both
/// directions use.
const WORKS: [(SchemaStepKind, &str); 6] = [
    (SchemaStepKind::CreateIndex, "create_index"),
    (SchemaStepKind::ValidateForeignKey, "validate_foreign_key"),
    (SchemaStepKind::NotNull, "not_null"),
    (SchemaStepKind::Backfill, "backfill"),
    // the two steps of a business key on a large table
    (SchemaStepKind::CreateKeyIndex, "create_key_index"),
    (SchemaStepKind::AddKeyConstraint, "add_key_constraint"),
];

/// Most fields a backfill row records.
pub const MAX_BACKFILL_FIELDS: usize = 64;

/// The stored name of a kind of schema step.
///
/// # Errors
/// `Programming` for a kind the queue does not know.
pub fn schema_work_name(kind: SchemaStepKind) -> Result<&'static str> {
    WORKS
        .iter()
        .find(|(k, _)| *k == kind)
        .map(|(_, name)| *name)
        .ok_or_else(|| {
            Error::programming(format!(
                "the schema step kind {kind:?} is not one the post-commit queue knows"
            ))
        })
}

/// A reference to the schema step of a large table that a module operation left for after its
/// commit: the table, the step's key (`erp_orm::SchemaStepKey`, whose prefix
/// is the kind of work) and, for a backfill, the stored computed fields it recorded. It holds
/// nothing executable. When the row runs, the module system asks the ORM to plan that one step
/// again from the current registry and the catalog (`Orm::plan_schema_step`), so a stored row can
/// only ever trigger work the registry declares.
///
/// ```
/// use erp_modules::post_commit::{SchemaStepRef, TableName};
/// let step = SchemaStepRef::new(
///     TableName::parse("sale_order")?,
///     "backfill.sale_order",
///     &["name_size"],
/// )?;
/// assert_eq!(step.key(), "backfill.sale_order");
/// assert!(SchemaStepRef::new(TableName::parse("sale_order")?, "x", &[]).is_err());
/// # Ok::<(), erp_fields::Error>(())
/// ```
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SchemaStepRef {
    key: SchemaStepKey,
    table: TableName,
    fields: Vec<ColumnName>,
    /// the stored name of the key's kind
    work: &'static str,
}

impl SchemaStepRef {
    /// Check and wrap a reference.
    ///
    /// # Errors
    /// `Programming` when the key is not a [`SchemaStepKey`], does not name `table` (a
    /// `not_null.` or `backfill.` key), when a backfill records no field, more than
    /// [`MAX_BACKFILL_FIELDS`] or a field that is not a column name, or another kind of step
    /// records any.
    pub fn new(table: TableName, key: &str, fields: &[&str]) -> Result<Self> {
        let fields: Vec<String> = fields.iter().map(|f| (*f).to_owned()).collect();
        Self::checked(None, table, key, &fields).map_err(Error::programming)
    }

    fn checked(
        work: Option<SchemaStepKind>,
        table: TableName,
        key: &str,
        fields: &[String],
    ) -> std::result::Result<Self, String> {
        let key = SchemaStepKey::parse(key).map_err(|e| e.message().to_owned())?;
        let work_name = |kind| schema_work_name(kind).map_err(|e| e.message().to_owned());
        if let Some(work) = work
            && work != key.kind()
        {
            return Err(format!(
                "schema step key {key} is a {} step, not {}",
                work_name(key.kind())?,
                work_name(work)?
            ));
        }
        let fits = match key.kind() {
            SchemaStepKind::Backfill | SchemaStepKind::NotNull => {
                key.table() == Some(table.as_str())
            }
            SchemaStepKind::CreateIndex
            | SchemaStepKind::ValidateForeignKey
            | SchemaStepKind::CreateKeyIndex
            | SchemaStepKind::AddKeyConstraint => true,
            other => {
                return Err(format!(
                    "the schema step kind {other:?} is not one the post-commit queue knows"
                ));
            }
        };
        if !fits {
            let (prefix, must) = if key.kind() == SchemaStepKind::Backfill {
                (format!("backfill.{table}"), "be")
            } else {
                (format!("not_null.{table}."), "start with")
            };
            return Err(format!(
                "schema step key {key:?} does not fit a {} step on table {table}: it must {must} \
                 {prefix:?}",
                work_name(key.kind())?,
                key = key.as_str(),
            ));
        }
        let backfill = key.kind() == SchemaStepKind::Backfill;
        if backfill && (fields.is_empty() || fields.len() > MAX_BACKFILL_FIELDS) {
            return Err(format!(
                "a backfill step records 1 to {MAX_BACKFILL_FIELDS} fields, not {}",
                fields.len()
            ));
        }
        if !backfill && !fields.is_empty() {
            return Err(format!(
                "only a backfill step records fields, not a {} step",
                work_name(key.kind())?
            ));
        }
        let fields = fields
            .iter()
            .map(|f| ColumnName::parse(f).map_err(|e| e.message().to_owned()))
            .collect::<std::result::Result<Vec<_>, _>>()?;
        let work = work_name(key.kind())?;
        Ok(Self {
            key,
            table,
            fields,
            work,
        })
    }

    /// The reference of an ORM step.
    ///
    /// # Errors
    /// `Programming` when the step's table or key does not pass the rules of the queue.
    pub fn from_step(step: &SchemaStep) -> Result<Self> {
        Self::new(
            TableName::parse(step.table())?,
            step.key(),
            &step.backfill_fields(),
        )
    }

    /// The kind of work, from the key.
    pub fn work(&self) -> SchemaStepKind {
        self.key.kind()
    }

    /// The table.
    pub fn table(&self) -> &TableName {
        &self.table
    }

    /// The step's key.
    pub fn key(&self) -> &str {
        self.key.as_str()
    }

    /// The step's key, checked.
    pub fn step_key(&self) -> &SchemaStepKey {
        &self.key
    }

    /// The stored computed fields a backfill recorded; empty for other steps.
    pub fn fields(&self) -> &[ColumnName] {
        &self.fields
    }
}

/// What a post-commit step does after the operation committed.
///
/// The statement is built when the step runs, with every identifier quoted; there is no way to
/// give it SQL text.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum PostCommitAction {
    /// Build an index without blocking writers.
    CreateIndex(CreateIndex),
    /// `ANALYZE` one table.
    Analyze {
        /// The table.
        table: TableName,
    },
    /// The large-table work of a schema change, planned again and run by the ORM.
    Schema(SchemaStepRef),
}

/// The stored form of an action's details (`ir_module_post_commit.spec`).
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct SpecDto {
    columns: Vec<ColumnDto>,
    unique: bool,
    method: String,
}

/// The stored form of a schema step (`spec` of a `schema_step` row).
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct SchemaSpecDto {
    work: String,
    key: String,
    #[serde(default)]
    fields: Vec<String>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ColumnDto {
    name: String,
    opclass: Option<String>,
}

/// The `kind` column value of an index step.
const KIND_CREATE_INDEX: &str = "create_index";
/// The `kind` column value of an analyze step.
const KIND_ANALYZE: &str = "analyze";
/// The `kind` column value of a schema step.
pub(crate) const KIND_SCHEMA_STEP: &str = "schema_step";

/// The values of the `kind` column (the selection of `ir.module.post_commit.kind`).
pub(crate) const KINDS: &[(&str, &str)] = &[
    (KIND_CREATE_INDEX, "Create index"),
    (KIND_ANALYZE, "Analyze table"),
    (KIND_SCHEMA_STEP, "Schema step of a large table"),
];

/// The columns of a stored action.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct StoredAction {
    pub(crate) kind: &'static str,
    pub(crate) table: String,
    pub(crate) index: Option<String>,
    pub(crate) spec: Option<String>,
}

impl PostCommitAction {
    /// Build an index.
    pub fn create_index(index: CreateIndex) -> Self {
        Self::CreateIndex(index)
    }

    /// `ANALYZE` a table.
    pub fn analyze(table: TableName) -> Self {
        Self::Analyze { table }
    }

    /// The step of a large-table schema change ([`SchemaStepRef::from_step`]).
    ///
    /// # Errors
    /// As [`SchemaStepRef::from_step`].
    pub fn schema_step(step: &SchemaStep) -> Result<Self> {
        SchemaStepRef::from_step(step).map(Self::Schema)
    }

    /// The table the step works on (kept in the queue row, and checked against the index before
    /// an invalid index is dropped).
    pub fn table(&self) -> &TableName {
        match self {
            Self::CreateIndex(c) => &c.table,
            Self::Analyze { table } => table,
            Self::Schema(s) => &s.table,
        }
    }

    /// The index the step builds, for an index step.
    pub fn index(&self) -> Option<&IndexName> {
        match self {
            Self::CreateIndex(c) => Some(&c.index),
            Self::Analyze { .. } | Self::Schema(_) => None,
        }
    }

    /// The columns to store.
    pub(crate) fn stored(&self) -> StoredAction {
        match self {
            Self::CreateIndex(c) => {
                let columns: Vec<serde_json::Value> = c
                    .columns
                    .iter()
                    .map(|col| {
                        serde_json::json!({
                            "name": col.name.as_str(),
                            "opclass": col.opclass.as_ref().map(OpClassName::as_str),
                        })
                    })
                    .collect();
                let spec = serde_json::json!({
                    "columns": columns,
                    "unique": c.unique,
                    "method": c.method.as_str(),
                });
                StoredAction {
                    kind: KIND_CREATE_INDEX,
                    table: c.table.as_str().to_owned(),
                    index: Some(c.index.as_str().to_owned()),
                    spec: Some(spec.to_string()),
                }
            }
            Self::Analyze { table } => StoredAction {
                kind: KIND_ANALYZE,
                table: table.as_str().to_owned(),
                index: None,
                spec: None,
            },
            Self::Schema(s) => StoredAction {
                kind: KIND_SCHEMA_STEP,
                table: s.table.as_str().to_owned(),
                index: None,
                spec: Some({
                    let mut spec = serde_json::json!({"work": s.work, "key": s.key()});
                    if !s.fields.is_empty() {
                        let names: Vec<&str> = s.fields.iter().map(ColumnName::as_str).collect();
                        spec["fields"] = serde_json::json!(names);
                    }
                    spec.to_string()
                }),
            },
        }
    }

    /// Read an action back from the columns of a queue row.
    ///
    /// # Errors
    /// The reason (for listing the row as invalid) when the kind is unknown, an identifier breaks
    /// its rule, the spec is not the strict JSON of [`PostCommitAction::stored`] or is longer than
    /// [`MAX_ACTION_SPEC_BYTES`], or a column that does not belong to the kind is set.
    pub(crate) fn from_stored(
        kind: &str,
        table: &str,
        index: Option<&str>,
        spec: Option<&str>,
    ) -> std::result::Result<Self, String> {
        let message = |e: Error| e.message().to_owned();
        let table = TableName::parse(table).map_err(message)?;
        match kind {
            KIND_ANALYZE => {
                if index.is_some() || spec.is_some() {
                    return Err("an analyze step has no index or spec".to_owned());
                }
                Ok(Self::Analyze { table })
            }
            KIND_SCHEMA_STEP => {
                if index.is_some() {
                    return Err("a schema step has no index name".to_owned());
                }
                let spec = spec.ok_or_else(|| "a schema step has no spec".to_owned())?;
                if spec.len() > MAX_ACTION_SPEC_BYTES {
                    return Err(format!(
                        "the spec is longer than {MAX_ACTION_SPEC_BYTES} bytes"
                    ));
                }
                let dto: SchemaSpecDto =
                    serde_json::from_str(spec).map_err(|e| format!("invalid spec: {e}"))?;
                let work = WORKS
                    .iter()
                    .find(|(_, name)| *name == dto.work)
                    .map(|(kind, _)| *kind)
                    .ok_or_else(|| format!("unknown schema step work {:?}", dto.work))?;
                SchemaStepRef::checked(Some(work), table, &dto.key, &dto.fields).map(Self::Schema)
            }
            KIND_CREATE_INDEX => {
                let index = index.ok_or_else(|| "an index step has no index name".to_owned())?;
                let index = IndexName::parse(index).map_err(message)?;
                let spec = spec.ok_or_else(|| "an index step has no spec".to_owned())?;
                if spec.len() > MAX_ACTION_SPEC_BYTES {
                    return Err(format!(
                        "the spec is longer than {MAX_ACTION_SPEC_BYTES} bytes"
                    ));
                }
                let dto: SpecDto =
                    serde_json::from_str(spec).map_err(|e| format!("invalid spec: {e}"))?;
                let method = IndexMethod::parse(&dto.method)
                    .ok_or_else(|| format!("unknown index method {:?}", dto.method))?;
                let mut columns = Vec::with_capacity(dto.columns.len());
                for column in &dto.columns {
                    let mut parsed = IndexColumn::new(&column.name).map_err(message)?;
                    if let Some(opclass) = &column.opclass {
                        parsed = parsed.with_opclass(opclass).map_err(message)?;
                    }
                    columns.push(parsed);
                }
                let mut create = CreateIndex::new(table, index, columns).map_err(message)?;
                create.unique = dto.unique;
                create.method = method;
                Ok(Self::CreateIndex(create))
            }
            other => Err(format!("unknown step kind {other:?}")),
        }
    }

    /// The statement, with every identifier quoted and the table qualified with `schema` (the
    /// phase's `current_schema()`, read once, so the statement, the index lookups and the
    /// invalid-index guard all name the same table). `None` for a schema step, which the ORM
    /// plans and runs.
    pub(crate) fn sql(&self, schema: &str) -> Option<String> {
        let qualified = |table: &TableName| {
            format!(
                "{}.{}",
                erp_sql::quote(schema),
                erp_sql::quote(table.as_str())
            )
        };
        Some(match self {
            Self::Schema(_) => return None,
            Self::CreateIndex(c) => {
                let columns: Vec<String> = c
                    .columns
                    .iter()
                    .map(|col| match &col.opclass {
                        Some(opclass) => format!(
                            "{} {}",
                            erp_sql::quote(col.name.as_str()),
                            erp_sql::quote(opclass.as_str())
                        ),
                        None => erp_sql::quote(col.name.as_str()),
                    })
                    .collect();
                format!(
                    "CREATE {}INDEX CONCURRENTLY IF NOT EXISTS {} ON {} USING {} ({})",
                    if c.unique { "UNIQUE " } else { "" },
                    erp_sql::quote(c.index.as_str()),
                    qualified(&c.table),
                    c.method.as_str(),
                    columns.join(", ")
                )
            }
            Self::Analyze { table } => format!("ANALYZE {}", qualified(table)),
        })
    }
}

#[cfg(test)]
mod tests {
    use erp_orm::MAX_STEP_KEY_BYTES;

    use super::*;

    fn index() -> CreateIndex {
        CreateIndex::new(
            TableName::parse("t").unwrap(),
            IndexName::parse("t_idx").unwrap(),
            vec![
                IndexColumn::new("a").unwrap(),
                IndexColumn::new("b")
                    .unwrap()
                    .with_opclass("gin_trgm_ops")
                    .unwrap(),
            ],
        )
        .unwrap()
        .unique()
        .method(IndexMethod::Gin)
    }

    #[test]
    fn an_action_survives_storage_and_renders_quoted_sql() {
        let action = PostCommitAction::create_index(index());
        let stored = action.stored();
        let back = PostCommitAction::from_stored(
            stored.kind,
            &stored.table,
            stored.index.as_deref(),
            stored.spec.as_deref(),
        )
        .unwrap();
        assert_eq!(back, action);
        assert_eq!(
            back.sql("public").as_deref(),
            Some(
                "CREATE UNIQUE INDEX CONCURRENTLY IF NOT EXISTS \"t_idx\" ON \"public\".\"t\" \
                 USING gin (\"a\", \"b\" \"gin_trgm_ops\")"
            )
        );
        let analyze = PostCommitAction::analyze(TableName::parse("t").unwrap());
        assert_eq!(
            analyze.sql("s\"x").as_deref(),
            Some("ANALYZE \"s\"\"x\".\"t\"")
        );
    }

    #[test]
    fn tampered_columns_are_refused_with_the_reason() {
        let spec = |s: &str| PostCommitAction::from_stored("create_index", "t", Some("i"), Some(s));
        let good = r#"{"columns":[{"name":"a","opclass":null}],"unique":false,"method":"btree"}"#;
        assert!(spec(good).is_ok());
        assert_eq!(
            spec(r#"{"columns":[{"name":"a\"; DROP TABLE x;--","opclass":null}],"unique":false,"method":"btree"}"#)
                .unwrap_err(),
            "invalid column name \"a\\\"; DROP TABLE x;--\": 1 to 63 bytes of a-z, 0-9, '_', not \
             starting with a digit"
        );
        assert_eq!(
            spec(
                r#"{"columns":[{"name":"a","opclass":null}],"unique":false,"method":"btree","x":1}"#
            )
            .unwrap_err(),
            "invalid spec: unknown field `x`, expected one of `columns`, `unique`, `method` at line 1 column 76"
        );
        assert_eq!(
            spec(r#"{"columns":[{"name":"a","opclass":null}],"unique":false,"method":"rum"}"#)
                .unwrap_err(),
            "unknown index method \"rum\""
        );
        assert_eq!(
            PostCommitAction::from_stored("analyze", "t", Some("i"), None).unwrap_err(),
            "an analyze step has no index or spec"
        );
        assert_eq!(
            PostCommitAction::from_stored("copy", "t", None, None).unwrap_err(),
            "unknown step kind \"copy\""
        );
        let long = format!(
            r#"{{"columns":[{{"name":"a","opclass":null}}],"unique":false,"method":"btree","pad":"{}"}}"#,
            "x".repeat(MAX_ACTION_SPEC_BYTES)
        );
        assert_eq!(
            spec(&long).unwrap_err(),
            format!("the spec is longer than {MAX_ACTION_SPEC_BYTES} bytes")
        );
    }

    #[test]
    fn a_schema_step_survives_storage_and_is_checked_on_the_way_back() {
        let table = TableName::parse("sale_order").unwrap();
        let step = SchemaStepRef::new(table.clone(), "not_null.sale_order.qty", &[]).unwrap();
        let action = PostCommitAction::Schema(step);
        let stored = action.stored();
        assert_eq!(stored.kind, "schema_step");
        assert_eq!(stored.index, None);
        let back = PostCommitAction::from_stored(
            stored.kind,
            &stored.table,
            stored.index.as_deref(),
            stored.spec.as_deref(),
        )
        .unwrap();
        assert_eq!(back, action);
        assert_eq!(back.sql("public"), None);
        // a backfill records its fields
        let backfill = PostCommitAction::Schema(
            SchemaStepRef::new(table, "backfill.sale_order", &["a", "b"]).unwrap(),
        );
        let stored = backfill.stored();
        assert_eq!(
            PostCommitAction::from_stored(stored.kind, &stored.table, None, stored.spec.as_deref())
                .unwrap(),
            backfill
        );
        let read = |work: &str, key: &str, index: Option<&str>, fields: &[&str]| {
            let spec = serde_json::json!({"work": work, "key": key, "fields": fields}).to_string();
            let spec = if fields.is_empty() {
                serde_json::json!({"work": work, "key": key}).to_string()
            } else {
                spec
            };
            PostCommitAction::from_stored("schema_step", "sale_order", index, Some(&spec))
        };
        assert_eq!(
            read("backfill", "backfill.other", None, &["a"]).unwrap_err(),
            "schema step key \"backfill.other\" does not fit a backfill step on table \
             sale_order: it must be \"backfill.sale_order\""
        );
        assert_eq!(
            read("not_null", "not_null.other.qty", None, &[]).unwrap_err(),
            "schema step key \"not_null.other.qty\" does not fit a not_null step on table \
             sale_order: it must start with \"not_null.sale_order.\""
        );
        assert_eq!(
            read("not_null", "not_null.sale_order.", None, &[]).unwrap_err(),
            "schema step key \"not_null.sale_order.\" is not a valid not_null.<table>.<column> \
             step key"
        );
        assert_eq!(
            read("backfill", "index.x", None, &[]).unwrap_err(),
            "schema step key index.x is a create_index step, not backfill"
        );
        assert_eq!(
            read("copy", "index.x", None, &[]).unwrap_err(),
            "unknown schema step work \"copy\""
        );
        assert_eq!(
            read("create_index", "index.x; y", None, &[]).unwrap_err(),
            format!(
                "invalid schema step key \"index.x; y\": 1 to {MAX_STEP_KEY_BYTES} bytes of \
                 a-z, 0-9, '_', '.'"
            )
        );
        assert_eq!(
            read("create_index", "index.x", Some("i"), &[]).unwrap_err(),
            "a schema step has no index name"
        );
        assert_eq!(
            read("backfill", "backfill.sale_order", None, &[]).unwrap_err(),
            "a backfill step records 1 to 64 fields, not 0"
        );
        assert_eq!(
            read("create_index", "index.x", None, &["a"]).unwrap_err(),
            "only a backfill step records fields, not a create_index step"
        );
        assert_eq!(
            PostCommitAction::from_stored("schema_step", "sale_order", None, None).unwrap_err(),
            "a schema step has no spec"
        );
        assert!(read("create_index", "index.x", None, &[]).is_ok());
    }

    /// The two steps of a business key on a large table are queue rows with their
    /// own work names; a row whose work and key disagree, or that records fields, is refused.
    #[test]
    fn the_steps_of_a_business_key_survive_storage() {
        let table = TableName::parse("sale_order").unwrap();
        for (key, work) in [
            ("key_index.sale_order_ref_key", "create_key_index"),
            ("key_constraint.sale_order_ref_key", "add_key_constraint"),
        ] {
            let step = SchemaStepRef::new(table.clone(), key, &[]).unwrap();
            assert_eq!(schema_work_name(step.work()).unwrap(), work);
            let action = PostCommitAction::Schema(step);
            let stored = action.stored();
            let spec: serde_json::Value =
                serde_json::from_str(stored.spec.as_deref().unwrap()).unwrap();
            assert_eq!(spec["work"], work);
            assert_eq!(
                PostCommitAction::from_stored(
                    stored.kind,
                    &stored.table,
                    None,
                    stored.spec.as_deref()
                )
                .unwrap(),
                action
            );
            assert_eq!(
                SchemaStepRef::new(table.clone(), key, &["a"])
                    .unwrap_err()
                    .message(),
                format!("only a backfill step records fields, not a {work} step")
            );
        }
        let spec = serde_json::json!({"work": "create_key_index", "key": "key_constraint.k"});
        assert_eq!(
            PostCommitAction::from_stored(
                "schema_step",
                "sale_order",
                None,
                Some(&spec.to_string())
            )
            .unwrap_err(),
            "schema step key key_constraint.k is a add_key_constraint step, not create_key_index"
        );
    }

    #[test]
    fn names_follow_the_rule() {
        assert!(TableName::parse("").is_err());
        assert!(TableName::parse("1a").is_err());
        assert!(TableName::parse("a-b").is_err());
        assert!(TableName::parse("Aa").is_err());
        for (what, err) in [
            ("table", TableName::parse("pg_class").unwrap_err()),
            ("index", IndexName::parse("pg_toast_idx").unwrap_err()),
        ] {
            assert_eq!(err.kind(), erp_fields::ErrorKind::Programming);
            let name = if what == "table" {
                "pg_class"
            } else {
                "pg_toast_idx"
            };
            assert_eq!(
                err.message(),
                format!(
                    "invalid {what} name {name:?}: names starting with pg_ are reserved for \
                     PostgreSQL's catalogs"
                )
            );
        }
        let long = "i".repeat(MAX_SQL_NAME_BYTES);
        assert_eq!(IndexName::parse(&long).unwrap().as_str(), long);
        let too_long = "i".repeat(MAX_SQL_NAME_BYTES + 1);
        for bad in ["", "Idx", "a-b", "a.b", "é", "1i", too_long.as_str()] {
            let err = IndexName::parse(bad).unwrap_err();
            assert_eq!(err.kind(), erp_fields::ErrorKind::Programming);
            assert_eq!(
                err.message(),
                format!(
                    "invalid index name {bad:?}: 1 to {MAX_SQL_NAME_BYTES} bytes of a-z, 0-9, \
                     '_', not starting with a digit"
                )
            );
        }
        assert!(TableName::parse(&"a".repeat(64)).is_err());
        assert!(TableName::parse(&"a".repeat(63)).is_ok());
        assert!(
            CreateIndex::new(
                TableName::parse("t").unwrap(),
                IndexName::parse("i").unwrap(),
                Vec::new()
            )
            .is_err()
        );
    }
}
