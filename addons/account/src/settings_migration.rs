// SPDX-License-Identifier: MIT OR Apache-2.0
//! Cutover from the global Accounting Settings store to declared integer parameters.

use erp_sdk::migrations::SemverVersion;
use erp_sdk::prelude::*;
use erp_sdk::serde_json;
use erp_sdk::settings::ParameterDecl;

#[erp_sdk::migration(addon = "account", version = "1.10", stage = post)]
async fn move_validation_limits(tx: &mut Transaction, _from: &SemverVersion) -> Result<()> {
    move_global_integer_parameters(
        tx,
        [
            crate::journal::settings::code_max_length_parameter()?,
            crate::tax::repartition_lines_parameter()?,
        ],
        "migration:account@1.10",
    )
    .await
}

/// Post-schema migration for `account` 1.16: backfills controlling, CIP asset, advanced budget
/// AVC, and intercompany trading partner defaults for existing rows.
#[erp_sdk::migration(addon = "account", version = "1.16", stage = post)]
async fn backfill_controlling_and_consolidation(
    tx: &mut Transaction,
    _from: &SemverVersion,
) -> Result<()> {
    tx.execute(
        "UPDATE account_account SET cost_center_policy = 'optional' WHERE cost_center_policy IS NULL",
        &[],
    )
    .await?;
    tx.execute(
        "UPDATE account_asset \
         SET asset_type = COALESCE(asset_type, 'normal'), \
             prorata_temporis = COALESCE(prorata_temporis, false) \
         WHERE asset_type IS NULL OR prorata_temporis IS NULL",
        &[],
    )
    .await?;
    tx.execute(
        "UPDATE account_budget \
         SET budget_type = COALESCE(budget_type, 'opex'), \
             version_code = COALESCE(version_code, 'V0') \
         WHERE budget_type IS NULL OR version_code IS NULL",
        &[],
    )
    .await?;
    // Schema sync applies PostgreSQL fast defaults (0, 100, 'ok') before post migrations run,
    // so also backfill pre-existing rows whose planned_amount <> 0 still carry default 0s.
    tx.execute(
        "UPDATE account_budget_line \
         SET original_amount = CASE WHEN COALESCE(original_amount, 0) = 0 AND COALESCE(revision_amount, 0) = 0 AND COALESCE(planned_amount, 0) <> 0 \
                                    THEN planned_amount ELSE COALESCE(original_amount, 0) END, \
             revision_amount = COALESCE(revision_amount, 0), \
             release_pct = COALESCE(release_pct, 100), \
             released_amount = CASE WHEN COALESCE(released_amount, 0) = 0 AND COALESCE(original_amount, 0) = 0 AND COALESCE(revision_amount, 0) = 0 AND COALESCE(planned_amount, 0) <> 0 \
                                    THEN planned_amount ELSE COALESCE(released_amount, 0) END, \
             committed_amount = COALESCE(committed_amount, 0), \
             avc_status = COALESCE(avc_status, 'ok') \
         WHERE original_amount IS NULL OR revision_amount IS NULL OR release_pct IS NULL \
            OR released_amount IS NULL OR committed_amount IS NULL OR avc_status IS NULL \
            OR (original_amount = 0 AND revision_amount = 0 AND released_amount = 0 AND COALESCE(planned_amount, 0) <> 0)",
        &[],
    )
    .await?;
    tx.execute(
        "UPDATE res_partner p \
         SET trading_partner_company_id = c.id \
         FROM res_company c \
         WHERE c.partner_id = p.id AND p.trading_partner_company_id IS NULL",
        &[],
    )
    .await?;
    Ok(())
}

pub(super) async fn move_global_integer_parameters(
    tx: &mut Transaction,
    parameters: impl IntoIterator<Item = ParameterDecl>,
    actor: &str,
) -> Result<()> {
    // Write the scope tuple while preserving the logical revision. A REPEATABLE READ Settings
    // writer holding the old tuple must retry. The deployment drain handles fresh old requests.
    tx.execute(
        "INSERT INTO ir_config_setting_scope(identity,revision) VALUES ('global','0')
         ON CONFLICT(identity) DO NOTHING",
        &[],
    )
    .await?;
    let rows = tx
        .query(
            "UPDATE ir_config_setting_scope SET revision=revision WHERE identity='global'
         RETURNING id::int8, revision::text",
            &[],
        )
        .await?;
    let [scope] = rows.as_slice() else {
        return Err(Error::internal(
            "The global settings scope could not be fenced.",
        ));
    };
    let scope_error = |error| Error::db(format!("Account Settings migration scope: {error}"));
    let scope_id: i64 = scope.try_get(0).map_err(scope_error)?;
    let revision: String = scope.try_get(1).map_err(scope_error)?;
    let subject = tx.env().uid().to_string();
    let mut changed = false;

    for parameter in parameters {
        let key = parameter.key();
        let db_error = |error| Error::db(format!("{key} migration: {error}"));
        let legacy = tx
            .query(
                "SELECT CASE WHEN octet_length(value_text) <= 32 THEN value_text END
               FROM ir_config_setting_value WHERE scope_id=$1 AND setting_key=$2",
                &[&scope_id, &key],
            )
            .await?;
        let current = tx
            .query(
                "SELECT value::text FROM ir_config_parameter WHERE key=$1 FOR UPDATE",
                &[&key],
            )
            .await?;
        let previous = current
            .first()
            .map(|row| row.try_get::<_, Option<String>>(0))
            .transpose()
            .map_err(db_error)?
            .flatten();
        let Some(legacy) = legacy.first() else {
            // Before this migration, an operator could have made a free-form parameter with
            // this name even when the Settings page had no override.
            parameter.read_text(previous.as_deref())?;
            continue;
        };
        let invalid = || Error::validation(format!("Stored setting {key} is invalid."));
        let legacy: Option<String> = legacy.try_get(0).map_err(db_error)?;
        let value: i64 =
            serde_json::from_str(legacy.as_deref().ok_or_else(invalid)?).map_err(|_| invalid())?;
        let canonical = value.to_string();
        parameter.check_text(Some(&canonical))?;
        if previous.as_deref() == Some(canonical.as_str()) {
            continue;
        }
        if let Some(old) = previous.filter(|text| !text.is_empty()) {
            tx.execute(
                "INSERT INTO ir_config_setting_audit
                    (scope_id,actor,subject_uid,setting_key,old_value,new_value,revision,create_date)
                 VALUES ($1,$2,$3,$4,$5,$6,$7,CURRENT_TIMESTAMP AT TIME ZONE 'UTC')",
                &[&scope_id, &actor, &subject, &key, &old, &canonical, &revision],
            ).await?;
        }
        tx.execute(
            "INSERT INTO ir_config_parameter(key,value) VALUES ($1,$2)
             ON CONFLICT(key) DO UPDATE SET value=EXCLUDED.value",
            &[&key, &canonical],
        )
        .await?;
        changed = true;
    }
    // The old rows stay in place for the retiring build's rollback contract.
    if changed {
        tx.invalidate_model("ir.config_parameter", None).await?;
    }
    Ok(())
}
