//! Model and field reflection tables (`ir.model`, `ir.model.fields`, `ir.model.relation`) and XML IDs.

use std::sync::Arc;

use erp_fields::{Error, FieldType, Result};
use erp_registry::ModelKind;

use crate::Transaction;

/// Synchronize registry models and fields into reflection tables `ir_model`, `ir_model_fields`, and `ir_model_relation`
/// with external identifiers in `ir_model_data`.
pub async fn reflect_models(tx: &mut Transaction) -> Result<()> {
    let registry = Arc::clone(tx.env().registry());
    if registry.model("ir.model").is_none() || registry.model("ir.model.fields").is_none() {
        return Ok(());
    }

    let has_model_data = registry.model("ir.model.data").is_some();

    for model in registry.models() {
        // MRO is newest-first. An addon extension may contribute fields and methods for an
        // existing model, but it does not take ownership of that model's canonical XML ID.
        // Use the original declaration; a new/classically inherited model still has its own
        // declaration under its own name.
        let model_addon = model
            .mro
            .iter()
            .rev()
            .find(|d| d.name == model.name)
            .map(|d| d.addon)
            .ok_or_else(|| {
                Error::programming(format!(
                    "Model '{}' has no declaring model in its MRO",
                    model.name
                ))
            })?;

        // 1. Reflect ir.model
        let transient = model.kind == ModelKind::Transient;
        let model_rows = tx
            .query(
                "SELECT id FROM ir_model WHERE model = $1 LIMIT 1",
                &[&model.name],
            )
            .await?;
        let model_id: i64 = if let Some(row) = model_rows.first() {
            let id: i64 = row.try_get(0).map_err(|e| Error::db(e.to_string()))?;
            tx.execute(
                "UPDATE ir_model SET name = $1, state = 'base', transient = $2 WHERE id = $3",
                &[&model.description, &transient, &id],
            )
            .await?;
            id
        } else {
            let ins = tx
                .query(
                    r#"
                    INSERT INTO ir_model (name, model, state, transient)
                    VALUES ($1, $2, 'base', $3)
                    RETURNING id
                    "#,
                    &[&model.description, &model.name, &transient],
                )
                .await?;
            ins[0].try_get(0).map_err(|e| Error::db(e.to_string()))?
        };

        // XML ID for model: <addon>.model_<model_table>
        if has_model_data {
            let model_xml_name = format!("model_{}", model.name.replace('.', "_"));
            let xml_sql = r#"
                INSERT INTO ir_model_data (module, name, model, res_id, noupdate)
                VALUES ($1, $2, 'ir.model', $3, false)
                ON CONFLICT (module, name) DO UPDATE SET res_id = $3, model = 'ir.model'
            "#;
            tx.execute(xml_sql, &[&model_addon, &model_xml_name, &model_id])
                .await?;
        }

        // 2. Reflect ir.model.fields
        for field in &model.fields {
            let ttype = field.ty.type_name();
            let string_label = &field.string;
            let help = field.help;
            let index_val = if field.index.is_some() {
                Some("btree")
            } else {
                None
            };
            let relation = field.ty.comodel();
            let relation_field = match &field.ty {
                FieldType::One2many { inverse, .. } => Some(inverse.as_str()),
                _ => None,
            };
            let (relation_table, col1, col2) = match &field.ty {
                FieldType::Many2many {
                    relation: Some(rel),
                    ..
                } => (
                    Some(rel.table.as_str()),
                    Some(rel.column1.as_str()),
                    Some(rel.column2.as_str()),
                ),
                _ => (None, None, None),
            };

            let field_rows = tx
                .query(
                    "SELECT id FROM ir_model_fields WHERE model_id = $1 AND name = $2 LIMIT 1",
                    &[&model_id, &field.name],
                )
                .await?;
            let field_id: i64 = if let Some(row) = field_rows.first() {
                let fid: i64 = row.try_get(0).map_err(|e| Error::db(e.to_string()))?;
                tx.execute(
                    r#"
                    UPDATE ir_model_fields
                    SET field_description = $1, help = $2, ttype = $3, relation = $4,
                        relation_field = $5, relation_table = $6, column1 = $7, column2 = $8,
                        state = 'base', index = $9, store = $10, readonly = $11, required = $12,
                        copied = $13, related = $14, selectable = true, translate = false
                    WHERE id = $15
                    "#,
                    &[
                        &string_label,
                        &help,
                        &ttype,
                        &relation,
                        &relation_field,
                        &relation_table,
                        &col1,
                        &col2,
                        &index_val,
                        &field.store,
                        &field.readonly,
                        &field.required,
                        &field.copy,
                        &field.related,
                        &fid,
                    ],
                )
                .await?;
                fid
            } else {
                let ins = tx
                    .query(
                        r#"
                        INSERT INTO ir_model_fields (
                            name, model_id, model, field_description, help, ttype,
                            relation, relation_field, relation_table, column1, column2,
                            state, index, store, readonly, required, copied, related,
                            selectable, translate
                        ) VALUES (
                            $1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11,
                            'base', $12, $13, $14, $15, $16, $17, $18, false
                        ) RETURNING id
                        "#,
                        &[
                            &field.name,
                            &model_id,
                            &model.name,
                            &string_label,
                            &help,
                            &ttype,
                            &relation,
                            &relation_field,
                            &relation_table,
                            &col1,
                            &col2,
                            &index_val,
                            &field.store,
                            &field.readonly,
                            &field.required,
                            &field.copy,
                            &field.related,
                            &true,
                        ],
                    )
                    .await?;
                ins[0].try_get(0).map_err(|e| Error::db(e.to_string()))?
            };

            // If relation_table present, upsert ir.model.relation
            if let Some(tbl) = relation_table
                && registry.model("ir.model.relation").is_some()
            {
                let rel_rows = tx
                    .query(
                        "SELECT id FROM ir_model_relation WHERE name = $1 LIMIT 1",
                        &[&tbl],
                    )
                    .await?;
                if rel_rows.is_empty() {
                    tx.execute(
                            "INSERT INTO ir_model_relation (name, model_id, module) VALUES ($1, $2, $3)",
                            &[&tbl, &model_id, &model_addon],
                        )
                        .await?;
                }
            }

            // XML ID for field: <addon>.field_<model_table>__<field_name>
            if has_model_data {
                let field_addon = field.modules.first().copied().unwrap_or(model_addon);
                let field_xml_name =
                    format!("field_{}__{}", model.name.replace('.', "_"), field.name);
                let xml_sql = r#"
                    INSERT INTO ir_model_data (module, name, model, res_id, noupdate)
                    VALUES ($1, $2, 'ir.model.fields', $3, false)
                    ON CONFLICT (module, name) DO UPDATE SET res_id = $3, model = 'ir.model.fields'
                "#;
                tx.execute(xml_sql, &[&field_addon, &field_xml_name, &field_id])
                    .await?;
            }

            // 3. Reflect ir.model.fields.selection
            if let FieldType::Selection {
                values: Some(values),
                ..
            } = &field.ty
                && registry.model("ir.model.fields.selection").is_some()
            {
                let field_addon = field.modules.first().copied().unwrap_or(model_addon);
                for (index, (val_key, val_label)) in values.iter().enumerate() {
                    let sel_rows = tx
                        .query(
                            "SELECT id FROM ir_model_fields_selection WHERE field_id = $1 AND value = $2 LIMIT 1",
                            &[&field_id, &val_key],
                        )
                        .await?;
                    let sel_id: i64 = if let Some(row) = sel_rows.first() {
                        let sid: i64 = row.try_get(0).map_err(|e| Error::db(e.to_string()))?;
                        tx.execute(
                            r#"
                            UPDATE ir_model_fields_selection
                            SET sequence = $1,
                                name = jsonb_set(COALESCE(name, '{}'::jsonb), '{en_US}', to_jsonb($2::text))
                            WHERE id = $3
                            "#,
                            &[&(index as i32), &val_label, &sid],
                        )
                        .await?;
                        sid
                    } else {
                        let ins = tx
                            .query(
                                r#"
                                INSERT INTO ir_model_fields_selection (field_id, value, name, sequence)
                                VALUES ($1, $2, jsonb_build_object('en_US', $3::text), $4)
                                RETURNING id
                                "#,
                                &[&field_id, &val_key, &val_label, &(index as i32)],
                            )
                            .await?;
                        ins[0].try_get(0).map_err(|e| Error::db(e.to_string()))?
                    };

                    if has_model_data {
                        let xmodel = model.name.replace('.', "_");
                        let xvalue = val_key.replace(['.', ' '], "_").to_ascii_lowercase();
                        let sel_xml_name = format!("selection__{xmodel}__{}__{xvalue}", field.name);
                        let xml_sql = r#"
                            INSERT INTO ir_model_data (module, name, model, res_id, noupdate)
                            VALUES ($1, $2, 'ir.model.fields.selection', $3, false)
                            ON CONFLICT (module, name) DO UPDATE SET res_id = $3, model = 'ir.model.fields.selection'
                        "#;
                        tx.execute(xml_sql, &[&field_addon, &sel_xml_name, &sel_id])
                            .await?;
                    }
                }
            }
        }
    }

    Ok(())
}
