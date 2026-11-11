//! `res.currency.rate`: dated exchange rates per currency and company (H41).
//!
//! Rates live on the root company of a branch hierarchy, or on no company at all (a rate shared
//! by every company).
//! The lookup that reads these rows is [`crate::money::RateBook`]; this file
//! only declares the table.

use erp_sdk::ValueMap;
use erp_sdk::prelude::*;

use crate::money::{company_chain, flush_named, rate_type_id_by_code};
use crate::res_currency_rate_type::SPOT_RATE_TYPE;

/// `res.currency.rate`.
#[model(
    name = "res.currency.rate",
    description = "Currency Rate",
    company_access = direct(field = "company_id", branch = "ancestor", empty = "allow"),
    order = "name desc, id",
    constraints = [(
        "_currency_rate_check",
        "CHECK (rate>0)",
        "The currency rate must be strictly positive."
    )],
    keys = [key(
        name = "_unique_name_per_day",
        fields = ["currency_id", "company_id", "rate_type_id", "name"],
        message = "Only one currency rate per day allowed!"
    )],
)]
pub struct ResCurrencyRate {
    #[field(date, required = true, index = true)]
    pub name: Option<Date>,
    /// Units of this currency per unit of the company's reference currency, exact and unrounded
    /// (`numeric`, no digits cap: `digits = 0`).
    #[field(float, digits = 0, required = true)]
    pub rate: Decimal,
    /// The currency the rate belongs to.
    #[field(
        many2one,
        comodel = "res.currency",
        required = true,
        ondelete = "cascade",
        index = true
    )]
    pub currency_id: Option<RecId>,
    /// Root company the rate applies to; empty for a rate shared by all companies.
    #[field(many2one, comodel = "res.company", ondelete = "cascade")]
    pub company_id: Option<RecId>,
    /// The series this rate belongs to (`res.currency.rate.type`); a rate created without a type
    /// is a `spot` rate. A type that still has rates cannot be deleted.
    #[field(
        many2one,
        comodel = "res.currency.rate.type",
        required = true,
        ondelete = "restrict",
        index = true
    )]
    pub rate_type_id: Option<RecId>,
}

#[methods(ResCurrencyRate, addon = "base")]
impl ResCurrencyRate {
    #[constrains("company_id")]
    async fn _check_company_id(
        this: &RecordSet<ResCurrencyRate>,
        tx: &mut Transaction,
    ) -> Result<()> {
        let mut ids: Vec<i64> = Vec::new();
        for value in this.as_records().values(tx, "company_id").await? {
            match value {
                Value::Int(id) => ids.push(id),
                Value::False | Value::Bool(false) => {}
                other => {
                    return Err(Error::invariant(format!(
                        "res.currency.rate.company_id read as {other:?}"
                    )));
                }
            }
        }
        if ids.is_empty() {
            return Ok(());
        }
        flush_named(tx, "res.company").await?;
        let rows = tx
            .query(
                "SELECT id FROM res_company WHERE id = ANY($1::int8[]) AND parent_id IS NOT NULL LIMIT 1",
                &[&ids],
            )
            .await?;
        if rows.is_empty() {
            Ok(())
        } else {
            Err(Error::validation(
                "Currency rates should only be created for main companies",
            ))
        }
    }

    /// A new rate is a
    /// `spot` rate unless it names a type; a database without the seeded `spot` type is an error,
    /// never a rate of no type.
    #[method(override)]
    async fn default_get(
        this: &RecordSet<ResCurrencyRate>,
        tx: &mut Transaction,
        names: Vec<String>,
        sup: Super,
    ) -> Result<ValueMap> {
        let wants_company = names.iter().any(|n| n == "company_id");
        let wants_type = names.iter().any(|n| n == "rate_type_id");
        let mut out = sup.call(tx, this.as_records(), (names,)).await?;
        if wants_company
            && !out.contains_key("company_id")
            && let Some(current) = this.env().company_id()?
        {
            let chain = company_chain(tx, current).await?;
            out.insert("company_id".to_owned(), Value::Int(chain.root_id()));
        }
        if wants_type && !out.contains_key("rate_type_id") {
            out.insert(
                "rate_type_id".to_owned(),
                Value::Int(rate_type_id_by_code(tx, SPOT_RATE_TYPE).await?),
            );
        }
        Ok(out)
    }
}
