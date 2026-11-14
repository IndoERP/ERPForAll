// SPDX-License-Identifier: MIT OR Apache-2.0
//! Inventory's scoped values; consumers read the shared Settings API.
use erp_sdk::settings::{ScopeKind, SettingDecl, SettingGroupDecl};
use erp_sdk::{Error, Result, Value};
use std::collections::BTreeMap;

const ADMIN: &str = "base.group_system";
const READ: &[&str] = &["stock.group_stock_manager", ADMIN];
const MONTH: &str = "stock.annual_inventory_month";
const DAY: &str = "stock.annual_inventory_day";

/// One native policy snapshot uses exactly these declared, global keys.
pub(super) const QUANT_KEYS: [&str; 8] = [
    "stock.quant_lock_timeout_ms",
    "stock.quant_max_attempts",
    "stock.quant_retry_base_ms",
    "stock.quant_retry_cap_ms",
    "stock.quant_idle_transaction_timeout_ms",
    "stock.quant_tcp_keepalives_idle_s",
    "stock.quant_tcp_keepalives_interval_s",
    "stock.quant_tcp_keepalives_count",
];

fn quant_declarations() -> Result<Vec<SettingDecl>> {
    // 4/D9: operational defaults, not milestone-end calibration measurements.
    [
        (QUANT_KEYS[0], "Stock lock wait (milliseconds)", 1000, (10, 10000)),
        (QUANT_KEYS[1], "Stock operation attempts", 5, (1, 5)),
        (QUANT_KEYS[2], "Stock retry base delay (milliseconds)", 50, (1, 2000)),
        (QUANT_KEYS[3], "Stock retry delay cap (milliseconds)", 2000, (1, 10000)),
        (QUANT_KEYS[4], "Stock idle transaction limit (milliseconds)", 60000, (1000, 600000)),
        (QUANT_KEYS[5], "Stock TCP keepalive idle (seconds)", 30, (1, 300)),
        (QUANT_KEYS[6], "Stock TCP keepalive interval (seconds)", 10, (1, 60)),
        (QUANT_KEYS[7], "Stock TCP keepalive probes", 3, (1, 10)),
    ]
    .into_iter()
    .enumerate()
    .map(|(sequence, (key, label, default, bounds))| {
        SettingDecl::integer(key, label, ScopeKind::Global, default, bounds)?
            .block("Stock operations")?
            .sequence(u32::try_from(sequence).map_err(|_| Error::programming("Stock policy sequence overflow."))? + 60)
            .help(&format!("Default: {default}. Allowed: {} to {}. Shared by all companies; each native stock operation snapshots these limits before it begins.", bounds.0, bounds.1))?
            .access(&["stock.group_stock_user", ADMIN], &[ADMIN])
    })
    .collect()
}

fn company(decl: SettingDecl, block: &str, sequence: u32, help: &str) -> Result<SettingDecl> {
    decl.block(block)?
        .sequence(sequence)
        .help(help)?
        .access(READ, &[ADMIN])
}
fn declarations() -> Result<Vec<SettingDecl>> {
    use ScopeKind::{Company, Global};
    let mut declarations = vec![
        company(SettingDecl::selection("stock.picking_policy", "Delivery policy", Company, "direct",
            &[("direct", "Send available items"), ("one", "Wait for the complete order")])?,
            "Shipping", 10, "Saved for this company. Operation types do not use this value yet.")?,
        company(SettingDecl::integer("stock.horizon_days", "Replenishment horizon (days)", Company, 365, (1, 3650))?,
            "Planning", 20, "Default: 365 days. Allowed: 1 to 3650 days. Saved only; replenishment does not use this value yet.")?,
        company(SettingDecl::selection(MONTH, "Annual count month", Company, "12", &[
            ("1", "January"), ("2", "February"), ("3", "March"), ("4", "April"),
            ("5", "May"), ("6", "June"), ("7", "July"), ("8", "August"),
            ("9", "September"), ("10", "October"), ("11", "November"), ("12", "December")])?,
            "Counting", 30, "Default: December. Choose a month from January through December. Saved only; counting does not use this date yet.")?,
        company(SettingDecl::integer(DAY, "Annual count day", Company, 31, (1, 31))?,
            "Counting", 31, "Default: 31. Choose a day that exists in the selected month; February ends at 28. Saved only; counting does not use this date yet.")?,
        company(SettingDecl::boolean("stock.stock_move_email_validation", "Delivery email", Company, false)?,
            "Shipping", 40, "Default: off. Saved only; delivery email is not sent by this setting yet.")?,
        company(SettingDecl::boolean("stock.stock_text_confirmation", "Delivery text message", Company, false)?,
            "Shipping", 41, "Default: off. Saved only; delivery text messages are not sent yet.")?,
        company(SettingDecl::selection("stock.stock_confirmation_type", "Delivery text channel", Company, "sms", &[("sms", "SMS")])?,
            "Shipping", 42, "Default and available channel: SMS. Saved only; no message is sent yet.")?,
        SettingDecl::string("stock.barcode_separator", "Combined barcode separator", Global, ",", 16)?
            .validator(separator)?.block("Barcode")?.sequence(50)
            .help("Default: comma. Allowed: 1 to 16 UTF-8 bytes without control characters. Shared by all companies. Saved only; barcode aggregation does not use this value yet.")?
            .access(&[ADMIN], &[ADMIN])?,
    ];
    declarations.extend(quant_declarations()?);
    Ok(declarations)
}
fn separator(value: &Value) -> Result<()> {
    match value {
        Value::Str(text)
            if !text.is_empty() && text.len() <= 16 && !text.chars().any(char::is_control) =>
        {
            Ok(())
        }
        _ => Err(Error::validation(
            "stock.barcode_separator takes 1 to 16 UTF-8 bytes without control characters.",
        )),
    }
}
fn annual_date(values: &BTreeMap<String, Value>) -> Result<()> {
    let Some(Value::Str(month)) = values.get(MONTH) else {
        return Err(Error::validation(
            "stock.annual_inventory_month takes a month key from 1 to 12.",
        ));
    };
    let max = match month.as_ref() {
        "2" => 28,
        "4" | "6" | "9" | "11" => 30,
        "1" | "3" | "5" | "7" | "8" | "10" | "12" => 31,
        _ => {
            return Err(Error::validation(
                "stock.annual_inventory_month takes a month key from 1 to 12.",
            ));
        }
    };
    match values.get(DAY) {
        Some(Value::Int(day)) if (1..=max).contains(day) => Ok(()),
        _ => Err(Error::validation(format!(
            "{DAY} takes 1 to {max} for {MONTH} {month}."
        ))),
    }
}
fn groups() -> Result<Vec<SettingGroupDecl>> {
    Ok(vec![
        SettingGroupDecl::new("stock.annual_inventory_date", &[MONTH, DAY], annual_date)?,
        SettingGroupDecl::new(
            "stock.quant_retry_backoff",
            &[QUANT_KEYS[2], QUANT_KEYS[3]],
            quant_backoff,
        )?,
    ])
}

fn quant_backoff(values: &BTreeMap<String, Value>) -> Result<()> {
    match (values.get(QUANT_KEYS[2]), values.get(QUANT_KEYS[3])) {
        (Some(Value::Int(base)), Some(Value::Int(cap))) if base <= cap => Ok(()),
        _ => Err(Error::validation(
            "stock.quant_retry_base_ms cannot exceed stock.quant_retry_cap_ms.",
        )),
    }
}
erp_sdk::setting_declarations!(declarations);
erp_sdk::setting_groups!(groups);

// the service hook is linked independently of the settings factories. Publishing
// inventory without one of its native registrations must fail, rather than show a partial page.
use erp_addon_base::ir_config_setting::IrConfigSetting;
use erp_sdk::prelude::{RecordSet, Transaction, extend, methods};

#[extend(IrConfigSetting, addon = "stock")]
struct StockSettings {}

#[methods(IrConfigSetting, addon = "stock")]
impl StockSettings {
    #[method(override)]
    async fn _register_hook(
        this: &RecordSet<IrConfigSetting>,
        tx: &mut Transaction,
        sup: Super,
    ) -> Result<()> {
        let expected: std::collections::BTreeSet<_> = [
            "stock.picking_policy",
            "stock.horizon_days",
            MONTH,
            DAY,
            "stock.stock_move_email_validation",
            "stock.stock_text_confirmation",
            "stock.stock_confirmation_type",
            "stock.barcode_separator",
        ]
        .into_iter()
        .chain(QUANT_KEYS)
        .map(str::to_owned)
        .collect();
        let mut keys = std::collections::BTreeSet::new();
        for entry in erp_sdk::settings::CONTRIBUTIONS
            .iter()
            .filter(|entry| entry.addon.name == "stock")
        {
            for declaration in (entry.declarations)()? {
                keys.insert(declaration.key().to_owned());
            }
        }
        let groups = erp_sdk::settings::GROUP_CONTRIBUTIONS
            .iter()
            .filter(|entry| entry.addon.name == "stock")
            .map(|entry| (entry.declarations)())
            .collect::<Result<Vec<_>>>()?;
        let group_keys: std::collections::BTreeSet<_> =
            groups.iter().flatten().map(|group| group.key()).collect();
        let expected_groups = std::collections::BTreeSet::from([
            "stock.annual_inventory_date",
            "stock.quant_retry_backoff",
        ]);
        if keys != expected || group_keys != expected_groups {
            return Err(Error::programming(
                "Inventory settings declarations or validation groups are missing from erp-addon-stock.",
            ));
        }
        crate::settings_panel::require_linked()?;
        sup.call(tx, this.as_records(), ()).await
    }
}
