// SPDX-License-Identifier: MIT OR Apache-2.0
//! Base's General section of the Settings page: Users, Languages and Companies blocks of information and
//! link rows (SPEC-SETGEN). Every value is computed with the caller's own rights.
use erp_sdk::prelude::*;
use erp_sdk::settings::{MAX_ITEM_DETAIL_BYTES, PanelItemDecl, PanelItemValue, SettingsScope};

/// The keys base declares here. `ir_config_setting::published` refuses a registry that lacks one: the
/// declarations are a link-time registration that a binary can drop.
pub(crate) const KEYS: [&str; 9] = [
    "base.jobs_configuration_status",
    "base.active_users",
    "base.manage_users",
    "base.languages",
    "base.manage_languages",
    "base.company_info",
    "base.company_update",
    "base.companies",
    "base.manage_companies",
];
const SETTINGS_ADMIN: &str = "base.group_system";

fn place(item: PanelItemDecl, block: &str, sequence: u32) -> Result<PanelItemDecl> {
    item.app("base")?
        .block(block)?
        .sequence(sequence)
        .access(&[SETTINGS_ADMIN])
}
fn items() -> Result<Vec<PanelItemDecl>> {
    Ok(vec![
        PanelItemDecl::info("base.jobs_configuration_status", "Configuration status")?
            .block("Background Jobs")?
            .sequence(90)
            .scope(erp_sdk::settings::ScopeKind::Global)
            .access(&[SETTINGS_ADMIN])?,
        place(
            PanelItemDecl::info("base.active_users", "Active Users")?,
            "Users",
            10,
        )?,
        place(
            PanelItemDecl::action_link(
                "base.manage_users",
                "Manage Users",
                "base.settings_users_action",
                false,
            )?,
            "Users",
            20,
        )?,
        place(
            PanelItemDecl::info("base.languages", "Languages")?,
            "Languages",
            30,
        )?,
        place(
            PanelItemDecl::action_link(
                "base.manage_languages",
                "Manage Languages",
                "base.settings_languages_action",
                false,
            )?,
            "Languages",
            40,
        )?,
        place(
            PanelItemDecl::info("base.company_info", "Company")?,
            "Companies",
            50,
        )?,
        place(
            PanelItemDecl::action_link(
                "base.company_update",
                "Update Info",
                "base.settings_companies_action",
                true,
            )?,
            "Companies",
            60,
        )?,
        place(
            PanelItemDecl::info("base.companies", "Companies")?,
            "Companies",
            70,
        )?,
        place(
            PanelItemDecl::action_link(
                "base.manage_companies",
                "Manage Companies",
                "base.settings_companies_action",
                false,
            )?,
            "Companies",
            80,
        )?,
    ])
}
fn resolve<'a>(
    env: &'a Env,
    tx: &'a mut Transaction,
    scope: &'a SettingsScope,
    key: &'a str,
) -> erp_sdk::BoxFuture<'a, Result<PanelItemValue>> {
    Box::pin(async move {
        match key {
            "base.jobs_configuration_status" => PanelItemValue::info(Value::str("Live configuration"))?
                .detail("Saved limits apply to new job admissions without restart. Running jobs keep their admitted limits."),
            "base.active_users" => {
                count(env, tx, "res.users", true, ("Active User", "Active Users")).await
            }
            "base.languages" => count(env, tx, "res.lang", false, ("Language", "Languages")).await,
            "base.companies" => {
                count(env, tx, "res.company", false, ("Company", "Companies")).await
            }
            "base.company_info" => company_info(env, tx, scope).await,
            "base.company_update" => PanelItemValue::record(company(env, tx, scope).await?),
            // Links without a record never reach a resolver; anything else is not base's item.
            _ => Err(Error::programming(format!(
                "Base does not declare the setting item {key}."
            ))),
        }
    })
}
erp_sdk::settings_panel_items!(items, resolve);

// Active rows (internal users only for `res.users`) the caller can read, with the singular or plural label.
async fn count(
    env: &Env,
    tx: &mut Transaction,
    model: &str,
    internal_users: bool,
    (singular, plural): (&str, &str),
) -> Result<PanelItemValue> {
    // An explicit `active` condition keeps a caller's `active_test` context from changing the number.
    let mut domain = Domain::field("active")?.eq(true);
    if internal_users {
        domain = domain & Domain::field("share")?.eq(false);
    }
    let number = env.records(model)?.search_count(tx, &domain, None).await?;
    // The reference shows the singular for 0 and 1.
    let label = if number <= 1 { singular } else { plural };
    let number = i64::try_from(number)
        .map_err(|_| Error::internal("A settings count does not fit a 64-bit integer."))?;
    PanelItemValue::info(Value::Int(number))?.label(label)
}
// The scope's company, or the caller's current company in the global scope.
async fn company(env: &Env, tx: &mut Transaction, scope: &SettingsScope) -> Result<RecId> {
    let id = match scope {
        // Membership was checked by the service before any resolver runs.
        SettingsScope::Company { company_id } => i64::try_from(u64::from(*company_id))
            .map_err(|_| Error::validation("Invalid settings request."))?,
        SettingsScope::Global {} => match tx.companies(env).await?.first() {
            Some(current) => current.get(),
            None => return Err(Error::access("No current company to describe.")),
        },
        _ => return Err(Error::validation("Invalid settings request.")),
    };
    RecId::from_real(id).ok_or_else(|| Error::validation("Invalid settings request."))
}
// Display normalisation of one row of the company text: every run of control characters
// (a pasted CR LF, a tab) becomes one space and the result is trimmed. The data is shown, not refused.
fn one_line(value: &str) -> String {
    let mut line = String::with_capacity(value.len());
    let mut in_control = false;
    for c in value.chars() {
        if c.is_control() {
            if !in_control {
                line.push(' ');
            }
            in_control = true;
        } else {
            line.push(c);
            in_control = false;
        }
    }
    line.trim().to_owned()
}
fn text(row: &std::collections::BTreeMap<String, Value>, field: &str) -> Result<Option<String>> {
    match row.get(field) {
        Some(Value::Str(value)) => {
            let line = one_line(value);
            Ok((!line.is_empty()).then_some(line))
        }
        Some(Value::False) => Ok(None),
        Some(_) => Err(Error::internal(format!(
            "res.company.{field} did not read as text."
        ))),
        None => Err(Error::internal(format!(
            "res.company.{field} is missing from its read."
        ))),
    }
}
async fn company_info(
    env: &Env,
    tx: &mut Transaction,
    scope: &SettingsScope,
) -> Result<PanelItemValue> {
    let id = company(env, tx, scope).await?;
    let rows = env
        .records("res.company")?
        .browse([id.get()])?
        .read(tx, &["name", "street", "street2", "zip", "city", "vat"])
        .await?;
    let row = rows
        .first()
        .ok_or_else(|| Error::missing("The company to describe does not exist."))?;
    // A name made only of spaces and control characters is company data, not a broken read.
    let name = text(row, "name")?
        .ok_or_else(|| Error::validation(format!("Company {} has no name to show.", id.get())))?;
    let mut lines = Vec::new();
    lines.extend(text(row, "street")?);
    lines.extend(text(row, "street2")?);
    match (text(row, "zip")?, text(row, "city")?) {
        (Some(zip), Some(city)) => lines.push(format!("{zip} - {city}")),
        (Some(part), None) | (None, Some(part)) => lines.push(part),
        (None, None) => {}
    }
    // One space after the label (BUG-108); the country's own tax label needs countries (SPEC-SETGEN §4.2).
    if let Some(vat) = text(row, "vat")? {
        lines.push(format!("VAT: {vat}"));
    }
    let value = PanelItemValue::info(Value::str(&name))?;
    if lines.is_empty() {
        return Ok(value);
    }
    let detail = lines.join("\n");
    if detail.len() > MAX_ITEM_DETAIL_BYTES {
        return Err(Error::validation(format!(
            "The address of company {} is longer than the {MAX_ITEM_DETAIL_BYTES} bytes the Settings \
             page shows.",
            id.get()
        )));
    }
    value.detail(&detail)
}
