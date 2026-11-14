// SPDX-License-Identifier: MIT OR Apache-2.0
//! Native foreign keys and table persistence (B6).
use super::decl::{SqlDeferral, SqlObjectKindDecl};
use super::graph::{SqlModelRef, SqlObjectId};
use super::validate::{Who, check_ident};
use super::{SqlObjectLimits, within};
use crate::{ModelSpec, Registry, TableObjectKind};
use erp_fields::{ColumnType, Result};

/// The declared unique target of a foreign key. No SQL expression is accepted.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SqlForeignKeyTarget {
    /// The model's primary key (`id`).
    PrimaryKey,
    /// A native typed business key, by its local table-object name.
    Key(&'static str),
}
/// ON DELETE behavior. A SET NULL action contains the local columns it clears.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SqlForeignKeyAction {
    /// Check at the constraint's declared time.
    NoAction,
    /// Refuse deletion immediately.
    Restrict,
    /// Delete referencing rows.
    Cascade,
    /// Clear only these nullable local columns.
    SetNull(&'static [&'static str]),
}
/// Native composite foreign-key declaration; validated when the registry is built.
///
/// ```
/// use erp_registry::{SqlForeignKeyDecl, SqlForeignKeyTarget, SqlForeignKeyAction};
/// let reference = SqlForeignKeyDecl::new("line_company_fk", "sample.line",
///     &["company_id", "parent_id"], "sample.parent", &["company_id", "id"],
///     SqlForeignKeyTarget::Key("_company_id"))
///     .on_delete(SqlForeignKeyAction::SetNull(&["parent_id"]));
/// ```
#[derive(Debug, Clone, Copy)]
#[non_exhaustive]
pub struct SqlForeignKeyDecl {
    pub(crate) name: &'static str,
    pub(crate) model: &'static str,
    pub(crate) columns: &'static [&'static str],
    pub(crate) target: &'static str,
    pub(crate) target_columns: &'static [&'static str],
    pub(crate) target_key: SqlForeignKeyTarget,
    pub(crate) delete: SqlForeignKeyAction,
    pub(crate) deferral: SqlDeferral,
}
impl SqlForeignKeyDecl {
    /// A named reference, initially NOT DEFERRABLE with ON DELETE NO ACTION.
    pub const fn new(
        name: &'static str,
        model: &'static str,
        columns: &'static [&'static str],
        target: &'static str,
        target_columns: &'static [&'static str],
        target_key: SqlForeignKeyTarget,
    ) -> Self {
        Self {
            name,
            model,
            columns,
            target,
            target_columns,
            target_key,
            delete: SqlForeignKeyAction::NoAction,
            deferral: SqlDeferral::NotDeferrable,
        }
    }
    /// Select ON DELETE behavior.
    pub const fn on_delete(mut self, action: SqlForeignKeyAction) -> Self {
        self.delete = action;
        self
    }
    /// Select constraint timing.
    pub const fn deferral(mut self, deferral: SqlDeferral) -> Self {
        self.deferral = deferral;
        self
    }
}
/// Physical persistence of an owning native model's table. Conversion requires a migration.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SqlTablePersistence {
    /// WAL-logged table (the default without a declaration).
    Logged,
    /// Ephemeral table contents, lost after an unclean shutdown and absent on standbys.
    Unlogged,
}
impl SqlTablePersistence {
    /// PostgreSQL `relpersistence` code.
    pub const fn catalog(self) -> &'static str {
        match self {
            Self::Logged => "p",
            Self::Unlogged => "u",
        }
    }
}
/// Table options reuse the model's columns, defaults and keys.
#[derive(Debug, Clone, Copy)]
#[non_exhaustive]
pub struct SqlTableOptionsDecl {
    pub(crate) model: &'static str,
    pub(crate) persistence: SqlTablePersistence,
}
impl SqlTableOptionsDecl {
    /// Declare persistence for `model`; only its original native owner may do so.
    pub const fn new(model: &'static str, persistence: SqlTablePersistence) -> Self {
        Self { model, persistence }
    }
}
/// Physical shape of one matched pair of foreign-key columns.
#[derive(Debug)]
#[non_exhaustive]
pub struct SqlForeignKeyColumn {
    ty: ColumnType,
    local_required: bool,
    target_required: bool,
}
impl SqlForeignKeyColumn {
    /// The common physical storage type.
    pub fn ty(&self) -> ColumnType {
        self.ty
    }
    /// Whether the local column is required.
    pub fn local_required(&self) -> bool {
        self.local_required
    }
    /// Whether the referenced column is required.
    pub fn target_required(&self) -> bool {
        self.target_required
    }
}
/// A validated foreign key with resolved source and target tables.
#[derive(Debug)]
#[non_exhaustive]
pub struct SqlForeignKey {
    pub(crate) decl: SqlForeignKeyDecl,
    pub(crate) column_shapes: Vec<SqlForeignKeyColumn>,
    pub(crate) model: SqlModelRef,
    pub(crate) target: SqlModelRef,
}
impl SqlForeignKey {
    /// Validated type and nullability pairs in column order.
    pub fn column_shapes(&self) -> &[SqlForeignKeyColumn] {
        &self.column_shapes
    }
    /// Physical constraint name.
    pub fn name(&self) -> &'static str {
        self.decl.name
    }
    /// Local table.
    pub fn model(&self) -> &SqlModelRef {
        &self.model
    }
    /// Referenced table.
    pub fn target(&self) -> &SqlModelRef {
        &self.target
    }
    /// Ordered local columns.
    pub fn columns(&self) -> &'static [&'static str] {
        self.decl.columns
    }
    /// Ordered referenced columns.
    pub fn target_columns(&self) -> &'static [&'static str] {
        self.decl.target_columns
    }
    /// Declared unique key identity.
    pub fn target_key(&self) -> SqlForeignKeyTarget {
        self.decl.target_key
    }
    /// ON DELETE action.
    pub fn on_delete(&self) -> SqlForeignKeyAction {
        self.decl.delete
    }
    /// Constraint timing.
    pub fn deferral(&self) -> SqlDeferral {
        self.decl.deferral
    }
}
/// Validated table options.
#[derive(Debug)]
#[non_exhaustive]
pub struct SqlTableOptions {
    pub(crate) model: SqlModelRef,
    pub(crate) persistence: SqlTablePersistence,
}
impl SqlTableOptions {
    /// Owning model/table.
    pub fn model(&self) -> &SqlModelRef {
        &self.model
    }
    /// Declared persistence.
    pub fn persistence(&self) -> SqlTablePersistence {
        self.persistence
    }
}
fn columns(who: Who<'_>, label: &str, values: &[&str], limit: usize) -> Result<()> {
    within(who, "foreign key columns", values.len(), limit)?;
    let mut seen = std::collections::BTreeSet::new();
    for name in values {
        check_ident(who, "column", name)?;
        if !seen.insert(name) {
            return Err(err!(
                who,
                "{label} column '{name}' is listed more than once"
            ));
        }
    }
    Ok(())
}
pub(crate) fn check(who: Who<'_>, f: &SqlForeignKeyDecl, limits: &SqlObjectLimits) -> Result<()> {
    check_ident(who, "constraint", f.name)?;
    if f.name.to_ascii_lowercase().starts_with("erp_") {
        return Err(err!(
            who,
            "names starting with 'erp_' are reserved for core objects"
        ));
    }
    if f.columns.is_empty() {
        return Err(err!(who, "foreign key columns must not be empty"));
    }
    if f.columns.len() != f.target_columns.len() {
        return Err(err!(who, "foreign key column arity differs"));
    }
    columns(who, "local", f.columns, limits.foreign_key_columns)?;
    columns(who, "target", f.target_columns, limits.foreign_key_columns)?;
    if let SqlForeignKeyTarget::Key(key) = f.target_key {
        check_ident(who, "target key", key)?;
    }
    if let SqlForeignKeyAction::SetNull(subset) = f.delete {
        if subset.is_empty() {
            return Err(err!(who, "SET NULL columns must not be empty"));
        }
        columns(who, "SET NULL", subset, limits.foreign_key_columns)?;
        for name in subset {
            if !f.columns.contains(name) {
                return Err(err!(who, "SET NULL column '{name}' is not a local column"));
            }
        }
    }
    Ok(())
}
fn field(who: Who<'_>, m: &ModelSpec, name: &str) -> Result<(ColumnType, bool)> {
    if name == "id" {
        return Ok((ColumnType::Int8, true));
    }
    let f = m
        .field(name)
        .filter(|f| !f.is_generated() && !f.company_dependent && !f.translate);
    match f.and_then(|f| f.column().map(|ty| (ty, f.required))) {
        Some(pair) => Ok(pair),
        None => Err(err!(
            who,
            "field '{}.{name}' is not an ordinary stored column",
            m.name
        )),
    }
}
pub(crate) fn resolve(
    registry: &Registry,
    id: SqlObjectId,
    f: &SqlForeignKeyDecl,
) -> Result<Vec<SqlForeignKeyColumn>> {
    let who = (id.addon(), id.key());
    let source = registry
        .model(f.model)
        .ok_or_else(|| err!(who, "model '{}' is not in the registry", f.model))?;
    let target = registry
        .model(f.target)
        .ok_or_else(|| err!(who, "model '{}' is not in the registry", f.target))?;
    match f.target_key {
        SqlForeignKeyTarget::PrimaryKey if f.target_columns == ["id"] => {}
        SqlForeignKeyTarget::PrimaryKey => {
            return Err(err!(who, "primary key target columns must be [id]"));
        }
        SqlForeignKeyTarget::Key(name) => {
            let found = target.table_objects.iter().find(|k| k.name == name);
            if !matches!(found.map(|k| &k.kind), Some(TableObjectKind::Key(key)) if key.fields == f.target_columns)
            {
                return Err(err!(
                    who,
                    "target key '{name}' does not match the ordered target columns"
                ));
            }
        }
    }
    let mut shapes = Vec::with_capacity(f.columns.len());
    for (local, remote) in f.columns.iter().zip(f.target_columns) {
        let (a, required) = field(who, source, local)?;
        let (b, target_required) = field(who, target, remote)?;
        if a != b {
            return Err(err!(
                who,
                "columns '{}.{local}' and '{}.{remote}' have incompatible types",
                source.name,
                target.name
            ));
        }
        if required && matches!(f.delete, SqlForeignKeyAction::SetNull(s) if s.contains(local)) {
            return Err(err!(
                who,
                "SET NULL column '{}.{local}' is required",
                source.name
            ));
        }
        shapes.push(SqlForeignKeyColumn {
            ty: a,
            local_required: required,
            target_required,
        });
    }
    Ok(shapes)
}
pub(crate) fn bytes(f: &SqlForeignKeyDecl) -> Option<usize> {
    let subset = match f.delete {
        SqlForeignKeyAction::SetNull(s) => s,
        _ => &[],
    };
    let key = match f.target_key {
        SqlForeignKeyTarget::Key(k) => k,
        SqlForeignKeyTarget::PrimaryKey => "id",
    };
    [f.name, f.model, f.target, key]
        .into_iter()
        .chain(f.columns.iter().copied())
        .chain(f.target_columns.iter().copied())
        .chain(subset.iter().copied())
        .try_fold(0usize, |n, s| n.checked_add(s.len()))
}
/// Table options are owner-only and unlogged tables require explicit restricted privileges.
pub(crate) fn table_options(
    registry: &Registry,
    who: Who<'_>,
    o: SqlTableOptionsDecl,
    decls: impl Iterator<Item = SqlObjectKindDecl>,
) -> Result<()> {
    let m = registry
        .model(o.model)
        .ok_or_else(|| err!(who, "model '{}' is not in the registry", o.model))?;
    let owner = m
        .mro
        .iter()
        .rev()
        .find(|d| d.name == m.name && !d.is_extension());
    if !owner.is_some_and(|d| d.addon == who.0 && !d.runtime) {
        return Err(err!(
            who,
            "table options for '{}' may only be declared by its original native owner",
            m.name
        ));
    }
    if o.persistence == SqlTablePersistence::Unlogged {
        if m.partition.is_some() {
            return Err(err!(
                who,
                "unlogged partitioned table '{}' is unsupported",
                m.name
            ));
        }
        if !decls
            .into_iter()
            .any(|d| matches!(d, SqlObjectKindDecl::TableGrant(g) if g.model == o.model))
        {
            return Err(err!(
                who,
                "unlogged table '{}' needs an explicit table grant policy",
                m.name
            ));
        }
    }
    Ok(())
}
