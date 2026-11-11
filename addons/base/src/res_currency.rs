//! `res.currency`: currency model.

use erp_sdk::prelude::*;

use crate::res_company::ResCompany;

/// `res.currency`.
#[model(
    name = "res.currency",
    description = "Currency",
    constraints = [
        ("_unique_name", "unique (name)", "The currency code must be unique!"),
        (
            "_rounding_gt_zero",
            "CHECK (rounding>0)",
            "The rounding factor must be greater than 0!"
        ),
    ],
)]
pub struct ResCurrency {
    /// Currency code (ISO 4217, 3 chars).
    #[field(char, size = 3, required = true)]
    pub name: Option<Arc<str>>,
    /// Currency symbol (e.g. $, €, £, Rp).
    #[field(char, required = true)]
    pub symbol: Option<Arc<str>>,
    /// Full currency name (e.g. US Dollar, Euro).
    #[field(char)]
    pub full_name: Option<Arc<str>>,
    /// Rounding factor (amounts are rounded to nearest multiple of rounding).
    #[field(float, digits = (12, 6), default = 0.01, required = true)]
    pub rounding: Decimal,
    /// Decimal places taken into account for operations on amounts.
    #[field(integer, compute = "_compute_decimal_places", store = true)]
    pub decimal_places: i32,
    /// Active flag.
    #[field(boolean, default = true)]
    pub active: bool,
    /// Position of currency symbol: 'after' or 'before'.
    #[field(
        selection,
        selection = [("after", "After Amount"), ("before", "Before Amount")],
        default = "after"
    )]
    pub position: Option<Arc<str>>,
    /// Currency numeric code (ISO 4217).
    #[field(integer)]
    pub iso_numeric: Option<i32>,
    /// Dated exchange rates of this currency (`res.currency.rate`).
    #[field(one2many, comodel = "res.currency.rate", inverse_name = "currency_id")]
    pub rate_ids: Vec<RecId>,
}

#[methods(ResCurrency, addon = "base")]
impl ResCurrency {
    /// Internal, positional adapter for core monetary publication.
    /// Currency reads use the method environment. Core supplies narrow currency-lookup
    /// sudo; amount-write authorization remains with the mutation caller.
    #[method]
    async fn _round_monetary(
        this: &RecordSet<ResCurrency>,
        tx: &mut Transaction,
        amounts: Vec<(RecId, Decimal)>,
    ) -> Result<Vec<Decimal>> {
        if amounts.is_empty() || amounts.len() > 256 {
            return Err(Error::programming(
                "_round_monetary expects 1 to 256 amounts",
            ));
        }
        let ids: Vec<RecId> = amounts.iter().map(|(id, _)| *id).collect();
        let table = crate::money::RoundingTable::load(this.env(), tx, &ids).await?;
        amounts
            .into_iter()
            .map(|(id, amount)| table.get(id)?.round(amount))
            .collect()
    }

    /// Compute decimal places from rounding factor.
    ///
    /// When `0 < rounding < 1`, decimal places = `ceil(log10(1 / rounding))`, else `0`.
    #[compute(depends = ["rounding"])]
    fn _compute_decimal_places(batch: &mut Batch<'_, ResCurrency>) -> Result<()> {
        for i in batch.indices() {
            let rounding = batch.get(i, ResCurrency::ROUNDING)?;
            // Exact decimal equivalent of ceil(log10(1 / rounding)): count the
            // mantissa's integral decimal digits, not binary floating-point logs.
            let places = if rounding > Decimal::ZERO && rounding < Decimal::ONE {
                let digits = rounding.mantissa().unsigned_abs().ilog10();
                i32::try_from(
                    rounding
                        .scale()
                        .checked_sub(digits)
                        .ok_or_else(|| Error::programming("invalid currency decimal scale"))?,
                )
                .map_err(|_| Error::programming("currency decimal places exceed i32"))?
            } else {
                0
            };
            batch.set(i, ResCurrency::DECIMAL_PLACES, places)?;
        }
        Ok(())
    }

    /// Company currency cannot be deactivated.
    #[constrains("active")]
    async fn _check_company_currency_stays_active(
        this: &RecordSet<ResCurrency>,
        tx: &mut Transaction,
    ) -> Result<()> {
        if tx.base_installing() {
            return Ok(());
        }
        let records = this.as_records();
        let actives = records.values(tx, "active").await?;
        let mut inactive_ids: Vec<i64> = Vec::new();
        for (i, act) in actives.iter().enumerate() {
            if !act.is_truthy() {
                inactive_ids.push(records.rec_ids()[i].get());
            }
        }
        if inactive_ids.is_empty() {
            return Ok(());
        }
        // Publish the archive before reading company dependencies. A concurrent
        // assignment touches this currency row and makes a stale snapshot retry.
        let active = records
            .spec()
            .field("active")
            .ok_or_else(|| Error::programming("res.currency.active is missing from the registry"))?
            .id;
        tx.flush_fields(records.model_id(), &[active]).await?;
        let companies = this.env().model::<ResCompany>()?;
        let domain = Domain::condition("currency_id", Operator::In, inactive_ids)?;
        let count = companies.search_count(tx, &domain, Some(1)).await?;
        if count > 0 {
            return Err(Error::user(
                "This currency is set on a company and therefore cannot be deactivated.",
            ));
        }
        Ok(())
    }
}
