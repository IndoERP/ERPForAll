//! `ir.default` runtime logic: field defaults resolution, scoping and caching (BASE §4.1).

use std::sync::Arc;

use erp_fields::convert::value_to_json;
use erp_fields::{Error, FieldType, RecId, Result, Value, ValueMap};
use erp_registry::Registry;
use rustc_hash::FxHashMap;

use crate::Transaction;
use crate::env::Env;
use crate::field_access::FieldOp;
use crate::ormcache::OrmCacheKey;
use crate::records::Records;

/// Convert a `serde_json::Value` to an `erp_fields::Value`.
pub fn json_to_value(j: &serde_json::Value) -> Value {
    match j {
        serde_json::Value::Null => Value::False,
        serde_json::Value::Bool(b) => Value::Bool(*b),
        serde_json::Value::Number(n) => {
            if let Some(i) = n.as_i64() {
                Value::Int(i)
            } else if let Some(f) = n.as_f64() {
                Value::Float(f)
            } else {
                Value::Str(n.to_string().into())
            }
        }
        serde_json::Value::String(s) => Value::Str(s.clone().into()),
        serde_json::Value::Array(arr) => Value::list(arr.iter().map(json_to_value)),
        serde_json::Value::Object(obj) => {
            let mut map = ValueMap::new();
            for (k, v) in obj {
                map.insert(k.clone(), json_to_value(v));
            }
            Value::Map(Arc::new(map))
        }
    }
}

/// Returns whether the `ir.default` model is registered in the registry.
/// When the base addon is not installed, no stored defaults exist.
#[inline]
pub fn is_available(registry: &Registry) -> bool {
    registry.model("ir.default").is_some()
}

/// Retrieve default values for a model (for current user and company) ordered by precedence.
pub async fn get_model_defaults(
    tx: &mut Transaction,
    env: &Env,
    model_name: &str,
    condition: Option<&str>,
) -> Result<FxHashMap<String, Value>> {
    let registry = env.registry();
    if registry.model(model_name).is_none() {
        return Err(Error::validation(format!(
            "Model '{model_name}' is not in the registry"
        )));
    }
    if registry.model("ir.default").is_none() {
        return Err(Error::validation(
            "Model 'ir.default' is not in the registry",
        ));
    }
    if registry.model("ir.model.fields").is_none() {
        return Err(Error::validation(
            "Model 'ir.model.fields' is not in the registry",
        ));
    }

    let uid = env.uid();
    // validated against the subject's newest resolution, also under sudo (review S1)
    let company_id = tx.companies(env).await?.first().map(|c| c.get());

    // Check ormcache
    let cache_key = OrmCacheKey::new(
        "ir.default",
        "_get_model_defaults",
        vec![
            Value::Int(uid),
            company_id.map_or(Value::False, Value::Int),
            Value::str(model_name),
            condition.map_or(Value::False, Value::str),
        ],
    );
    if let Some(Value::Map(m)) = tx.ormcache_get("ir.default", &cache_key).await? {
        let mut out = FxHashMap::default();
        for (k, v) in m.iter() {
            out.insert(k.clone(), v.clone());
        }
        return Ok(out);
    }

    let sql = r#"
        SELECT f.name, d.json_value
        FROM ir_default d
        JOIN ir_model_fields f ON d.field_id = f.id
        WHERE f.model = $1
          AND (d.user_id IS NULL OR d.user_id = $2)
          AND (d.company_id IS NULL OR d.company_id = $3)
          AND (d.condition = $4 OR ($4 IS NULL AND d.condition IS NULL))
        ORDER BY d.user_id, d.company_id, d.id
    "#;

    let user_param = Some(uid);
    let rows = tx
        .query(sql, &[&model_name, &user_param, &company_id, &condition])
        .await?;

    let mut result = FxHashMap::default();
    let mut map = ValueMap::new();
    for row in rows {
        let name: String = row.try_get(0).map_err(|e| Error::db(e.to_string()))?;
        let json_text: String = row.try_get(1).map_err(|e| Error::db(e.to_string()))?;
        if let std::collections::hash_map::Entry::Vacant(e) = result.entry(name) {
            let j: serde_json::Value = serde_json::from_str(&json_text).map_err(|err| {
                Error::validation(format!(
                    "Invalid JSON default value for field '{}': {err}",
                    e.key()
                ))
            })?;
            let val = json_to_value(&j);
            map.insert(e.key().clone(), val.clone());
            e.insert(val);
        }
    }

    tx.ormcache_add("ir.default", cache_key, Value::Map(Arc::new(map)))
        .await?;
    Ok(result)
}

/// Set a default value for a model and field.
#[allow(clippy::too_many_arguments)]
pub async fn set(
    tx: &mut Transaction,
    env: &Env,
    model_name: &str,
    field_name: &str,
    value: &Value,
    user_id: Option<RecId>,
    company_id: Option<RecId>,
    condition: Option<&str>,
) -> Result<()> {
    // 1. Validation of model and field
    let Some(model_spec) = env.registry().model(model_name) else {
        return Err(Error::validation(format!(
            "Invalid field {model_name}.{field_name}"
        )));
    };
    let Some(field_spec) = model_spec.field(field_name) else {
        return Err(Error::validation(format!(
            "Invalid field {model_name}.{field_name}"
        )));
    };

    crate::database_generated::check_assignment(model_spec, field_spec)?;

    if matches!(field_spec.ty, FieldType::Integer)
        && let Value::Int(i) = value
        && !(-2_147_483_648 < *i && *i < 2_147_483_647)
    {
        return Err(Error::validation(format!(
            "Invalid value for {model_name}.{field_name}: {i} is out of bounds (integers should be between -2,147,483,648 and 2,147,483,647)"
        )));
    }
    let fref = erp_fields::convert::FieldRef {
        model: model_name,
        field: field_name,
        label: &field_spec.string,
    };
    // HTML defaults are stored only after host admission, with their report retained.
    // Other kinds preserve ir.default's historical validation and JSON representation.
    let admitted_html;
    let value = if matches!(field_spec.ty, FieldType::Html) {
        let before = tx.mutations();
        let admitted = crate::html::input_cell(fref, &field_spec.ty, value)
            .and_then(|cell| tx.admit_html_cell(model_spec, field_spec, cell));
        match admitted {
            Ok(cell) => admitted_html = Value::from_cell(&cell),
            Err(error) => {
                tx.poison_after(before, &error);
                return Err(error);
            }
        }
        &admitted_html
    } else {
        value
    };
    if !matches!(field_spec.ty, FieldType::Html)
        && erp_fields::convert::to_cell(fref, &field_spec.ty, value).is_err()
    {
        let val_display = match value {
            Value::Str(s) => s.as_ref().to_string(),
            _ => value.to_string(),
        };
        return Err(Error::validation(format!(
            "Invalid value for {model_name}.{field_name}: {val_display}"
        )));
    }

    authorize_set(
        tx,
        env,
        model_spec.id,
        model_name,
        field_name,
        user_id,
        company_id,
    )
    .await?;

    // 4. Ensure ir_model and ir_model_fields record exists
    let field_id = ensure_field_id(tx, env, model_name, field_name).await?;

    let json_text = serde_json::to_string(&value_to_json(value)?).map_err(|e| {
        Error::validation(format!("Invalid value for {model_name}.{field_name}: {e}"))
    })?;

    let user_val = user_id.map(|u| u.get());
    let comp_val = company_id.map(|c| c.get());

    let existing_query = r#"
        SELECT id, json_value FROM ir_default
        WHERE field_id = $1
          AND (user_id = $2 OR ($2 IS NULL AND user_id IS NULL))
          AND (company_id = $3 OR ($3 IS NULL AND company_id IS NULL))
          AND (condition = $4 OR ($4 IS NULL AND condition IS NULL))
        LIMIT 1
    "#;
    let existing = tx
        .query(
            existing_query,
            &[&field_id, &user_val, &comp_val, &condition],
        )
        .await?;

    if let Some(row) = existing.first() {
        let id: i64 = row.try_get(0).map_err(|e| Error::db(e.to_string()))?;
        let prev_json: String = row.try_get(1).map_err(|e| Error::db(e.to_string()))?;
        if prev_json != json_text {
            tx.execute(
                "UPDATE ir_default SET json_value = $1 WHERE id = $2",
                &[&json_text, &id],
            )
            .await?;
            tx.invalidate_ormcache("ir.default");
            tx.cache.clear();
        }
    } else {
        tx.execute(
            r#"
            INSERT INTO ir_default (field_id, user_id, company_id, condition, json_value)
            VALUES ($1, $2, $3, $4, $5)
            "#,
            &[&field_id, &user_val, &comp_val, &condition, &json_text],
        )
        .await?;
        tx.invalidate_ormcache("ir.default");
        tx.cache.clear();
    }

    Ok(())
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum DefaultsRights {
    /// Superuser mode or `base.group_system`: every default.
    All,
    /// `base.group_user` only: reads all, changes the defaults whose `user_id` is the caller's.
    OwnOnly,
}

/// A refusal: logged like the engine's denials, then returned.
fn refuse(env: &Env, model_name: &str, field_name: &str, message: String) -> Error {
    tracing::info!(
        target: "erp::orm",
        uid = env.uid(),
        model = model_name,
        field = field_name,
        "ir.default access denied: {message}"
    );
    Error::access(message)
}

/// The rights of `env` on `ir.default`.
/// Superuser mode is the system's own and needs no lookup.
async fn defaults_rights(
    tx: &mut Transaction,
    env: &Env,
    model_name: &str,
    field_name: &str,
    action: &str,
) -> Result<DefaultsRights> {
    if env.su() || tx.has_group(env, "base.group_system").await? {
        return Ok(DefaultsRights::All);
    }
    if tx.has_group(env, "base.group_user").await? {
        return Ok(DefaultsRights::OwnOnly);
    }
    Err(refuse(
        env,
        model_name,
        field_name,
        format!("Only internal users may {action} defaults ({model_name}.{field_name})"),
    ))
}

/// The access rules of [`set`] for a caller outside superuser mode.
///
/// The principal's validated companies are
/// authoritative here; the `allowed_company_ids` context key (which picks the company on reads) is
/// not.
async fn authorize_set(
    tx: &mut Transaction,
    env: &Env,
    model: erp_registry::ModelId,
    model_name: &str,
    field_name: &str,
    user_id: Option<RecId>,
    company_id: Option<RecId>,
) -> Result<()> {
    if env.su() {
        return Ok(());
    }
    if defaults_rights(tx, env, model_name, field_name, "set").await? == DefaultsRights::OwnOnly {
        match user_id {
            None => {
                return Err(refuse(
                    env,
                    model_name,
                    field_name,
                    format!(
                        "Only system administrators may create global defaults ({model_name}.{field_name})"
                    ),
                ));
            }
            Some(u) if u.get() != env.uid() => {
                return Err(refuse(
                    env,
                    model_name,
                    field_name,
                    format!(
                        "Only system administrators may set the default of another user: user {} is \
                         not user {} ({model_name}.{field_name})",
                        u.get(),
                        env.uid()
                    ),
                ));
            }
            Some(_) => {}
        }
        if let Some(c) = company_id
            && !tx
                .companies(env)
                .await?
                .iter()
                .any(|allowed| allowed.get() == c.get())
        {
            return Err(refuse(
                env,
                model_name,
                field_name,
                format!(
                    "Only system administrators may set a default for a company that is not one of \
                     your allowed companies: company {} ({model_name}.{field_name})",
                    c.get()
                ),
            ));
        }
    }
    tx.check_field_names(env, model, [field_name], FieldOp::Write)
}

/// Retrieve default for specific field and scope.
pub async fn get(
    tx: &mut Transaction,
    _env: &Env,
    model_name: &str,
    field_name: &str,
    user_id: Option<RecId>,
    company_id: Option<RecId>,
    condition: Option<&str>,
) -> Result<Option<Value>> {
    let sql = r#"
        SELECT d.json_value
        FROM ir_default d
        JOIN ir_model_fields f ON d.field_id = f.id
        WHERE f.model = $1 AND f.name = $2
          AND (d.user_id = $3 OR ($3 IS NULL AND d.user_id IS NULL))
          AND (d.company_id = $4 OR ($4 IS NULL AND d.company_id IS NULL))
          AND (d.condition = $5 OR ($5 IS NULL AND d.condition IS NULL))
        LIMIT 1
    "#;
    let user_val = user_id.map(|u| u.get());
    let comp_val = company_id.map(|c| c.get());

    let rows = tx
        .query(
            sql,
            &[&model_name, &field_name, &user_val, &comp_val, &condition],
        )
        .await?;

    if let Some(row) = rows.first() {
        let json_text: String = row.try_get(0).map_err(|e| Error::db(e.to_string()))?;
        let j: serde_json::Value = serde_json::from_str(&json_text).map_err(|err| {
            Error::validation(format!(
                "Invalid JSON default value for field '{model_name}.{field_name}': {err}"
            ))
        })?;
        let val = json_to_value(&j);
        Ok(Some(val))
    } else {
        Ok(None)
    }
}

/// Discard defaults of many2one fields pointing to `records`.
pub async fn discard_records(tx: &mut Transaction, records: &Records) -> Result<()> {
    if records.is_empty() {
        return Ok(());
    }
    let model_name = records.model_name();
    let ids_json: Vec<String> = records
        .rec_ids()
        .iter()
        .map(|id| id.get().to_string())
        .collect();

    let sql = r#"
        DELETE FROM ir_default
        WHERE id IN (
            SELECT d.id
            FROM ir_default d
            JOIN ir_model_fields f ON d.field_id = f.id
            WHERE f.ttype = 'many2one'
              AND f.relation = $1
              AND d.json_value = ANY($2)
        )
    "#;
    let count = tx.execute(sql, &[&model_name, &ids_json]).await?;
    if count > 0 {
        tx.invalidate_ormcache("ir.default");
        tx.cache.clear();
    }
    Ok(())
}

/// A caller in `base.group_user` but not `base.group_system` may delete only their own personal
/// defaults.
///
/// # Errors
///
/// `Access` for a caller outside `base.group_user` and `base.group_system`, or for a matching row
/// the caller may not delete; the statements' and the group lookup's errors; `Validation` for a
/// value that has no JSON form.
pub async fn discard_values(
    tx: &mut Transaction,
    env: &Env,
    model_name: &str,
    field_name: &str,
    values: &[Value],
) -> Result<()> {
    let mut json_vals: Vec<String> = Vec::with_capacity(values.len());
    for v in values {
        let json_text = serde_json::to_string(&value_to_json(v)?).map_err(|e| {
            Error::validation(format!("Invalid value for {model_name}.{field_name}: {e}"))
        })?;
        json_vals.push(json_text);
    }

    let rights = defaults_rights(tx, env, model_name, field_name, "discard").await?;
    if rights == DefaultsRights::OwnOnly {
        let uid = env.uid();
        let foreign = tx
            .query(
                r#"
                SELECT 1
                FROM ir_default d
                JOIN ir_model_fields f ON d.field_id = f.id
                WHERE f.model = $1 AND f.name = $2
                  AND d.json_value = ANY($3)
                  AND d.user_id IS DISTINCT FROM $4::int8
                LIMIT 1
                "#,
                &[&model_name, &field_name, &json_vals, &uid],
            )
            .await?;
        if !foreign.is_empty() {
            return Err(refuse(
                env,
                model_name,
                field_name,
                format!(
                    "Only system administrators may discard the defaults of other users or \
                     global defaults ({model_name}.{field_name})"
                ),
            ));
        }
    }
    let sql = r#"
        DELETE FROM ir_default
        WHERE id IN (
            SELECT d.id
            FROM ir_default d
            JOIN ir_model_fields f ON d.field_id = f.id
            WHERE f.model = $1 AND f.name = $2
              AND d.json_value = ANY($3)
        )
    "#;
    let count = tx
        .execute(sql, &[&model_name, &field_name, &json_vals])
        .await?;
    if count > 0 {
        tx.invalidate_ormcache("ir.default");
        tx.cache.clear();
    }
    Ok(())
}

/// Ensure `ir_model` and `ir_model_fields` row exists for `(model_name, field_name)` and return `field_id`.
async fn ensure_field_id(
    tx: &mut Transaction,
    env: &Env,
    model_name: &str,
    field_name: &str,
) -> Result<i64> {
    let rows = tx
        .query(
            "SELECT id FROM ir_model_fields WHERE model = $1 AND name = $2 LIMIT 1",
            &[&model_name, &field_name],
        )
        .await?;
    if let Some(row) = rows.first() {
        return row.try_get(0).map_err(|e| Error::db(e.to_string()));
    }

    // Ensure ir_model row exists
    let model_rows = tx
        .query(
            "SELECT id FROM ir_model WHERE model = $1 LIMIT 1",
            &[&model_name],
        )
        .await?;
    let model_id: i64 = if let Some(row) = model_rows.first() {
        row.try_get(0).map_err(|e| Error::db(e.to_string()))?
    } else {
        let desc = env
            .registry()
            .model(model_name)
            .map_or(model_name, |m| m.description.as_str());
        let ins = tx
            .query(
                "INSERT INTO ir_model (name, model, state, transient) VALUES ($1, $2, 'base', false) RETURNING id",
                &[&desc, &model_name],
            )
            .await?;
        ins[0].try_get(0).map_err(|e| Error::db(e.to_string()))?
    };

    let field_spec = env
        .registry()
        .model(model_name)
        .and_then(|m| m.field(field_name));
    let ttype = field_spec.map_or("char", |f| f.ty.type_name());
    let string_label = field_spec.map_or(field_name, |f| &f.string);
    let relation = field_spec.and_then(|f| f.ty.comodel());

    let ins_field = tx
        .query(
            r#"
            INSERT INTO ir_model_fields (name, model_id, model, field_description, ttype, relation, state, store, readonly, required, copied, selectable, translate)
            VALUES ($1, $2, $3, $4, $5, $6, 'base', true, false, false, true, true, false)
            RETURNING id
            "#,
            &[&field_name, &model_id, &model_name, &string_label, &ttype, &relation],
        )
        .await?;
    ins_field[0]
        .try_get(0)
        .map_err(|e| Error::db(e.to_string()))
}
