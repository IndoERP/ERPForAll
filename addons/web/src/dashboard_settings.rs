// SPDX-License-Identifier: MIT OR Apache-2.0
//! Database-scoped dashboard policy; only these nonsecret settings are read with metadata authority.
#[cfg(test)]
use erp_sdk::ErrorKind;
use erp_sdk::prelude::*;
use erp_sdk::settings::{ScopeKind, SettingDecl, SettingGroupDecl};
use erp_sdk::views::wire::{SettingValue, SettingsScope};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

pub(super) const ITEMS: &str = "web.dashboard_max_items";
pub(super) const DAYS: &str = "web.dashboard_max_period_days";
pub(super) const PERIODS: &str = "web.dashboard_periods";
const DEFAULT_PERIODS: &str = r#"[{"id":"last_7_days","label":"Last 7 days","kind":"rolling","days":7},{"id":"last_30_days","label":"Last 30 days","kind":"rolling","days":30},{"id":"last_90_days","label":"Last 90 days","kind":"rolling","days":90},{"id":"this_month","label":"This month","kind":"month"},{"id":"this_year","label":"This year","kind":"year"}]"#;
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Period {
    pub id: String,
    pub label: String,
    pub kind: String,
    pub days: Option<u32>,
}
fn invalid() -> Error {
    Error::validation(
        "web.dashboard_periods requires 1–16 unique bounded period choices with valid rolling days or calendar kinds.",
    )
}
fn parse(text: &str) -> Result<Vec<Period>> {
    if text.len() > 4096 {
        return Err(invalid());
    }
    let periods: Vec<Period> = serde_json::from_str(text).map_err(|_| invalid())?;
    if periods.is_empty() || periods.len() > 16 {
        return Err(invalid());
    }
    for (i, period) in periods.iter().enumerate() {
        if period.id.is_empty()
            || period.id.len() > 64
            || !period
                .id
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'_')
            || period.label.trim().is_empty()
            || period.label.len() > 256
            || periods[..i].iter().any(|p| p.id == period.id)
        {
            return Err(invalid());
        }
        match (period.kind.as_str(), period.days) {
            ("rolling", Some(1..=3660)) | ("month" | "year", None) => {}
            _ => return Err(invalid()),
        }
    }
    Ok(periods)
}
fn validate(value: &Value) -> Result<()> {
    match value {
        Value::Str(s) => parse(s).map(|_| ()),
        _ => Err(invalid()),
    }
}
fn declarations() -> Result<Vec<SettingDecl>> {
    let admin = &["base.group_system"];
    let mut declarations = Vec::new();
    for (key, label, default, bounds) in [
        (ITEMS, "Maximum dashboard items", 8, (1, 32)),
        (DAYS, "Maximum dashboard period days", 366, (1, 3660)),
    ] {
        declarations.push(
            SettingDecl::integer(key, label, ScopeKind::Global, default, bounds)?
                .block("Dashboards")?
                .access(admin, admin)?,
        );
    }
    declarations.push(
        SettingDecl::string(
            PERIODS,
            "Dashboard period choices",
            ScopeKind::Global,
            DEFAULT_PERIODS,
            4096,
        )?
        .validator(validate)?
        .block("Dashboards")?
        .access(admin, admin)?,
    );
    Ok(declarations)
}

fn validate_group(values: &BTreeMap<String, Value>) -> Result<()> {
    let Some(Value::Int(maximum)) = values.get(DAYS) else {
        return Err(Error::validation(
            "web.dashboard_max_period_days must be an integer.",
        ));
    };
    let Some(Value::Str(text)) = values.get(PERIODS) else {
        return Err(invalid());
    };
    for period in parse(text)? {
        if period.days.is_some_and(|days| i64::from(days) > *maximum)
            || (period.kind == "month" && *maximum < 31)
            || (period.kind == "year" && *maximum < 366)
        {
            return Err(Error::validation(
                "web.dashboard_periods contains a period beyond web.dashboard_max_period_days.",
            ));
        }
    }
    Ok(())
}
fn groups() -> Result<Vec<SettingGroupDecl>> {
    Ok(vec![SettingGroupDecl::new(
        "web.dashboard_policy",
        &[DAYS, PERIODS],
        validate_group,
    )?])
}
erp_sdk::setting_groups!(groups);
erp_sdk::setting_declarations!(declarations);
pub(super) struct Policy {
    pub max_items: usize,
    pub max_days: u32,
    pub periods: Vec<Period>,
}
pub(super) async fn read(env: &Env, tx: &mut Transaction) -> Result<Policy> {
    // Trusted, fixed nonsecret configuration keys only. No target-model data uses this environment.
    let values = erp_addon_base::ir_config_setting::read_values(
        &env.sudo(true),
        tx,
        SettingsScope::Global {},
        [ITEMS, DAYS, PERIODS]
            .into_iter()
            .map(str::to_owned)
            .collect(),
    )
    .await?;
    let get = |key| {
        values
            .values
            .iter()
            .find(|e| e.key == key)
            .map(|e| &e.value)
            .ok_or_else(|| Error::invariant("Dashboard setting was not returned."))
    };
    let integer = |key| match get(key)? {
        SettingValue::Integer(v) => Ok(*v),
        _ => Err(Error::invariant(
            "Dashboard setting has an unexpected type.",
        )),
    };
    let periods = match get(PERIODS)? {
        SettingValue::String(s) => parse(s)?,
        _ => return Err(invalid()),
    };
    Ok(Policy {
        max_items: usize::try_from(integer(ITEMS)?)
            .map_err(|_| Error::invariant("Dashboard item limit overflow."))?,
        max_days: u32::try_from(integer(DAYS)?)
            .map_err(|_| Error::invariant("Dashboard day limit overflow."))?,
        periods,
    })
}

#[cfg(test)]
#[path = "tests/dashboard_settings.rs"]
mod tests;
