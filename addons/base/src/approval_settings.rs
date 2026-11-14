// SPDX-License-Identifier: MIT OR Apache-2.0
//! Database-global operating policy below's unchanged hard admission ceiling.
use erp_sdk::settings::{ScopeKind, SettingDecl, SettingsRegistry};
use erp_sdk::{Env, Error, Operation, Result, SUPERUSER_ID, Transaction};

pub(crate) const KEY: &str = "base.approval_max_approvers";

fn declaration() -> Result<SettingDecl> {
    let ceiling = i64::try_from(crate::ir_approval_policy::MAX_APPROVERS)
        .map_err(|_| Error::programming("The approver ceiling does not fit Settings."))?;
    SettingDecl::integer(KEY, "Maximum approvers", ScopeKind::Global, ceiling, (1, ceiling))?
        .block("Approvals")?
        .help("Limits named users plus groups and distinct eligible humans. Default: 64; allowed: 1 to 64. Archive or replace oversized active revisions before lowering. Group growth is checked at resolution.")?
        .access(&["base.group_system"], &["base.group_system"])
}

fn declarations() -> Result<Vec<SettingDecl>> {
    Ok(vec![declaration()?])
}
erp_sdk::setting_declarations!(declarations);

pub(crate) fn require(registry: &SettingsRegistry) -> Result<()> {
    if registry.get(KEY).is_none() {
        return Err(Error::programming(format!(
            "The settings declaration {KEY} of erp-addon-base is not linked into this binary."
        )));
    }
    Ok(())
}

fn corrupt() -> Error {
    Error::validation(format!("Stored setting {KEY} is invalid."))
}

/// Read only this nonsecret policy without granting generic Settings access to the caller.
pub(crate) async fn limit(tx: &mut Transaction) -> Result<usize> {
    let env = tx.env();
    for model in ["ir.config.setting.scope", "ir.config.setting.value"] {
        tx.flush_model(env.records(model)?.model_id()).await?;
    }
    // Present NULL/oversized values remain rows, so they cannot become an absent/default value.
    // 32 bytes bounds the serialized i64 scalar, including malformed stored values.
    let rows = tx
        .query(
            "SELECT CASE WHEN octet_length(v.value_text)<=32 THEN v.value_text END \
         FROM ir_config_setting_value v JOIN ir_config_setting_scope s ON s.id=v.scope_id \
         WHERE s.identity='global' AND v.setting_key=$1",
            &[&KEY],
        )
        .await?;
    let declaration = declaration()?;
    let value = match rows.as_slice() {
        [] => declaration.default_value().clone(), // only absent storage uses the default.
        [row] => {
            let text: String = row.try_get(0).map_err(|_| corrupt())?;
            let n: i64 = erp_sdk::serde_json::from_str(&text).map_err(|_| corrupt())?;
            erp_sdk::Value::Int(n)
        }
        _ => return Err(corrupt()), // The scope/key uniqueness contract must hold.
    };
    declaration.validate(&value).map_err(|_| corrupt())?;
    match value {
        erp_sdk::Value::Int(n) => usize::try_from(n).map_err(|_| corrupt()),
        _ => Err(corrupt()),
    }
}

/// Policy changes and Settings writes fence the same row. A row lock alone would admit write
/// skew under REPEATABLE READ; this no-op UPDATE forces stale writers to serialize/retry.
/// It does not change the Settings CAS revision or invent a Settings audit event.
pub(crate) async fn lock_policy_edit(tx: &mut Transaction) -> Result<()> {
    tx.execute("INSERT INTO ir_config_setting_scope(identity,revision) VALUES ('global','0') ON CONFLICT(identity) DO NOTHING", &[]).await?;
    tx.execute(
        "UPDATE ir_config_setting_scope SET revision=revision WHERE identity='global'",
        &[],
    )
    .await?;
    Ok(())
}

/// A1: refuse a proposed value below any active revision's named or potential eligible set.
/// Settings calls this after authorization and while holding its global scope writer lock.
pub(crate) async fn check_active(env: &Env, tx: &mut Transaction, proposed: i64) -> Result<()> {
    tx.flush().await?;
    // Membership goes from an approval group to every group implying it (Graph::down). UNION
    // terminates even for malformed cycles. Each lateral user probe stops at proposed + 1;
    // offenders are read in ten-row windows for access checks; only ten labels are retained.
    let sql = "WITH RECURSIVE holding(policy_id,gid) AS ( \
           SELECT r.policy_id,r.group_id FROM ir_approval_policy_groups_rel r \
           JOIN ir_approval_policy p ON p.id=r.policy_id WHERE p.active AND p.outcome='approve' \
           UNION SELECT h.policy_id,i.gid FROM holding h JOIN res_groups_implied_rel i ON i.hid=h.gid \
         ), offenders AS ( \
           SELECT p.id,p.name FROM ir_approval_policy p WHERE p.active AND p.outcome='approve' AND ( \
             (SELECT count(*) FROM ir_approval_policy_users_rel r WHERE r.policy_id=p.id) + \
             (SELECT count(*) FROM ir_approval_policy_groups_rel r WHERE r.policy_id=p.id)>$1 \
             OR (SELECT count(*) FROM (SELECT u.id FROM res_users u \
                 WHERE u.active AND NOT u.share AND u.id<>$2 AND ( \
                   EXISTS(SELECT 1 FROM ir_approval_policy_users_rel r WHERE r.policy_id=p.id AND r.user_id=u.id) \
                   OR EXISTS(SELECT 1 FROM holding h JOIN res_groups_users_rel m ON m.gid=h.gid \
                             WHERE h.policy_id=p.id AND m.uid=u.id)) \
                 AND (p.company_id IS NULL OR EXISTS(SELECT 1 FROM res_company_users_rel c \
                     WHERE c.user_id=u.id AND c.cid=p.company_id)) LIMIT ($1+1)) eligible)>$1) \
         ) SELECT id::int8,left(name,256),count(*) OVER()::int8 FROM offenders WHERE id>$3 ORDER BY id LIMIT 10";
    let mut after = 0_i64;
    let mut total = 0_i64;
    let mut seen = 0_i64;
    let mut names = Vec::new();
    loop {
        let rows = tx
            .query(sql, &[&proposed, &SUPERUSER_ID.get(), &after])
            .await?;
        let Some(first) = rows.first() else {
            break;
        };
        let ids = rows
            .iter()
            .map(|row| row.try_get::<_, i64>(0).map_err(|_| corrupt()))
            .collect::<Result<Vec<_>>>()?;
        // A global Settings administrator still has company/record-scoped policy access.
        // Check every offender in bounded windows before exposing any labels or total count.
        if !env
            .records("ir.approval.policy")?
            .browse(ids.iter().copied())?
            .has_access(tx, Operation::Read)
            .await?
        {
            return Err(Error::access(format!(
                "{KEY} cannot be lowered: an active approval revision is unavailable or access is denied."
            )));
        }
        if after == 0 {
            total = first.try_get(2).map_err(|_| corrupt())?;
            for row in &rows {
                let id: i64 = row.try_get(0).map_err(|_| corrupt())?;
                let name: String = row.try_get(1).map_err(|_| corrupt())?;
                names.push(format!("'{name}' (id {id})"));
            }
        }
        after = *ids.last().ok_or_else(corrupt)?;
        seen = seen
            .checked_add(i64::try_from(rows.len()).map_err(|_| corrupt())?)
            .ok_or_else(corrupt)?;
        if seen >= total {
            break;
        }
    }
    if names.is_empty() {
        return Ok(());
    }
    let shown = i64::try_from(names.len()).map_err(|_| corrupt())?;
    let rest = total.checked_sub(shown).ok_or_else(corrupt)?;
    let remaining = if rest > 0 {
        format!("; and {rest} more")
    } else {
        String::new()
    };
    Err(Error::validation(format!(
        "{KEY} cannot be set to {proposed}: active approval revisions exceed this limit: {}{remaining}. Archive or replace these revisions first.",
        names.join(", ")
    )))
}
