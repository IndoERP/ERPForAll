// SPDX-License-Identifier: MIT OR Apache-2.0
//! Stock upgrade steps: missing reservation lead times keep their former zero (1.1); the stock unit
//! policy moves from the Settings store to its declared system parameters (1.5).
use erp_sdk::migrations::SemverVersion;
use erp_sdk::prelude::*;

#[erp_sdk::migration(addon="stock",version="1.1",stage=pre)]
async fn reservation_lead_times(tx: &mut Transaction, _from: &SemverVersion) -> Result<()> {
    let rows = tx
        .query("SELECT to_regclass('stock_picking_type') IS NOT NULL", &[])
        .await?;
    let row = rows
        .first()
        .ok_or_else(|| Error::internal("Operation type migration probe returned no row."))?;
    let exists: bool = row
        .try_get(0)
        .map_err(|e| Error::db("Operation type migration probe failed.").with_source(e))?;
    if !exists {
        return Ok(());
    }
    // A legacy missing integer read as zero. Existing nonzero configuration is not reset or
    // clamped: the named checks refuse incompatible values and roll the upgrade back.
    tx.execute("UPDATE stock_picking_type SET reservation_days_before=0 WHERE reservation_days_before IS NULL",&[]).await?;
    tx.execute("UPDATE stock_picking_type SET reservation_days_before_priority=0 WHERE reservation_days_before_priority IS NULL",&[]).await?;
    Ok(())
}

fn db_error(what: &str, error: &dyn std::fmt::Display) -> Error {
    Error::db(format!("stock 1.5 policy move, {what}: {error}"))
}

/// The audit actor of a parameter text this migration replaces.
const POLICY_ACTOR: &str = "migration:stock@1.5";

/// Move the stock unit policy from the global Settings store to its declared system parameters
/// ("Settings to TP"). A stored value becomes the parameter row of the same key in
/// canonical text; the effective value does not change, so no Settings revision advances and no
/// audit row is written, except when a non-empty parameter row held a different text (the Settings
/// value wins, the replaced text is audited). A value outside its declaration, or a final state the
/// backoff group refuses, stops the upgrade naming the key. Source rows stay in place, unread, for
/// one release (lead decision L1). Runs before registry publication creates the default rows.
#[erp_sdk::migration(addon = "stock", version = "1.5", stage = post)]
async fn quant_policy_parameters(tx: &mut Transaction, _from: &SemverVersion) -> Result<()> {
    let env = tx.env();
    let params = erp_sdk::params::registry(&env)?;
    let keys: Vec<String> = crate::settings::QUANT_KEYS
        .iter()
        .map(|key| (*key).to_owned())
        .collect();
    // The scope row lock serialises with a Settings writer of an older node.
    let stored = tx
        .query(
            "SELECT v.setting_key::text, v.value_text::text, s.id::int8, s.revision::text \
             FROM ir_config_setting_value v JOIN ir_config_setting_scope s ON s.id = v.scope_id \
             WHERE s.identity = 'global' AND v.setting_key = ANY($1) ORDER BY v.setting_key FOR UPDATE OF s",
            &[&keys],
        )
        .await?;
    let current = tx
        .query(
            "SELECT key::text, value::text FROM ir_config_parameter WHERE key = ANY($1)",
            &[&keys],
        )
        .await?;
    let mut present = std::collections::BTreeMap::new();
    for row in &current {
        let key: String = row.try_get(0).map_err(|e| db_error("parameter key", &e))?;
        let value: Option<String> = row
            .try_get(1)
            .map_err(|e| db_error("parameter value", &e))?;
        present.insert(key, value.filter(|text| !text.is_empty()));
    }
    let mut moved = std::collections::BTreeMap::new();
    for row in &stored {
        let key: String = row.try_get(0).map_err(|e| db_error("setting key", &e))?;
        let text: String = row.try_get(1).map_err(|e| db_error("setting value", &e))?;
        let scope: i64 = row.try_get(2).map_err(|e| db_error("scope", &e))?;
        let revision: String = row.try_get(3).map_err(|e| db_error("revision", &e))?;
        let decl = params.get(&key).ok_or_else(|| {
            Error::programming(format!(
                "stock 1.5 policy move: {key} is not a declared parameter."
            ))
        })?;
        // `<key> takes <bounds>.`: the upgrade stops, nothing is clamped.
        let Value::Int(value) = decl.check_text(Some(&text))? else {
            return Err(Error::programming(format!(
                "{key} is not declared as an integer."
            )));
        };
        let canonical = value.to_string();
        match present.get(&key).cloned().flatten() {
            Some(old) if old == canonical => {}
            previous => {
                if let Some(old) = &previous {
                    let subject = env.uid().to_string();
                    tx.execute(
                        "INSERT INTO ir_config_setting_audit(scope_id,actor,subject_uid,setting_key,old_value,new_value,revision,create_date) \
                         VALUES ($1,$2,$3,$4,$5,$6,$7,CURRENT_TIMESTAMP AT TIME ZONE 'UTC')",
                        &[&scope, &POLICY_ACTOR, &subject, &key, old, &canonical, &revision],
                    )
                    .await?;
                }
                tx.execute(
                    "INSERT INTO ir_config_parameter(key, value) VALUES ($1, $2) \
                     ON CONFLICT (key) DO UPDATE SET value = EXCLUDED.value",
                    &[&key, &canonical],
                )
                .await?;
            }
        }
        moved.insert(key, Value::Int(value));
    }
    if moved.is_empty() {
        return Ok(());
    }
    // The final state of every key: moved, an existing parameter row or the default.
    let mut values = std::collections::BTreeMap::new();
    for key in keys {
        let value = match moved.remove(&key) {
            Some(value) => value,
            None => {
                let decl = params.get(&key).ok_or_else(|| {
                    Error::programming(format!(
                        "stock 1.5 policy move: {key} is not a declared parameter."
                    ))
                })?;
                decl.read_text(present.get(&key).cloned().flatten().as_deref())?
            }
        };
        values.insert(key, value);
    }
    params.validate_groups(&values)?;
    tx.invalidate_model("ir.config_parameter", None).await
}
