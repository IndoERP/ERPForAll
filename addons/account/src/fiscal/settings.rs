// SPDX-License-Identifier: MIT OR Apache-2.0
//! The fiscal calendar settings: company-scope declared settings in the "Fiscal periods" block
//! of the Accounting settings page. A branch uses its root company's values, as the reference
//! delegates the fiscal year end to the root.

use erp_addon_base::ir_config_setting::read_values;
use erp_sdk::prelude::*;
use erp_sdk::serde_json;
use erp_sdk::settings::{ScopeKind, SettingDecl, SettingGroupDecl};
use erp_sdk::views::wire::{SettingsScope, WireId};
use std::collections::BTreeMap;

use super::calendar::FiscalYearEnd;

/// Day of the month the fiscal year ends on (the reference's `fiscalyear_last_day`).
pub const LAST_DAY_SETTING: &str = "account.fiscalyear_last_day";
/// Month the fiscal year ends in (the reference's `fiscalyear_last_month`).
pub const LAST_MONTH_SETTING: &str = "account.fiscalyear_last_month";
/// How many months after today a fiscal period may be created for.
pub const HORIZON_SETTING: &str = "account.fiscal_period_horizon_months";

/// Default of [`LAST_DAY_SETTING`].
pub const LAST_DAY_DEFAULT: i64 = 31;
/// Default of [`LAST_MONTH_SETTING`].
pub const LAST_MONTH_DEFAULT: &str = "12";
/// Default of [`HORIZON_SETTING`]: the next twelve months.
pub const HORIZON_DEFAULT: i64 = 12;
/// Floor of [`HORIZON_SETTING`]: the current and the next month.
pub const HORIZON_FLOOR: i64 = 1;
/// Hard bound of [`HORIZON_SETTING`]: ten years, beyond which a date is a typing error, not a plan.
pub const HORIZON_HARD: i64 = 120;

const MONTHS: [(&str, &str); 12] = [
    ("1", "January"),
    ("2", "February"),
    ("3", "March"),
    ("4", "April"),
    ("5", "May"),
    ("6", "June"),
    ("7", "July"),
    ("8", "August"),
    ("9", "September"),
    ("10", "October"),
    ("11", "November"),
    ("12", "December"),
];

fn settings() -> Result<Vec<SettingDecl>> {
    let place = |decl: SettingDecl, sequence: u32| -> Result<SettingDecl> {
        decl.block("Fiscal periods")?.sequence(sequence).access(
            &["account.group_account_readonly"],
            &["account.group_account_manager"],
        )
    };
    Ok(vec![
        place(
            SettingDecl::integer(
                LAST_DAY_SETTING,
                "Fiscal year last day",
                ScopeKind::Company,
                LAST_DAY_DEFAULT,
                (1, 31),
            )?,
            10,
        )?,
        place(
            SettingDecl::selection(
                LAST_MONTH_SETTING,
                "Fiscal year last month",
                ScopeKind::Company,
                LAST_MONTH_DEFAULT,
                &MONTHS,
            )?,
            20,
        )?,
        place(
            SettingDecl::integer(
                HORIZON_SETTING,
                "Fiscal periods may be created up to this many months ahead",
                ScopeKind::Company,
                HORIZON_DEFAULT,
                (HORIZON_FLOOR, HORIZON_HARD),
            )?,
            30,
        )?,
    ])
}
erp_sdk::setting_declarations!(settings);

/// The fiscal year end is one value in two settings: a write that leaves an impossible pair
/// (31 February) is refused, naming both.
fn validate_year_end(values: &BTreeMap<String, Value>) -> Result<()> {
    let day = match values.get(LAST_DAY_SETTING) {
        Some(Value::Int(day)) => *day,
        _ => {
            return Err(Error::validation(format!(
                "{LAST_DAY_SETTING} must be an integer."
            )));
        }
    };
    let month = match values.get(LAST_MONTH_SETTING) {
        Some(Value::Str(month)) => month.parse::<i64>().map_err(|_| {
            Error::validation(format!("{LAST_MONTH_SETTING} must be a month number."))
        })?,
        _ => {
            return Err(Error::validation(format!(
                "{LAST_MONTH_SETTING} must be a month."
            )));
        }
    };
    FiscalYearEnd::new(day, month).map(drop)
}

fn groups() -> Result<Vec<SettingGroupDecl>> {
    Ok(vec![SettingGroupDecl::new(
        "account.fiscal_year_end",
        &[LAST_DAY_SETTING, LAST_MONTH_SETTING],
        validate_year_end,
    )?])
}
erp_sdk::setting_groups!(groups);

/// The fiscal calendar of a company tree, read from its root company.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FiscalCalendar {
    year_end: FiscalYearEnd,
    horizon_months: i32,
}

impl FiscalCalendar {
    /// A calendar ending its fiscal year at `year_end` that creates periods up to
    /// `horizon_months` after today.
    ///
    /// # Errors
    ///
    /// `Validation` naming [`HORIZON_SETTING`] when `horizon_months` is outside its bounds.
    pub fn new(year_end: FiscalYearEnd, horizon_months: i64) -> Result<Self> {
        if !(HORIZON_FLOOR..=HORIZON_HARD).contains(&horizon_months) {
            return Err(Error::validation(format!(
                "{HORIZON_SETTING} is {horizon_months}; it must be between {HORIZON_FLOOR} and \
                 {HORIZON_HARD} months"
            )));
        }
        // inside the bounds above, so the cast is exact
        let horizon_months = horizon_months as i32;
        Ok(Self {
            year_end,
            horizon_months,
        })
    }

    /// The fiscal year end.
    #[must_use]
    pub fn year_end(&self) -> FiscalYearEnd {
        self.year_end
    }

    /// Months after today a period may be created for.
    #[must_use]
    pub fn horizon_months(&self) -> i32 {
        self.horizon_months
    }

    /// The settings of `root_company` (their defaults when never set). The framework reads its
    /// own configuration with elevated rights; the caller needs no settings group.
    ///
    /// # Errors
    ///
    /// Settings read failures; a stored value outside its bounds or an impossible day for the
    /// month, refused naming the setting.
    pub async fn read(tx: &mut Transaction, root_company: i64) -> Result<Self> {
        let env = tx.env().sudo(true).with_company(root_company)?;
        let id = u64::try_from(root_company)
            .ok()
            .and_then(|id| WireId::try_from(id).ok())
            .ok_or_else(|| Error::validation(format!("company id {root_company} is invalid")))?;
        let keys = [LAST_DAY_SETTING, LAST_MONTH_SETTING, HORIZON_SETTING];
        let read = read_values(
            &env,
            tx,
            SettingsScope::Company { company_id: id },
            keys.iter().map(|k| (*k).to_owned()).collect(),
        )
        .await?;
        let read = serde_json::to_value(&read)
            .map_err(|error| Error::internal(format!("fiscal calendar settings: {error}")))?;
        let value = |key: &str| {
            read["values"]
                .as_array()
                .and_then(|values| values.iter().find(|v| v["key"] == key))
                .map(|entry| entry["value"].clone())
                .ok_or_else(|| Error::internal(format!("{key} was not read")))
        };
        let day = value(LAST_DAY_SETTING)?
            .as_i64()
            .ok_or_else(|| Error::internal(format!("{LAST_DAY_SETTING} is not an integer")))?;
        let month = value(LAST_MONTH_SETTING)?
            .as_str()
            .and_then(|m| m.parse::<i64>().ok())
            .ok_or_else(|| Error::internal(format!("{LAST_MONTH_SETTING} is not a month")))?;
        let horizon = value(HORIZON_SETTING)?
            .as_i64()
            .ok_or_else(|| Error::internal(format!("{HORIZON_SETTING} is not an integer")))?;
        Self::new(FiscalYearEnd::new(day, month)?, horizon)
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, reason = "tests")]
    use super::*;
    use erp_sdk::settings::SettingKind;

    #[test]
    fn the_calendar_settings_are_bounded_company_settings() {
        let declarations = settings().unwrap();
        let keys: Vec<&str> = declarations.iter().map(|d| d.key()).collect();
        assert_eq!(
            keys,
            [LAST_DAY_SETTING, LAST_MONTH_SETTING, HORIZON_SETTING]
        );
        for decl in &declarations {
            assert_eq!(decl.write_groups(), &["account.group_account_manager"]);
        }
        assert_eq!(
            declarations[0].default_value(),
            &Value::Int(LAST_DAY_DEFAULT)
        );
        let SettingKind::Integer { min, max } = declarations[0].kind() else {
            panic!("integer setting expected");
        };
        assert_eq!((*min, *max), (1, 31));
        assert_eq!(
            declarations[1].default_value(),
            &Value::Str(LAST_MONTH_DEFAULT.into())
        );
        assert_eq!(
            declarations[2].default_value(),
            &Value::Int(HORIZON_DEFAULT)
        );
        let SettingKind::Integer { min, max } = declarations[2].kind() else {
            panic!("integer setting expected");
        };
        assert_eq!((*min, *max), (HORIZON_FLOOR, HORIZON_HARD));
        let year_end = FiscalYearEnd::new(31, 12).unwrap();
        for months in [HORIZON_FLOOR - 1, HORIZON_HARD + 1] {
            let error = FiscalCalendar::new(year_end, months).unwrap_err();
            assert!(error.message().contains(HORIZON_SETTING), "{error:?}");
        }
        let edge = FiscalCalendar::new(year_end, HORIZON_HARD).unwrap();
        assert_eq!(i64::from(edge.horizon_months()), HORIZON_HARD);
    }
}
