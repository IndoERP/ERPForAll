// SPDX-License-Identifier: MIT OR Apache-2.0
//! A scoped record link, with no second copy of company values in scalar settings storage.
use erp_sdk::prelude::*;
use erp_sdk::settings::{PanelItemDecl, PanelItemValue, ScopeKind, SettingsScope};

/// Returns all Accounting Settings panel items (links and optional enterprise feature toggles).
pub fn items() -> Result<Vec<PanelItemDecl>> {
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
        PanelItemDecl::menu_link(
            "account.setup_journals",
            "Journals",
            "account.menu_action_account_journal_form",
        )?
        .block("Journals")?
        .sequence(10)
        .scope(ScopeKind::Company)
        .access(&["account.group_account_manager"])?,
        PanelItemDecl::menu_link("account.setup_taxes", "Taxes", "account.menu_taxes")?
            .block("Taxes")?
            .sequence(10)
            .scope(ScopeKind::Company)
            .access(&["account.group_account_readonly"])?,
        // Optional enterprise controlling, CIP, budgeting AVC, and consolidation feature toggles.
        // Disabled by default so SME/UMKM installations remain clean until enabled by an admin.
        PanelItemDecl::group_toggle(
            "account.group_cost_center_controlling",
            "Cost Center & Profit Center Controlling",
            crate::COST_CENTER_GROUP,
        )?
        .block("Controlling")?
        .sequence(10)
        .access(&["base.group_system"])?,
        PanelItemDecl::menu_link(
            "account.setup_cost_centers",
            "Cost & Profit Centers",
            "account.menu_analytic_accounts",
        )?
        .block("Controlling")?
        .sequence(20)
        .scope(ScopeKind::Company)
        .access(&[crate::COST_CENTER_GROUP])?,
        PanelItemDecl::group_toggle(
            "account.group_asset_cip",
            "Fixed Assets & Construction in Progress (CIP)",
            crate::ASSET_CIP_GROUP,
        )?
        .block("Fixed Assets & CIP")?
        .sequence(10)
        .access(&["base.group_system"])?,
        PanelItemDecl::menu_link(
            "account.setup_asset_classes",
            "Asset Classes",
            "account.menu_account_asset_category",
        )?
        .block("Fixed Assets & CIP")?
        .sequence(20)
        .scope(ScopeKind::Company)
        .access(&[crate::ASSET_CIP_GROUP])?,
        PanelItemDecl::group_toggle(
            "account.group_budget_avc",
            "Advanced Budgeting & Availability Control (AVC)",
            crate::BUDGET_AVC_GROUP,
        )?
        .block("Budgeting & AVC")?
        .sequence(10)
        .access(&["base.group_system"])?,
        PanelItemDecl::menu_link(
            "account.setup_budget_profiles",
            "Budget Control Profiles",
            "account.menu_account_budget_profile",
        )?
        .block("Budgeting & AVC")?
        .sequence(20)
        .scope(ScopeKind::Company)
        .access(&[crate::BUDGET_AVC_GROUP])?,
        PanelItemDecl::group_toggle(
            "account.group_group_consolidation",
            "Group Financial Statement Consolidation",
            crate::CONSOLIDATION_GROUP,
        )?
        .block("Group Consolidation")?
        .sequence(10)
        .access(&["base.group_system"])?,
        PanelItemDecl::menu_link(
            "account.setup_consolidation_groups",
            "Consolidation Groups",
            "account.menu_consolidation_groups",
        )?
        .block("Group Consolidation")?
        .sequence(20)
        .scope(ScopeKind::Global)
        .access(&[crate::CONSOLIDATION_GROUP])?,
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
