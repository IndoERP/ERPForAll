// SPDX-License-Identifier: MIT OR Apache-2.0
//! A scoped record link, with no second copy of company values in scalar settings storage.
use erp_sdk::prelude::*;
use erp_sdk::settings::{PanelItemDecl, PanelItemValue, ScopeKind, SettingsScope};

fn items() -> Result<Vec<PanelItemDecl>> {
    Ok(vec![
        PanelItemDecl::action_link(
            "account.company_configuration",
            "Configure company accounting",
            "account.company_accounting_action",
            true,
        )?
        .block("Company accounting")?
        .sequence(10)
        .scope(ScopeKind::Company)
        .access(&["base.group_system"])?,
    ])
}
fn resolve<'a>(
    env: &'a Env,
    tx: &'a mut Transaction,
    scope: &'a SettingsScope,
    key: &'a str,
) -> erp_sdk::BoxFuture<'a, Result<PanelItemValue>> {
    Box::pin(async move {
        if key != "account.company_configuration" {
            return Err(Error::programming(format!(
                "Account does not declare the setting item {key}."
            )));
        }
        let SettingsScope::Company { company_id } = scope else {
            return Err(Error::validation(
                "Choose a company for accounting configuration.",
            ));
        };
        let id = i64::try_from(u64::from(*company_id))
            .map_err(|_| Error::validation("Invalid company for accounting configuration."))?;
        // The Settings service validates membership before calling a resolver. Check ordinary
        // record access too; no sudo and no implicit change of the selected company.
        let company = env.records("res.company")?.browse([id])?;
        company.check_access(tx, erp_sdk::Operation::Read).await?;
        if company.exists(tx).await?.len() != 1 {
            return Err(Error::access("Choose a company available to your account."));
        }
        PanelItemValue::record(
            RecId::from_real(id).ok_or_else(|| {
                Error::validation("Invalid company for accounting configuration.")
            })?,
        )
    })
}
erp_sdk::settings_panel_items!(items, resolve);
