// SPDX-License-Identifier: MIT OR Apache-2.0
//! `res.currency.rate.type`: the kind of an exchange-rate series (foundation L-3, L-4).
//!
//! A rate type names an independent dated series per currency and company (spot, tax, closing, ...)
//! and owns the date rule its lookup follows. The six seeded types are in
//! `data/res_currency_rate_type_data.xml`. The lookup that reads the rows is
//! [`crate::money::RateBook::load_typed`]; a type never falls back to another type.

use erp_sdk::Operation;
use erp_sdk::prelude::*;

/// The code of the type an untyped caller or an untyped new rate means.
pub const SPOT_RATE_TYPE: &str = "spot";

/// `res.currency.rate.type`.
#[model(
    name = "res.currency.rate.type",
    description = "Currency Rate Type",
    order = "sequence, id",
    keys = [key(
        name = "_unique_code",
        fields = ["code"],
        message = "The rate type code must be unique!"
    )],
)]
pub struct ResCurrencyRateType {
    /// Stable identity of the type (`spot`, `tax`, ...); what addons and APIs name it by.
    /// The default `spot` code cannot be changed or cleared.
    #[field(char, required = true)]
    pub code: Option<Arc<str>>,
    /// Label shown to users.
    #[field(char, required = true)]
    pub name: Option<Arc<str>>,
    #[field(
        selection,
        selection = [
            ("strictly_before", "Strictly Before the Date"),
            ("on_or_before", "On or Before the Date")
        ],
        default = "strictly_before",
        required = true
    )]
    pub date_rule: Option<Arc<str>>,
    /// Only `spot` allows it; any other type reports a missing rate.
    #[field(boolean, default = false)]
    pub later_fallback: bool,
    /// Display order.
    #[field(integer, default = 10)]
    pub sequence: i32,
    /// Archived types are hidden from pickers; their rates stay and lookups still read them.
    /// The default `spot` type cannot be archived or deleted.
    #[field(boolean, default = true)]
    pub active: bool,
}

#[methods(ResCurrencyRateType, addon = "base")]
impl ResCurrencyRateType {
    /// Keep the canonical default available without restricting labels or other rate types.
    #[method(override)]
    async fn write(
        this: &RecordSet<ResCurrencyRateType>,
        tx: &mut Transaction,
        vals: RawVals,
        sup: Super,
    ) -> Result<()> {
        let changes_code = vals.get("code").is_some_and(
            |value| !matches!(value, Cell::Str(code) if code.as_ref() == SPOT_RATE_TYPE),
        );
        let archives = matches!(vals.get("active"), Some(Cell::Bool(false) | Cell::Null));
        if !this.is_empty() && (changes_code || archives) {
            let records = this.as_records();
            records.check_access(tx, Operation::Write).await?;
            erp_sdk::__private::base_check_write_fields(tx, records, &vals)?;
            if contains_spot(records, tx).await? {
                if changes_code {
                    return Err(Error::user(
                        "The code of the default currency rate type 'spot' cannot be changed.",
                    ));
                }
                return Err(Error::user(
                    "The default currency rate type 'spot' cannot be archived.",
                ));
            }
        }
        sup.call(tx, this.as_records(), (vals,)).await
    }

    /// Refuse the whole deletion before any rows or pending values are removed.
    #[method(override)]
    async fn unlink(
        this: &RecordSet<ResCurrencyRateType>,
        tx: &mut Transaction,
        sup: Super,
    ) -> Result<()> {
        if !this.is_empty() {
            let records = this.as_records();
            records.check_access(tx, Operation::Unlink).await?;
            if contains_spot(records, tx).await? {
                return Err(Error::user(
                    "The default currency rate type 'spot' cannot be deleted.",
                ));
            }
        }
        sup.call(tx, this.as_records(), ()).await
    }
}

/// Read the entire batch before its caller delegates any mutation to the base implementation.
async fn contains_spot(records: &Records, tx: &mut Transaction) -> Result<bool> {
    for value in records.values(tx, "code").await? {
        match value {
            Value::Str(code) if code.as_ref() == SPOT_RATE_TYPE => return Ok(true),
            Value::Str(_) => {}
            other => {
                return Err(Error::invariant(format!(
                    "res.currency.rate.type.code read as {other:?}"
                )));
            }
        }
    }
    Ok(false)
}
