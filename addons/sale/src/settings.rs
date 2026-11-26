// SPDX-License-Identifier: MIT OR Apache-2.0
//! Declared settings of the Sales addon (`sale`).

use erp_addon_base::ir_config_setting::read_values;
use erp_sdk::prelude::*;
use erp_sdk::serde_json;
use erp_sdk::settings::{ScopeKind, SettingDecl, SettingsScope};

/// "Lock confirmed sales": a confirmed order is locked at once.
pub const LOCK_CONFIRMED: &str = "sale.lock_confirmed_sales";
/// Enable percentage discounts on individual order lines and global discounts.
pub const GROUP_DISCOUNT_PER_SO_LINE: &str = "sale.group_discount_per_so_line";
/// Enable multi-currency and rule-based pricelists.
pub const GROUP_PRICELIST: &str = "sale.group_pricelist";
/// Default quotation validity window in days (`0` disables default expiry).
pub const QUOTATION_VALIDITY_DAYS: &str = "sale.quotation_validity_days";
/// Automatically generate a draft invoice upon order confirmation.
pub const AUTOMATIC_INVOICE: &str = "sale.automatic_invoice";
/// Allow customers to sign quotations online via portal.
pub const PORTAL_CONFIRMATION_SIGN: &str = "sale.portal_confirmation_sign";
/// Allow customers to confirm/pay quotations online via portal.
pub const PORTAL_CONFIRMATION_PAY: &str = "sale.portal_confirmation_pay";
/// Enable reusable Quotation Templates.
pub const USE_QUOTATION_TEMPLATES: &str = "sale.use_quotation_templates";
/// Show profit margins on sales order lines and orders.
pub const DISPLAY_MARGINS: &str = "sale.display_margins";

fn settings() -> Result<Vec<SettingDecl>> {
    let admin = |decl: SettingDecl| decl.access(&["base.group_user"], &["base.group_system"]);
    Ok(vec![
        admin(
            SettingDecl::boolean(
                LOCK_CONFIRMED,
                "Lock confirmed sales",
                ScopeKind::Global,
                false,
            )?
            .block("Quotations & Orders")?,
        )?,
        admin(
            SettingDecl::integer(
                QUOTATION_VALIDITY_DAYS,
                "Default quotation validity (days)",
                ScopeKind::Global,
                30,
                (0, 365),
            )?
            .block("Quotations & Orders")?,
        )?,
        admin(
            SettingDecl::boolean(
                USE_QUOTATION_TEMPLATES,
                "Quotation templates",
                ScopeKind::Global,
                true,
            )?
            .block("Quotations & Orders")?,
        )?,
        admin(
            SettingDecl::boolean(
                PORTAL_CONFIRMATION_SIGN,
                "Online signature",
                ScopeKind::Global,
                true,
            )?
            .block("Quotations & Orders")?,
        )?,
        admin(
            SettingDecl::boolean(
                PORTAL_CONFIRMATION_PAY,
                "Online payment",
                ScopeKind::Global,
                false,
            )?
            .block("Quotations & Orders")?,
        )?,
        admin(
            SettingDecl::boolean(
                GROUP_DISCOUNT_PER_SO_LINE,
                "Discounts on order lines & global discounts",
                ScopeKind::Global,
                true,
            )?
            .block("Pricing")?,
        )?,
        admin(
            SettingDecl::boolean(GROUP_PRICELIST, "Pricelists", ScopeKind::Global, true)?
                .block("Pricing")?,
        )?,
        admin(
            SettingDecl::boolean(
                DISPLAY_MARGINS,
                "Show margins on orders",
                ScopeKind::Global,
                true,
            )?
            .block("Pricing")?,
        )?,
        admin(
            SettingDecl::boolean(
                AUTOMATIC_INVOICE,
                "Automatic invoice on confirmation",
                ScopeKind::Global,
                false,
            )?
            .block("Invoicing")?,
        )?,
    ])
}
erp_sdk::setting_declarations!(settings);

async fn read_bool(tx: &mut Transaction, key: &str) -> Result<bool> {
    let env = tx.env().sudo(true);
    let read = read_values(&env, tx, SettingsScope::Global {}, vec![key.to_owned()]).await?;
    let read = serde_json::to_value(&read)
        .map_err(|error| Error::internal(format!("sales settings: {error}")))?;
    let entry = read["values"]
        .as_array()
        .and_then(|values| values.iter().find(|v| v["key"] == key))
        .ok_or_else(|| Error::internal(format!("sales setting '{key}' missing from response")))?;
    entry["value"]
        .as_bool()
        .ok_or_else(|| Error::internal(format!("sales setting '{key}' is not a boolean")))
}

/// The value of [`LOCK_CONFIRMED`], read with the framework's rights.
pub(crate) async fn lock_confirmed(tx: &mut Transaction) -> Result<bool> {
    read_bool(tx, LOCK_CONFIRMED).await
}

/// The value of [`AUTOMATIC_INVOICE`].
pub async fn automatic_invoice(tx: &mut Transaction) -> Result<bool> {
    read_bool(tx, AUTOMATIC_INVOICE).await
}

/// The value of [`QUOTATION_VALIDITY_DAYS`].
pub(crate) async fn quotation_validity_days(tx: &mut Transaction) -> Result<i32> {
    let env = tx.env().sudo(true);
    let read = read_values(
        &env,
        tx,
        SettingsScope::Global {},
        vec![QUOTATION_VALIDITY_DAYS.to_owned()],
    )
    .await?;
    let read = serde_json::to_value(&read)
        .map_err(|error| Error::internal(format!("sales settings: {error}")))?;
    let entry = read["values"]
        .as_array()
        .and_then(|values| values.iter().find(|v| v["key"] == QUOTATION_VALIDITY_DAYS))
        .ok_or_else(|| {
            Error::internal(format!(
                "sales setting '{QUOTATION_VALIDITY_DAYS}' missing from response"
            ))
        })?;
    let raw = entry["value"].as_i64().ok_or_else(|| {
        Error::internal(format!(
            "sales setting '{QUOTATION_VALIDITY_DAYS}' is not an integer"
        ))
    })?;
    i32::try_from(raw).map_err(|_| {
        Error::validation(format!(
            "sales setting '{QUOTATION_VALIDITY_DAYS}' ({raw}) is out of i32 range"
        ))
    })
}
