// SPDX-License-Identifier: MIT OR Apache-2.0
//! Columns that the browser's importer and exporter can actually represent for a given user.

use erp_fields::{Error, FieldType, Result, Value};
use erp_orm::{FieldOp, Operation, Records, Transaction};
use erp_registry::FieldSpec;
use std::collections::{HashMap, HashSet};

use super::{ExportRights, GroupExportRights};

/// A selectable column. Paths for explicit relation identifiers use the loader's slash syntax.
#[derive(Debug, PartialEq, Eq)]
pub struct ExchangeField {
    pub name: String,
    pub label: String,
    /// A create requires an input for this field (there is no declared default).
    pub required: bool,
}

fn supported(field: &FieldSpec, import: bool) -> bool {
    if field.magic
        || field.secret
        || field.encrypted
        || matches!(
            field.ty,
            FieldType::Id
                | FieldType::RowVersion
                | FieldType::Binary { .. }
                | FieldType::Json
                | FieldType::Reference { .. }
                | FieldType::Many2oneReference { .. }
                | FieldType::Selection { values: None, .. }
        )
    {
        return false;
    }
    if import {
        return !field.readonly
            && !field.is_generated()
            && ((field.compute.is_none() && field.related.is_none()) || field.inverse.is_some())
            && !matches!(
                field.ty,
                FieldType::DateRange { .. } | FieldType::DatetimeRange { .. }
            );
    }
    // One-to-many export can expand one record into many rows. The list export
    // offers single-cell columns, while load supports one-to-many child paths.
    !matches!(field.ty, FieldType::One2many { .. })
}

async fn fields(
    records: &Records,
    tx: &mut Transaction,
    import: bool,
) -> Result<Vec<ExchangeField>> {
    // One policy-filtered projection for the whole model. The ORM takes both field groups and
    // model-specific policy overrides into account. Requesting only readonly avoids selection calls.
    let descriptions = records.fields_get(tx, None, Some(&["readonly"])).await?;
    let mut columns = Vec::new();
    let mut related_access = HashMap::new();
    for (name, attributes) in descriptions {
        let Some(field) = records.spec().field(&name) else {
            continue;
        };
        if name == "id" {
            columns.push(ExchangeField {
                name,
                label: "External ID".into(),
                required: false,
            });
            continue;
        }
        if !supported(field, import)
            || import && attributes.get("readonly") != Some(&Value::Bool(false))
        {
            continue;
        }
        if let Some(target) = field.ty.comodel() {
            let can_read = if let Some(&allowed) = related_access.get(target) {
                allowed
            } else {
                let allowed = records
                    .env()
                    .records(target)?
                    .has_access(tx, Operation::Read)
                    .await?;
                related_access.insert(target.to_owned(), allowed);
                allowed
            };
            if !can_read {
                continue;
            }
        }
        columns.push(ExchangeField {
            name: name.clone(),
            label: field.string.clone(),
            required: field.required && field.default.is_none() && field.compute.is_none(),
        });
        if import && field.ty.comodel().is_some() {
            for (suffix, label) in [("id", "External ID"), (".id", "Database ID")] {
                columns.push(ExchangeField {
                    name: format!("{name}/{suffix}"),
                    label: format!("{} / {label}", field.string),
                    required: false,
                });
            }
        }
    }
    Ok(columns)
}

/// Writable input fields for an ordinary governed import.
pub async fn import_fields(records: &Records, tx: &mut Transaction) -> Result<Vec<ExchangeField>> {
    records.check_access(tx, Operation::Create).await?;
    records.check_access(tx, Operation::Write).await?;
    fields(records, tx, true).await
}

/// Readable export columns after enforcing the same export admission as the data operation.
pub async fn export_fields(records: &Records, tx: &mut Transaction) -> Result<Vec<ExchangeField>> {
    records.check_access(tx, Operation::Read).await?;
    if !GroupExportRights.may_export(tx, records.env()).await? {
        return Err(Error::access(super::export::NO_RIGHTS));
    }
    fields(records, tx, false).await
}

/// Preserve list column order, add required fields, and avoid including a relation twice when
/// the list already requested its explicit identifier. A model without a list still gets headers.
pub fn template_columns<'a>(
    fields: &'a [ExchangeField],
    preferred: &[String],
) -> Vec<&'a ExchangeField> {
    let by_name: HashMap<&str, _> = fields
        .iter()
        .map(|field| (field.name.as_str(), field))
        .collect();
    let mut selected = Vec::new();
    let mut roots = HashSet::new();
    for name in preferred {
        if let Some(field) = by_name.get(name.as_str()) {
            let root = field.name.split('/').next().unwrap_or(&field.name);
            if roots.insert(root) {
                selected.push(*field);
            }
        }
    }
    for field in fields {
        if field.required && roots.insert(field.name.as_str()) {
            selected.push(field);
        }
    }
    if selected.is_empty() {
        selected.extend(
            fields
                .iter()
                .filter(|field| field.name != "id" && !field.name.contains('/')),
        );
    }
    selected
}

/// Accept hand-mapped relation paths from earlier CSV imports. At every hop check the
/// caller's access before looking at its target. The loader rechecks record rules as it writes.
pub async fn validate_import_columns(
    records: &Records,
    tx: &mut Transaction,
    columns: &[&str],
) -> Result<()> {
    let available = import_fields(records, tx).await?;
    let known: HashSet<_> = available.iter().map(|f| f.name.as_str()).collect();
    let mut unique = HashSet::new();
    for &column in columns {
        if !unique.insert(column) {
            return Err(Error::validation(format!(
                "The column '{column}' is mapped twice."
            )));
        }
        if known.contains(column) {
            continue;
        }
        let parts = super::path::checked_path(column)?;
        let unavailable =
            || Error::validation(format!("The column '{column}' is unavailable for import."));
        let (last, parents) = parts.split_last().ok_or_else(unavailable)?;
        if parents.is_empty() {
            return Err(unavailable());
        }
        let mut current = records.clone();
        for name in parents {
            let field = current.spec().field(name).ok_or_else(unavailable)?;
            if !supported(field, true)
                || !tx
                    .has_field_access(current.env(), current.model_name(), name, FieldOp::Write)
                    .await?
                || !tx
                    .has_field_access(current.env(), current.model_name(), name, FieldOp::Read)
                    .await?
            {
                return Err(unavailable());
            }
            let target = field.ty.comodel().ok_or_else(unavailable)?;
            current = current.env().records(target)?;
            current.check_access(tx, Operation::Read).await?;
        }
        if !matches!(last.as_str(), "id" | ".id") {
            let field = current.spec().field(last).ok_or_else(unavailable)?;
            if !supported(field, true)
                || !tx
                    .has_field_access(current.env(), current.model_name(), last, FieldOp::Read)
                    .await?
                || !tx
                    .has_field_access(current.env(), current.model_name(), last, FieldOp::Write)
                    .await?
            {
                return Err(unavailable());
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn template_keeps_list_order_and_adds_each_required_input_once() {
        let fields = [
            ("id", false),
            ("name", true),
            ("owner", true),
            ("owner/id", false),
            ("email", false),
        ]
        .map(|(name, required)| ExchangeField {
            name: name.into(),
            label: name.into(),
            required,
        });
        let preferred = ["readonly_display", "email", "owner/id", "email"].map(String::from);
        let names: Vec<_> = template_columns(&fields, &preferred)
            .into_iter()
            .map(|field| field.name.as_str())
            .collect();
        assert_eq!(names, ["email", "owner/id", "name"]);
    }
}
