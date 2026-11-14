// SPDX-License-Identifier: MIT OR Apache-2.0
//! Database-owned admission bound for each tax's invoice/refund distribution.
use erp_addon_base::ir_config_setting::read_values;
use erp_sdk::prelude::*;
use erp_sdk::settings::{ScopeKind, SettingDecl};
use erp_sdk::views::wire::SettingsScope;

/// Maximum lines per tax and document kind, including the base line.
pub const REPARTITION_LINES_MAX: &str = "account.tax_repartition_lines_max";
fn settings() -> Result<Vec<SettingDecl>> {
    Ok(vec![
        SettingDecl::integer(
            REPARTITION_LINES_MAX,
            "Taxes: maximum distribution lines per document kind",
            ScopeKind::Global,
            16,
            (2, 64),
        )?
        .block("Taxes")?
        .access(
            &["account.group_account_readonly"],
            &["account.group_account_manager"],
        )?,
    ])
}
erp_sdk::setting_declarations!(settings);

pub(crate) async fn limit(tx: &mut Transaction) -> Result<usize> {
    let values = read_values(
        &tx.env().sudo(true),
        tx,
        SettingsScope::Global {},
        vec![REPARTITION_LINES_MAX.to_owned()],
    )
    .await?;
    let values = erp_sdk::serde_json::to_value(values)
        .map_err(|e| Error::internal(format!("Tax settings: {e}")))?;
    let n = values["values"][0]["value"]
        .as_u64()
        .filter(|n| (2..=64).contains(n))
        .ok_or_else(|| {
            Error::validation(format!("{REPARTITION_LINES_MAX} must be between 2 and 64."))
        })?;
    usize::try_from(n).map_err(|e| Error::internal(format!("Tax line limit: {e}")))
}
