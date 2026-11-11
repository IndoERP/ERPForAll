//! Business keys and row versions in the registry (and D3; design note
//! , §4.1, §9).
//!
//! - `row_version = true` on a declaration adds the server-maintained `row_version` field
//!   ([`ROW_VERSION_FIELD`]) to the model; once a declaration in the method resolution order turns
//!   it on, a later one cannot turn it off.
//! - A [`KeyDecl`] table object becomes a [`BusinessKey`] of the composed model once its fields
//!   are checked: stored columns of the model's own table, of a kind whose equality PostgreSQL and
//!   the ORM agree on.

use erp_fields::{Error, FieldType, Result};

use crate::decl::{FieldDecl, KindDecl, ModelDecl, TableObjectKind};
use crate::ident::shorten_identifier;
use crate::spec::{FieldId, ModelSpec};

/// Name of the row version field and column.
pub const ROW_VERSION: &str = "row_version";

/// The field `row_version = true` adds: readonly, not copied, required (the column is `NOT NULL`,
/// filled with 1 on existing rows by schema sync).
pub(crate) const ROW_VERSION_FIELDS: &[FieldDecl] =
    &[FieldDecl::new(ROW_VERSION, KindDecl::RowVersion)
        .string("Row Version")
        .readonly(true)
        .copy(false)
        .required(true)];

/// Whether the declarations of `model` (newest first, as in [`ModelSpec::mro`]) turn the row
/// version on, and the declaration that does it first (the oldest).
///
/// # Errors
///
/// `Programming` when a declaration turns it off after an older one turned it on.
pub(crate) fn row_version_origin(
    model: &str,
    mro: &[&'static ModelDecl],
) -> Result<Option<&'static ModelDecl>> {
    let mut origin: Option<&'static ModelDecl> = None;
    for decl in mro.iter().rev() {
        match (decl.row_version, origin) {
            (Some(true), None) => origin = Some(decl),
            (Some(false), Some(on)) => {
                return Err(Error::programming(format!(
                    "{} sets row_version = false on model '{model}', but {} already turned it on; \
                     a row version cannot be turned off once declared",
                    decl.describe(),
                    on.describe()
                )));
            }
            _ => {}
        }
    }
    Ok(origin)
}

/// A business key of a composed model (a [`TableObjectKind::Key`] whose fields were checked).
#[derive(Debug, Clone)]
#[non_exhaustive]
pub struct BusinessKey {
    /// Declared name, starting with `_` (e.g. `_quant_key`).
    pub name: &'static str,
    /// Database constraint name: `<table>_<name without the leading _>`, shortened (the full name
    /// of every table object).
    pub full_name: String,
    /// The key fields, in the constraint's column order.
    pub fields: Vec<FieldId>,
    /// User message when a row would duplicate the key.
    pub message: &'static str,
    /// `NULLS DISTINCT`: rows with NULL parts never conflict, so the key cannot arbitrate an
    /// upsert.
    pub nulls_distinct: bool,
}

impl BusinessKey {
    /// Why the key cannot arbitrate an upsert, or `None` when it can.
    pub fn arbitration_refusal(&self) -> Option<&'static str> {
        self.nulls_distinct.then_some("nulls_distinct")
    }
}

impl ModelSpec {
    /// The business key declared as `name` (e.g. `_quant_key`), if the model has one.
    pub fn business_key(&self, name: &str) -> Option<&BusinessKey> {
        self.keys.iter().find(|k| k.name == name)
    }

    /// The business keys of the model, in table-object order.
    pub fn business_keys(&self) -> &[BusinessKey] {
        &self.keys
    }
}

/// The full name of a table object declared as `name` on `model` (the composer's rule, which
/// also refuses names without the leading `_` before this runs).
fn full_name(model: &ModelSpec, name: &str) -> Result<String> {
    let suffix = name.strip_prefix('_').ok_or_else(|| {
        Error::programming(format!(
            "Names of SQL objects in a model must start with '_' (got '{name}' on model '{}')",
            model.name
        ))
    })?;
    Ok(shorten_identifier(&format!("{}_{suffix}", model.table)))
}

/// Check the business keys and the row version field of every model and record them on the
/// specs. Runs once relational fields and `_inherits` fields are resolved.
pub(crate) fn finish(models: &mut [ModelSpec]) -> Result<()> {
    for model in models.iter_mut() {
        check_row_version(model)?;
        let keys = business_keys(model)?;
        model.keys = keys;
    }
    Ok(())
}

/// The `row_version` field exists exactly when a declaration turned the row version on, and no
/// declaration declares a field of that name or kind itself.
fn check_row_version(model: &ModelSpec) -> Result<()> {
    for decl in &model.mro {
        for fd in decl.fields {
            let reason = if matches!(fd.kind, KindDecl::RowVersion) {
                "the row version kind is reserved to the field the registry adds for \
                 row_version = true"
            } else if fd.name == ROW_VERSION && model.row_version.is_some() {
                "the name is reserved to the field the registry adds for row_version = true"
            } else {
                continue;
            };
            return Err(Error::programming(format!(
                "{} declares field '{}' on model '{}': {reason}",
                decl.describe(),
                fd.name,
                model.name
            )));
        }
    }
    if let Some(fid) = model.row_version {
        let field = model.field_by_id(fid);
        if !matches!(field.ty, FieldType::RowVersion) || field.name != ROW_VERSION {
            return Err(Error::programming(format!(
                "model '{}': the row version field is '{}' of type {}, expected '{ROW_VERSION}'",
                model.name,
                field.name,
                field.ty.type_name()
            )));
        }
    }
    Ok(())
}

/// Whether a field kind can be part of a business key: exact equality in PostgreSQL and in the
/// ORM's cells (no floats), a column of its own (no x2many), not the id or the row version.
fn key_kind_refusal(ty: &FieldType) -> Option<&'static str> {
    match ty {
        FieldType::Boolean
        | FieldType::Integer
        | FieldType::Float { digits: Some(_) }
        | FieldType::Monetary { .. }
        | FieldType::Char { .. }
        | FieldType::Selection { .. }
        | FieldType::Date
        | FieldType::Datetime
        | FieldType::Uuid
        | FieldType::Xid8
        | FieldType::TimestampTz
        | FieldType::Many2one { .. }
        | FieldType::Reference { .. }
        | FieldType::Many2oneReference { .. } => None,
        FieldType::Float { digits: None } => {
            Some("a float without digits compares inexactly; give it digits or use another field")
        }
        FieldType::Text | FieldType::Html => Some(
            "text and HTML fields are unbounded, and a btree entry over about 2.7 kB fails with a \
             bare database error; give the key a char field with a size",
        ),
        FieldType::Json => Some("JSON documents have no exact key equality"),
        FieldType::One2many { .. } | FieldType::Many2many { .. } => {
            Some("x2many fields have no column")
        }
        FieldType::Id => Some("the id is unique already"),
        FieldType::RowVersion => Some("the row version changes on every update"),
        FieldType::DateRange { .. } | FieldType::DatetimeRange { .. } => Some(
            "range fields have no exact key equality in the ORM; use key(..., without_overlaps = \
             \"<range field>\") for a period",
        ),
    }
}

/// The checked business keys of `model`, in table-object order.
fn business_keys(model: &ModelSpec) -> Result<Vec<BusinessKey>> {
    // declared names of the key table objects, by full name (the newest declaration's wins, as
    // the composer composes them)
    let mut declared: Vec<(String, &'static str)> = Vec::new();
    for decl in model.mro.iter().rev() {
        for obj in decl.table_objects {
            let full = full_name(model, obj.name)?;
            match declared.iter_mut().find(|(f, _)| *f == full) {
                Some(entry) => entry.1 = obj.name,
                None => declared.push((full, obj.name)),
            }
        }
    }
    let mut keys = Vec::new();
    for obj in &model.table_objects {
        let TableObjectKind::Key(decl) = obj.kind else {
            continue;
        };
        let Some(name) = declared
            .iter()
            .find(|(f, _)| *f == obj.full_name)
            .map(|(_, n)| *n)
        else {
            return Err(Error::programming(format!(
                "model '{}': no declaration names the key table object '{}'",
                model.name, obj.full_name
            )));
        };
        let what = format!("key '{name}' of model '{}'", model.name);
        if decl.fields.is_empty() {
            return Err(Error::programming(format!("{what} has no fields")));
        }
        if decl.message.trim().is_empty() {
            return Err(Error::programming(format!(
                "{what} needs a message (shown when a row would duplicate the key)"
            )));
        }
        let mut fields: Vec<FieldId> = Vec::with_capacity(decl.fields.len());
        for fname in decl.fields {
            let field = model
                .field(fname)
                .ok_or_else(|| Error::programming(format!("{what}: unknown field '{fname}'")))?;
            if fields.contains(&field.id) {
                return Err(Error::programming(format!(
                    "{what} names field '{fname}' twice"
                )));
            }
            if field.inherited.is_some() {
                return Err(Error::programming(format!(
                    "{what}: field '{fname}' is inherited through _inherits; key fields must be \
                     columns of the model's own table"
                )));
            }
            if let Some(reason) = key_kind_refusal(&field.ty) {
                return Err(Error::programming(format!(
                    "{what}: field '{fname}' ({}) cannot be a key field: {reason}",
                    field.ty.type_name()
                )));
            }
            if field.column().is_none() {
                return Err(Error::programming(format!(
                    "{what}: field '{fname}' is not stored; key fields must be columns of the \
                     model's own table"
                )));
            }
            fields.push(field.id);
        }
        keys.push(BusinessKey {
            name,
            full_name: obj.full_name.clone(),
            fields,
            message: decl.message,
            nulls_distinct: decl.nulls_distinct,
        });
    }
    Ok(keys)
}
