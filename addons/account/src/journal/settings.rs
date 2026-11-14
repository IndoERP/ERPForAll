// SPDX-License-Identifier: MIT OR Apache-2.0
//! Declared settings of the journals (Accounting settings, block "Journals"): the longest journal
//! code, the code prefix proposed per journal type and the account types a journal's default
//! account may have per journal type. The reference keeps these as literals in
//! (default-account domain) and `:898-918`
//! (code prefixes); here an administrator changes them, each with a default and a bound.

use std::collections::BTreeMap;

use erp_addon_base::ir_config_setting::read_values;
use erp_sdk::prelude::*;
use erp_sdk::serde_json;
use erp_sdk::settings::{ScopeKind, SettingDecl};
use erp_sdk::views::wire::SettingsScope;

/// The six journal types, in the reference's order.
pub const JOURNAL_TYPES: [&str; 6] = ["sale", "purchase", "cash", "bank", "credit", "general"];

/// The account types of `account.account` (its selection), for validating the allowed-types
/// settings. `crates/erp-cli/tests/account_journals.rs` checks it against the model's selection.
pub const ACCOUNT_TYPES: [&str; 19] = [
    "asset_receivable",
    "asset_cash",
    "asset_current",
    "asset_non_current",
    "asset_prepayments",
    "asset_fixed",
    "liability_payable",
    "liability_credit_card",
    "liability_current",
    "liability_non_current",
    "equity",
    "equity_unaffected",
    "income",
    "income_other",
    "expense",
    "expense_other",
    "expense_depreciation",
    "expense_direct_cost",
    "off_balance",
];

/// Declared setting: the longest journal code, in characters.
pub const CODE_MAX_LENGTH: &str = "account.journal_code_max_length";
/// Default of [`CODE_MAX_LENGTH`]. The reference sets no limit; 16 holds its proposed codes and
/// the imported names of its tests.
pub const CODE_MAX_LENGTH_DEFAULT: i64 = 16;
/// Floor of [`CODE_MAX_LENGTH`].
pub const CODE_MAX_LENGTH_FLOOR: i64 = 1;
/// Hard bound of [`CODE_MAX_LENGTH`]: the code is the fixed text of every entry number of the
/// journal and stays far below the 128 bytes a sequence pattern may carry.
pub const CODE_MAX_LENGTH_HARD: i64 = 32;

/// Longest proposed code prefix, in bytes.
const PREFIX_MAX_BYTES: usize = 16;
/// Longest list of allowed account types, in bytes (all nineteen types fit).
const TYPES_MAX_BYTES: usize = 512;

/// Per journal type: label, proposed code prefix, allowed default-account types (the reference's
/// default-account domain).
const DEFAULTS: [(&str, &str, &str, &str); 6] = [
    ("sale", "Sales", "INV", "income,income_other"),
    (
        "purchase",
        "Purchase",
        "BILL",
        "expense,expense_depreciation,expense_direct_cost",
    ),
    ("cash", "Cash", "CSH", "asset_cash"),
    ("bank", "Bank", "BNK", "asset_cash,liability_credit_card"),
    ("credit", "Credit Card", "CCD", "liability_credit_card"),
    (
        "general",
        "Miscellaneous",
        "MISC",
        "asset_receivable,asset_cash,asset_current,asset_non_current,asset_prepayments,\
         asset_fixed,liability_payable,liability_credit_card,liability_current,\
         liability_non_current,equity,equity_unaffected,income,income_other,expense,\
         expense_depreciation,expense_direct_cost,off_balance",
    ),
];

/// Key of the code prefix proposed for journals of `journal_type`.
pub fn prefix_key(journal_type: &str) -> String {
    format!("account.journal_code_prefix_{journal_type}")
}

/// Key of the account types allowed as default account of journals of `journal_type`.
pub fn account_types_key(journal_type: &str) -> String {
    format!("account.journal_account_types_{journal_type}")
}

fn text(value: &Value) -> Result<&str> {
    match value {
        Value::Str(text) => Ok(&**text),
        _ => Err(Error::validation("expected a text value")),
    }
}

/// A prefix is one word: no spaces, no line breaks.
fn check_prefix(value: &Value) -> Result<()> {
    let prefix = text(value)?;
    if prefix.is_empty() || prefix.chars().any(char::is_whitespace) {
        return Err(Error::validation(
            "A journal code prefix is one word without spaces.",
        ));
    }
    Ok(())
}

/// A comma-separated list of account types, each known and listed once.
fn check_account_types(value: &Value) -> Result<()> {
    parse_account_types(text(value)?).map(|_| ())
}

fn parse_account_types(list: &str) -> Result<Vec<String>> {
    if list.trim().is_empty() {
        return Err(Error::validation(
            "List at least one account type, such as income,income_other.",
        ));
    }
    let mut seen = Vec::new();
    for item in list.split(',').map(str::trim) {
        if !ACCOUNT_TYPES.contains(&item) || seen.iter().any(|s: &String| s == item) {
            return Err(Error::validation(format!(
                "{item:?} is not an account type, or is listed twice; list account types such as \
                 income,income_other"
            )));
        }
        seen.push(item.to_owned());
    }
    Ok(seen)
}

fn settings() -> Result<Vec<SettingDecl>> {
    let admin = |decl: SettingDecl| {
        decl.block("Journals")?.access(
            &["account.group_account_readonly"],
            &["account.group_account_manager"],
        )
    };
    let mut decls = vec![admin(SettingDecl::integer(
        CODE_MAX_LENGTH,
        "Journals: longest journal code (characters)",
        ScopeKind::Global,
        CODE_MAX_LENGTH_DEFAULT,
        (CODE_MAX_LENGTH_FLOOR, CODE_MAX_LENGTH_HARD),
    )?)?];
    for (journal_type, label, prefix, types) in DEFAULTS {
        decls.push(admin(
            SettingDecl::string(
                &prefix_key(journal_type),
                &format!("Journals: code proposed for a new {label} journal"),
                ScopeKind::Global,
                prefix,
                PREFIX_MAX_BYTES,
            )?
            .validator(check_prefix)?,
        )?);
        decls.push(admin(
            SettingDecl::string(
                &account_types_key(journal_type),
                &format!("Journals: account types allowed as default account of a {label} journal"),
                ScopeKind::Global,
                types,
                TYPES_MAX_BYTES,
            )?
            .validator(check_account_types)?,
        )?);
    }
    Ok(decls)
}
erp_sdk::setting_declarations!(settings);

/// The journal settings as read for one operation.
#[derive(Debug, Clone)]
pub struct JournalSettings {
    /// [`CODE_MAX_LENGTH`].
    pub code_max_length: usize,
    prefixes: BTreeMap<String, String>,
    account_types: BTreeMap<String, Vec<String>>,
}

impl JournalSettings {
    /// Read every journal setting in one statement. The framework reads its own configuration with
    /// elevated rights: creating a journal needs no settings group.
    ///
    /// # Errors
    ///
    /// Settings read failures; a stored value of the wrong shape.
    pub async fn read(tx: &mut Transaction) -> Result<Self> {
        let env = tx.env().sudo(true);
        let mut keys = vec![CODE_MAX_LENGTH.to_owned()];
        for journal_type in JOURNAL_TYPES {
            keys.push(prefix_key(journal_type));
            keys.push(account_types_key(journal_type));
        }
        let read = read_values(&env, tx, SettingsScope::Global {}, keys).await?;
        let read = serde_json::to_value(&read)
            .map_err(|error| Error::internal(format!("journal settings: {error}")))?;
        let values: BTreeMap<String, serde_json::Value> = read["values"]
            .as_array()
            .ok_or_else(|| Error::internal("journal settings: no values"))?
            .iter()
            .filter_map(|entry| Some((entry["key"].as_str()?.to_owned(), entry["value"].clone())))
            .collect();
        let string = |key: &str| -> Result<String> {
            values
                .get(key)
                .and_then(serde_json::Value::as_str)
                .map(str::to_owned)
                .ok_or_else(|| Error::internal(format!("{key} is not a text value")))
        };
        let code_max_length = values
            .get(CODE_MAX_LENGTH)
            .and_then(serde_json::Value::as_u64)
            .and_then(|n| usize::try_from(n).ok())
            .ok_or_else(|| {
                Error::internal(format!("{CODE_MAX_LENGTH} is not a positive integer"))
            })?;
        let mut prefixes = BTreeMap::new();
        let mut account_types = BTreeMap::new();
        for journal_type in JOURNAL_TYPES {
            prefixes.insert(journal_type.to_owned(), string(&prefix_key(journal_type))?);
            let list = string(&account_types_key(journal_type))?;
            account_types.insert(journal_type.to_owned(), parse_account_types(&list)?);
        }
        Ok(Self {
            code_max_length,
            prefixes,
            account_types,
        })
    }

    /// The code prefix proposed for a new journal of `journal_type`.
    ///
    /// # Errors
    ///
    /// `Validation` for a type that is not a journal type.
    pub fn prefix(&self, journal_type: &str) -> Result<&str> {
        self.prefixes
            .get(journal_type)
            .map(String::as_str)
            .ok_or_else(|| unknown_type(journal_type))
    }

    /// Whether an account of `account_type` may be the default account of a `journal_type`
    /// journal: the type is one the journal type's setting allows. The refusal names the type and
    /// the setting to change; the caller refuses an account it may not read before asking.
    ///
    /// # Errors
    ///
    /// `Validation` naming the setting when the type is not allowed, or for an unknown journal type.
    pub fn check_default_account_type(&self, journal_type: &str, account_type: &str) -> Result<()> {
        let allowed = self
            .account_types
            .get(journal_type)
            .ok_or_else(|| unknown_type(journal_type))?;
        if allowed.iter().any(|t| t == account_type) {
            return Ok(());
        }
        Err(Error::validation(format!(
            "An account of type {account_type} cannot be the default account of a {journal_type} \
             journal: the allowed types are {} (setting {}).",
            allowed.join(", "),
            account_types_key(journal_type)
        )))
    }
}

fn unknown_type(journal_type: &str) -> Error {
    Error::validation(format!("{journal_type:?} is not a journal type"))
}
