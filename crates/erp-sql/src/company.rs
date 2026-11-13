// SPDX-License-Identifier: MIT OR Apache-2.0
//! Prepared property fallbacks and their shared scalar SQL expression.
//! This module performs no database access or authorization. The ORM validates the company
//! against the caller, resolves snapshot-local defaults and supplies admitted scalar values.
use crate::param::{Param, Scalar};
use crate::{literal, quote};
use erp_fields::{Cell, Error, FieldType, Result, Storage, Value, serde_json};
use erp_registry::{FieldId, FieldSpec, ModelId, ModelSpec, Registry};
use std::collections::BTreeMap;
use std::sync::Arc;

/// A validated company and typed, prepared fallbacks. Values are bound, never SQL text.
#[derive(Clone)]
#[non_exhaustive]
pub struct CompanyValues {
    company: i64,
    values: BTreeMap<String, serde_json::Value>,
    types: BTreeMap<String, FieldType>,
    bytes: usize,
}
impl std::fmt::Debug for CompanyValues {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CompanyValues")
            .field("fields", &self.values.len())
            .finish_non_exhaustive()
    }
}
fn key(model: ModelId, field: FieldId) -> String {
    format!("{}.{}", model.index(), field.index())
}
/// The diagnostic used by every context-free property compiler.
pub fn missing(model: &ModelSpec, field: &FieldSpec) -> Error {
    Error::programming(format!(
        "{}.{}: company-dependent SQL requires prepared company values",
        model.name, field.name
    ))
}
impl CompanyValues {
    /// Construct for a positive company id. This is not a company-access grant.
    /// # Errors
    /// Validation if `company` is not positive.
    pub fn new(company: i64) -> Result<Self> {
        if company <= 0 {
            return Err(Error::validation(
                "company-dependent SQL requires a positive company id",
            ));
        }
        Ok(Self {
            company,
            values: BTreeMap::new(),
            types: BTreeMap::new(),
            bytes: 0,
        })
    }
    /// Add one already admitted fallback in the field's logical scalar representation.
    /// # Errors
    /// Programming for a foreign field, unsupported field or mismatched cell; Validation for
    /// a non-finite float or a prepared payload larger than the storage admission limit.
    pub fn insert(&mut self, model: &ModelSpec, field: &FieldSpec, fallback: &Cell) -> Result<()> {
        let invalid = || {
            Error::programming(format!(
                "{}.{}: invalid prepared company fallback type",
                model.name, field.name
            ))
        };
        if model
            .fields
            .get(field.id.index())
            .is_none_or(|f| !std::ptr::eq(f, field))
            || !field.company_dependent
            || !fallback.fits(field.ty.storage())
            || matches!(fallback, Cell::Id(id) if *id <= 0)
        {
            return Err(invalid());
        }
        if matches!(fallback, Cell::Str(text) if text.len() > 16 * 1024 * 1024) {
            return Err(Error::validation(
                "prepared company context exceeds the byte limit",
            ));
        }
        scalar_cast(&field.ty).ok_or_else(invalid)?;
        let json = match fallback {
            Cell::Null => match field.ty.storage() {
                Storage::Bool => serde_json::Value::Bool(false),
                Storage::Int | Storage::Float => serde_json::json!(0),
                Storage::Decimal => serde_json::json!("0"),
                _ => serde_json::Value::Null,
            },
            Cell::Float(v) if !v.is_finite() => {
                return Err(Error::validation("company-dependent float must be finite"));
            }
            Cell::Decimal(v) => serde_json::json!(v.normalize().to_string()),
            Cell::DateTime(v) => {
                let micros = v.time().3;
                serde_json::json!(if micros == 0 {
                    v.to_string()
                } else {
                    format!("{v}.{micros:06}")
                })
            }
            _ => erp_fields::convert::value_to_json(&Value::from_cell(fallback))?,
        };
        let key = key(model.id, field.id);
        let old = self
            .values
            .get(&key)
            .map_or(0, |v| v.to_string().len() + key.len() + 64);
        let added = json.to_string().len() + key.len() + 64;
        let bytes = self
            .bytes
            .checked_sub(old)
            .and_then(|n| n.checked_add(added))
            .ok_or_else(|| Error::validation("prepared company context exceeds the byte limit"))?;
        if bytes > 16 * 1024 * 1024 {
            return Err(Error::validation(
                "prepared company context exceeds the byte limit",
            ));
        }
        self.bytes = bytes;
        self.types.insert(key.clone(), field.ty.clone());
        self.values.insert(key, json);
        Ok(())
    }
    /// Refuse a property dependency whose fallback was not prepared.
    /// # Errors
    /// Programming naming the missing field.
    pub fn validate(&self, registry: &Registry, fields: &[(ModelId, FieldId)]) -> Result<()> {
        for &(model, field) in fields {
            let model = registry.model_by_id(model);
            let field = model.field_by_id(field);
            if field.company_dependent {
                let key = key(model.id, field.id);
                if !self.values.contains_key(&key) {
                    return Err(missing(model, field));
                }
                if self.types.get(&key) != Some(&field.ty) {
                    return Err(Error::programming(format!(
                        "{}.{}: invalid prepared company fallback type",
                        model.name, field.name
                    )));
                }
            }
        }
        Ok(())
    }
    /// Merge additional prepared fields for the same validated company.
    /// # Errors
    /// Programming if the contexts belong to different companies.
    pub fn extend(&mut self, other: Self) -> Result<()> {
        if self.company != other.company {
            return Err(Error::programming(
                "cannot merge prepared values of different companies",
            ));
        }
        let mut bytes = self.bytes;
        for (key, value) in &other.values {
            let ty = other
                .types
                .get(key)
                .ok_or_else(|| Error::programming("prepared company fallback type is missing"))?;
            if self.types.get(key).is_some_and(|own| own != ty) {
                return Err(Error::programming(
                    "cannot merge different prepared field types",
                ));
            }
            let old = self
                .values
                .get(key)
                .map_or(0, |v| v.to_string().len() + key.len() + 64);
            let added = value.to_string().len() + key.len() + 64;
            bytes = bytes
                .checked_sub(old)
                .and_then(|n| n.checked_add(added))
                .ok_or_else(|| {
                    Error::validation("prepared company context exceeds the byte limit")
                })?;
            if bytes > 16 * 1024 * 1024 {
                return Err(Error::validation(
                    "prepared company context exceeds the byte limit",
                ));
            }
        }
        self.bytes = bytes;
        self.types.extend(other.types);
        self.values.extend(other.values);
        Ok(())
    }
    /// One bound JSON document for a statement component; it is never placed in a shape cache.
    /// # Errors
    /// Validation when the complete document exceeds the property-map admission limit.
    pub fn param(&self) -> Result<Param> {
        let text = serde_json::json!({"company": self.company.to_string(), "values": self.values})
            .to_string();
        if text.len() > 16 * 1024 * 1024 {
            return Err(Error::validation(
                "prepared company context exceeds the byte limit",
            ));
        }
        Ok(Param::Scalar(Scalar::Text(Arc::from(text))))
    }
}
fn scalar_cast(ty: &FieldType) -> Option<&'static str> {
    Some(match ty {
        FieldType::Boolean => "boolean",
        FieldType::Integer => "int4",
        FieldType::Float { digits: None } => "float8",
        FieldType::Float { digits: Some(_) } | FieldType::Monetary { .. } => "numeric",
        FieldType::Many2one { .. } => "int8",
        FieldType::Date => "date",
        FieldType::Datetime => "timestamp",
        FieldType::Char { .. }
        | FieldType::Text
        | FieldType::Html
        | FieldType::Selection { .. } => "text",
        _ => return None,
    })
}
/// The select item that loads `field`'s stored map with every reference to a missing row set to
/// JSON null, when `field` is a company-dependent many2one; `None` for any other field (select
/// `column` as it is). `column` is the column's own text, quoted and qualified for the statement.
///
/// A reference whose target is gone reads as empty, the same rule as [`expression`]: the entry
/// stays (as null) so it never falls back to the default. An entry that is not a plain integer
/// passes through for the decoder to refuse. The existence test ignores record rules, as a
/// foreign key would.
#[must_use]
pub fn load_column(registry: &Registry, field: &FieldSpec, column: &str) -> Option<String> {
    if !field.company_dependent || field.inherited.is_some() {
        return None;
    }
    let FieldType::Many2one { comodel, .. } = &field.ty else {
        return None;
    };
    // an unknown comodel leaves the column unfiltered here; `prepare_properties` and the
    // deletion code refuse it loudly when the field is used
    let target = registry.model(comodel);
    debug_assert!(
        target.is_some(),
        "unknown comodel {comodel} of {}",
        field.name
    );
    let target = target?;
    let value = "\"__pe\".\"value\"";
    let text = format!("{value}#>>'{{}}'");
    Some(format!(
        "CASE WHEN jsonb_typeof({column}) = 'object' THEN COALESCE((SELECT jsonb_object_agg(\"__pe\".\"key\", \
         CASE WHEN jsonb_typeof({value}) = 'number' THEN CASE WHEN ({text}) ~ '^[0-9]{{1,18}}$' THEN \
         CASE WHEN EXISTS (SELECT 1 FROM {} AS \"__pt\" WHERE \"__pt\".\"id\" = ({text})::int8) THEN {value} \
         ELSE 'null'::jsonb END ELSE {value} END ELSE {value} END) FROM jsonb_each({column}) AS \"__pe\"), {column}) \
         ELSE {column} END AS {}",
        quote(&target.table),
        quote(field.name)
    ))
}

/// An effective scalar on `alias`. Callers validate dependencies before rendering.
/// Missing joined source rows stay absent; JSON null suppresses fallback. Dangling many2one
/// targets normalize to absent independently of the operator's comodel access restrictions.
/// # Errors
/// Programming for unsupported logical types or a zero parameter slot.
pub fn expression(
    registry: &Registry,
    model: &ModelSpec,
    field: &FieldSpec,
    alias: &str,
    slot: usize,
) -> Result<String> {
    if slot == 0 {
        return Err(missing(model, field));
    }
    let cast = scalar_cast(&field.ty).ok_or_else(|| missing(model, field))?;
    let context = format!("(${slot}::text::jsonb)");
    let raw = format!("{}.{}", quote(alias), quote(field.name));
    let value = format!(
        "((CASE WHEN {}.\"id\" IS NULL THEN NULL ELSE COALESCE({raw}->({context}->>'company'), {context}->'values'->{}) END)#>>'{{}}')::{cast}",
        quote(alias),
        literal(&key(model.id, field.id))
    );
    if let FieldType::Many2one { comodel, .. } = &field.ty {
        let target = registry
            .model(comodel)
            .ok_or_else(|| Error::programming(format!("unknown property comodel {comodel}")))?;
        Ok(format!(
            "(CASE WHEN EXISTS (SELECT 1 FROM {} AS \"__property_target\" WHERE \"__property_target\".\"id\" = {value}) THEN {value} ELSE NULL END)",
            quote(&target.table)
        ))
    } else {
        Ok(format!("({value})"))
    }
}

impl CompanyValues {
    /// Conservative fallback evaluation used only to prove a partial-index prefilter safe.
    /// Unknown patterns, relation visibility and principal-dependent values never prune rows.
    pub(crate) fn fallback_matches(
        &self,
        model: &ModelSpec,
        field: &FieldSpec,
        condition: &erp_domain::Condition,
    ) -> Option<bool> {
        use crate::param::{ScalarKind, falsy_value, to_scalar};
        use erp_domain::{Operator as Op, Value as D};
        if matches!(field.ty, FieldType::Many2one { .. }) {
            return None;
        }
        let json = self.values.get(&key(model.id, field.id))?;
        let scalar = match (&field.ty, json) {
            (_, serde_json::Value::Null) => falsy_value(&field.ty),
            (FieldType::Boolean, _) => None,
            (ty, _) => {
                let value = match json {
                    serde_json::Value::String(v) => D::Str(v.clone()),
                    serde_json::Value::Number(v) => {
                        if let Some(i) = v.as_i64() {
                            D::Int(i)
                        } else {
                            D::Float(v.as_f64()?)
                        }
                    }
                    _ => return None,
                };
                Some(match to_scalar(ScalarKind::of_field(ty)?, &value) {
                    Ok(v) => v,
                    Err(_) => return None,
                })
            }
        };
        let equal = |value: &D| -> Option<bool> {
            if matches!(field.ty, FieldType::Boolean) {
                return match value {
                    D::Bool(v) => Some(json.as_bool()? == *v),
                    _ => None,
                };
            }
            if matches!(value, D::Bool(false)) {
                return Some(
                    json.is_null()
                        || scalar
                            .as_ref()
                            .is_some_and(|s| Some(s) == falsy_value(&field.ty).as_ref()),
                );
            }
            let value = match to_scalar(ScalarKind::of_field(&field.ty)?, value) {
                Ok(v) => v,
                Err(_) => return None,
            };
            Some(scalar.as_ref().is_some_and(|s| *s == value))
        };
        match condition.operator() {
            Op::In | Op::NotIn => {
                let D::List(values) = condition.value() else {
                    return None;
                };
                let mut matched = false;
                for value in values {
                    matched |= equal(value)?;
                }
                Some(if condition.operator() == Op::In {
                    matched
                } else {
                    !matched
                })
            }
            Op::Eq => equal(condition.value()),
            Op::Ne => equal(condition.value()).map(|v| !v),
            Op::Lt | Op::Le | Op::Gt | Op::Ge => {
                let Some(scalar) = scalar else {
                    return Some(false);
                };
                let value = match to_scalar(ScalarKind::of_field(&field.ty)?, condition.value()) {
                    Ok(v) => v,
                    Err(_) => return None,
                };
                let cmp = scalar.partial_cmp_same(&value)?;
                Some(match condition.operator() {
                    Op::Lt => cmp.is_lt(),
                    Op::Le => !cmp.is_gt(),
                    Op::Gt => cmp.is_gt(),
                    Op::Ge => !cmp.is_lt(),
                    _ => return None,
                })
            }
            op if op.is_like()
                && matches!(
                    field.ty,
                    FieldType::Char { .. }
                        | FieldType::Text
                        | FieldType::Html
                        | FieldType::Selection { .. }
                ) =>
            {
                if json.is_null() {
                    return Some(op.is_negative());
                }
                let text = json.as_str()?;
                let D::Str(pattern) = condition.value() else {
                    return None;
                };
                if !text.is_ascii() || !pattern.is_ascii() || pattern.contains(['%', '_', '\\']) {
                    return None;
                }
                let insensitive = matches!(op.positive(), Op::ILike | Op::EqILike);
                let (text, pattern) = if insensitive {
                    (text.to_ascii_lowercase(), pattern.to_ascii_lowercase())
                } else {
                    (text.to_owned(), pattern.to_owned())
                };
                let matched = if matches!(op.positive(), Op::EqLike | Op::EqILike) {
                    text == pattern
                } else {
                    text.contains(&pattern)
                };
                Some(if op.is_negative() { !matched } else { matched })
            }
            _ => None,
        }
    }
}
